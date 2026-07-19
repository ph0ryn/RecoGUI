"""Fixed-model lifecycle for the RecoGUI engine."""

from __future__ import annotations

import hashlib
import json
import shutil
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from threading import Event, Lock
from typing import Any, cast
from urllib.parse import quote

from reco.config import DEFAULT_CLI_CONFIG
from reco.errors import RecoError
from reco.vad import SILERO_VAD_VERSION, ensure_silero_vad_asset

MODEL_ID = DEFAULT_CLI_CONFIG.default_model
MODEL_REVISION = DEFAULT_CLI_CONFIG.default_model_revision


class ModelState(StrEnum):
  """Managed model lifecycle state."""

  MISSING = "missing"
  DOWNLOADING = "downloading"
  READY = "ready"
  LOADING = "loading"
  LOADED = "loaded"
  INVALID = "invalid"


@dataclass(frozen=True)
class ModelSnapshot:
  """Serializable managed-model state."""

  state: ModelState
  model_id: str
  revision: str
  path: str | None
  bytes_on_disk: int


class ModelManager:
  """Download, verify, and atomically publish one pinned model."""

  def __init__(
    self,
    models_directory: Path,
    *,
    download_func: Callable[..., str] | None = None,
    vad_download_func: Callable[[Path], Path] = ensure_silero_vad_asset,
  ) -> None:
    self.models_directory = models_directory
    self.model_directory = models_directory / "qwen3-asr-ja-mlx-8bit"
    self.temporary_directory = models_directory / ".qwen3-asr-ja-mlx-8bit.download"
    self.manifest_path = self.model_directory / "reco-model-manifest.json"
    self.vad_asset_path = models_directory / f"silero-vad-{SILERO_VAD_VERSION}.onnx"
    self._download_func = download_func
    self._vad_download_func = vad_download_func
    self._cancel = Event()
    self._lock = Lock()
    self._busy = False
    self._verified: bool | None = None
    models_directory.mkdir(parents=True, exist_ok=True)

  def snapshot(self, *, loaded: bool = False) -> ModelSnapshot:
    """Return current state without contacting the network."""

    if self._busy:
      state = ModelState.DOWNLOADING
    elif not self.model_directory.is_dir():
      state = ModelState.MISSING
    elif self._verified is False or not self._manifest_metadata_valid():
      state = ModelState.INVALID
    else:
      state = ModelState.LOADED if loaded else ModelState.READY
    return ModelSnapshot(
      state=state,
      model_id=MODEL_ID,
      revision=MODEL_REVISION,
      path=str(self.model_directory) if self.model_directory.exists() else None,
      bytes_on_disk=_directory_size(self.model_directory),
    )

  def download(self, progress: Callable[[dict[str, object]], None] | None = None) -> Path:
    """Download the pinned snapshot and atomically promote it after verification."""

    with self._lock:
      if self._busy:
        raise RecoError("Model download is already in progress")
      self._busy = True
    self._cancel.clear()
    try:
      if self.model_directory.is_dir() and self.verify():
        self._vad_download_func(self.vad_asset_path)
        return self.model_directory
      free_bytes = shutil.disk_usage(self.models_directory).free
      if free_bytes < 4 * 1024**3:
        raise RecoError("At least 4 GiB of free disk space is required to download the model")
      self.temporary_directory.mkdir(parents=True, exist_ok=True)
      if progress is not None:
        progress({"phase": "downloading", "bytesDownloaded": 0, "bytesTotal": None})
      downloader = self._download_func or _download_model_files
      downloader(
        repo_id=MODEL_ID,
        revision=MODEL_REVISION,
        local_dir=str(self.temporary_directory),
        cancel_event=self._cancel,
        progress=progress,
      )
      if self._cancel.is_set():
        raise RecoError("Model download was cancelled")
      manifest = _build_manifest(self.temporary_directory)
      (self.temporary_directory / "reco-model-manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
      )
      self._vad_download_func(self.vad_asset_path)
      if self._cancel.is_set():
        raise RecoError("Model download was cancelled")
      if self.model_directory.exists():
        shutil.rmtree(self.model_directory)
      self.temporary_directory.replace(self.model_directory)
      if not self.verify(force=True):
        raise RecoError("Downloaded model failed manifest verification")
      if progress is not None:
        size = _directory_size(self.model_directory)
        progress({"phase": "ready", "bytesDownloaded": size, "bytesTotal": size})
      return self.model_directory
    finally:
      with self._lock:
        self._busy = False

  def cancel_download(self) -> None:
    """Request cancellation at the next safe download boundary."""

    self._cancel.set()

  def verify(self, *, force: bool = True) -> bool:
    """Verify the revision and every file recorded in the local manifest."""

    if self._verified is True and not force:
      return True
    try:
      manifest = json.loads(self.manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
      self._verified = False
      return False
    if manifest.get("modelId") != MODEL_ID or manifest.get("revision") != MODEL_REVISION:
      self._verified = False
      return False
    files = manifest.get("files")
    if not isinstance(files, dict) or not files:
      self._verified = False
      return False
    for relative_name, expected_hash in files.items():
      if not isinstance(relative_name, str) or not isinstance(expected_hash, str):
        self._verified = False
        return False
      path = self.model_directory / relative_name
      if not path.is_file() or _sha256(path) != expected_hash:
        self._verified = False
        return False
    self._verified = True
    return True

  def ensure_verified(self) -> bool:
    """Perform one full verification per process and reuse that result for sessions."""

    return self.verify(force=False)

  def delete(self, *, engine_idle: bool) -> bool:
    """Delete the managed model only while no session can use it."""

    if not engine_idle or self._busy:
      raise RecoError("The model can only be deleted while the engine is idle")
    if not self.model_directory.exists():
      return False
    shutil.rmtree(self.model_directory)
    self._verified = None
    return True

  def _manifest_metadata_valid(self) -> bool:
    try:
      manifest = json.loads(self.manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
      return False
    return manifest.get("modelId") == MODEL_ID and manifest.get("revision") == MODEL_REVISION


def _download_model_files(**options: Any) -> str:
  """Download one file at a time with resumable ranges and chunk cancellation."""

  try:
    from huggingface_hub import HfApi
  except ImportError as exc:
    raise RecoError("huggingface-hub is required to download the managed model") from exc
  repo_id = str(options["repo_id"])
  revision = str(options["revision"])
  destination = Path(str(options["local_dir"]))
  cancel_event = options["cancel_event"]
  progress = options.get("progress")
  if not isinstance(cancel_event, Event):
    raise TypeError("cancel_event must be a threading.Event")
  info = HfApi().model_info(repo_id, revision=revision, files_metadata=True)
  siblings = sorted(cast(list[Any], info.siblings), key=lambda item: str(item.rfilename))
  total = sum(int(item.size or 0) for item in siblings) or None
  downloaded = sum(path.stat().st_size for path in destination.rglob("*.part"))
  for sibling in siblings:
    if cancel_event.is_set():
      raise RecoError("Model download was cancelled")
    relative_name = sibling.rfilename
    target = destination / relative_name
    expected_size = int(sibling.size or 0) or None
    if target.is_file() and (expected_size is None or target.stat().st_size == expected_size):
      downloaded += target.stat().st_size
      continue
    downloaded = _download_resumable_file(
      repo_id,
      revision,
      relative_name,
      target,
      expected_size,
      cancel_event,
      downloaded,
      total,
      progress if callable(progress) else None,
    )
  return str(destination)


def _download_resumable_file(
  repo_id: str,
  revision: str,
  relative_name: str,
  target: Path,
  expected_size: int | None,
  cancel_event: Event,
  downloaded_before: int,
  total: int | None,
  progress: Callable[[dict[str, object]], None] | None,
) -> int:
  target.parent.mkdir(parents=True, exist_ok=True)
  partial = target.with_suffix(target.suffix + ".part")
  offset = partial.stat().st_size if partial.exists() else 0
  url = f"https://huggingface.co/{repo_id}/resolve/{revision}/{quote(relative_name)}"
  headers = {"Range": f"bytes={offset}-"} if offset else {}
  request = urllib.request.Request(url, headers=headers)
  try:
    response = urllib.request.urlopen(request, timeout=60)
  except (OSError, urllib.error.URLError) as exc:
    raise RecoError(f"Could not download model file {relative_name}: {exc}") from exc
  append = offset > 0 and getattr(response, "status", None) == 206
  if offset and not append:
    offset = 0
  mode = "ab" if append else "wb"
  with response, partial.open(mode) as destination:
    while block := response.read(1024 * 1024):
      if cancel_event.is_set():
        raise RecoError("Model download was cancelled")
      destination.write(block)
      offset += len(block)
      if progress is not None:
        progress(
          {
            "phase": "downloading",
            "file": relative_name,
            "bytesDownloaded": downloaded_before + offset,
            "bytesTotal": total,
          }
        )
  if expected_size is not None and offset != expected_size:
    raise RecoError(f"Model file {relative_name} has size {offset}; expected {expected_size}")
  partial.replace(target)
  return downloaded_before + offset


def _build_manifest(directory: Path) -> dict[str, object]:
  files = {
    str(path.relative_to(directory)): _sha256(path)
    for path in sorted(directory.rglob("*"))
    if path.is_file() and path.name != "reco-model-manifest.json" and ".cache" not in path.parts
  }
  if not files:
    raise RecoError("Downloaded model snapshot is empty")
  return {"modelId": MODEL_ID, "revision": MODEL_REVISION, "files": files}


def _sha256(path: Path) -> str:
  digest = hashlib.sha256()
  with path.open("rb") as source:
    while block := source.read(1024 * 1024):
      digest.update(block)
  return digest.hexdigest()


def _directory_size(path: Path) -> int:
  if not path.exists():
    return 0
  return sum(item.stat().st_size for item in path.rglob("*") if item.is_file())

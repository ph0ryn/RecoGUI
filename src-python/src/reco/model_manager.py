"""Read-only discovery of models managed by the Hugging Face CLI."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from threading import Lock
from typing import Any

from reco.vad import SILERO_VAD_VERSION


class ModelState(StrEnum):
  CLI_MISSING = "cliMissing"
  UNSELECTED = "unselected"
  UNAVAILABLE = "unavailable"
  LOADING = "loading"
  READY = "ready"
  ERROR = "error"


@dataclass(frozen=True)
class ModelReference:
  repo_id: str
  revision: str


@dataclass(frozen=True)
class CachedModel:
  repo_id: str
  revision: str
  snapshot_path: Path
  size: str
  last_modified: str
  refs: tuple[str, ...]

  def public(self) -> dict[str, object]:
    return {
      "repoId": self.repo_id,
      "revision": self.revision,
      "size": self.size,
      "lastModified": self.last_modified,
      "refs": list(self.refs),
    }


@dataclass(frozen=True)
class ModelSnapshot:
  state: ModelState
  selected: ModelReference | None
  error: str | None = None

  def public(self) -> dict[str, object]:
    selected = None
    if self.selected is not None:
      selected = {"repoId": self.selected.repo_id, "revision": self.selected.revision}
    result: dict[str, object] = {"status": self.state.value, "selected": selected}
    if self.error is not None:
      result.update({"errorCode": "model_error", "errorMessage": self.error})
    return result


CliRunner = Callable[[list[str]], subprocess.CompletedProcess[str]]


class ModelManager:
  """Resolve cached model snapshots without modifying the Hugging Face cache."""

  def __init__(
    self,
    assets_directory: Path,
    *,
    selected: ModelReference | None = None,
    cli_runner: CliRunner | None = None,
    executable_resolver: Callable[[], Path | None] | None = None,
  ) -> None:
    self.assets_directory = assets_directory
    self.vad_asset_path = assets_directory / f"silero-vad-{SILERO_VAD_VERSION}.onnx"
    self.selected = selected
    self._models: dict[tuple[str, str], CachedModel] = {}
    self._state = ModelState.UNSELECTED if selected is None else ModelState.UNAVAILABLE
    self._error: str | None = None
    self._cli_runner = cli_runner or _run_cli
    self._executable_resolver = executable_resolver or _resolve_hf_executable
    self._lock = Lock()
    assets_directory.mkdir(parents=True, exist_ok=True)

  def snapshot(self) -> ModelSnapshot:
    with self._lock:
      return ModelSnapshot(self._state, self.selected, self._error)

  def list_models(self) -> list[dict[str, object]]:
    executable = self._executable_resolver()
    if executable is None:
      with self._lock:
        self._models.clear()
        self._state = ModelState.CLI_MISSING
        self._error = "The Hugging Face CLI was not found"
      return []
    try:
      result = self._cli_runner([str(executable), "cache", "ls", "--revisions", "--format", "json"])
      if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "hf cache ls failed")
      raw = json.loads(result.stdout)
      models = _parse_models(raw)
    except (OSError, RuntimeError, json.JSONDecodeError, ValueError, TypeError) as exc:
      with self._lock:
        self._models.clear()
        self._state = ModelState.ERROR
        self._error = str(exc)
      return []
    with self._lock:
      self._models = {(model.repo_id, model.revision): model for model in models}
      selected = self.selected
      if selected is None:
        self._state = ModelState.UNSELECTED
      elif (
        (selected.repo_id, selected.revision) not in self._models
        or not self._models[(selected.repo_id, selected.revision)].snapshot_path.is_dir()
        or self._state not in {ModelState.LOADING, ModelState.READY}
      ):
        self._state = ModelState.UNAVAILABLE
      self._error = None
    return [model.public() for model in models]

  def resolve(self, reference: ModelReference) -> Path | None:
    with self._lock:
      model = self._models.get((reference.repo_id, reference.revision))
    if model is None or not model.snapshot_path.is_dir():
      return None
    return model.snapshot_path

  def begin_loading(self, reference: ModelReference) -> Path:
    with self._lock:
      self.selected = reference
    path = self.resolve(reference)
    if path is None:
      with self._lock:
        self._state = ModelState.UNAVAILABLE
      raise ValueError("The selected model revision is not available in the Hugging Face cache")
    with self._lock:
      self._state = ModelState.LOADING
      self._error = None
    return path

  def mark_ready(self) -> None:
    with self._lock:
      self._state = ModelState.READY
      self._error = None

  def mark_error(self, message: str) -> None:
    with self._lock:
      self._state = ModelState.ERROR
      self._error = message


def _resolve_hf_executable() -> Path | None:
  discovered = shutil.which("hf")
  candidates = [Path(discovered)] if discovered else []
  candidates.append(Path.home() / ".local" / "bin" / "hf")
  return next((path for path in candidates if path.is_file() and os.access(path, os.X_OK)), None)


def _run_cli(argv: list[str]) -> subprocess.CompletedProcess[str]:
  return subprocess.run(argv, check=False, capture_output=True, text=True)


def _parse_models(value: Any) -> list[CachedModel]:
  if not isinstance(value, list):
    raise ValueError("hf cache ls returned a non-array JSON value")
  models: list[CachedModel] = []
  for entry in value:
    if not isinstance(entry, Mapping) or entry.get("repo_type") != "model":
      continue
    repo_id = entry.get("repo_id")
    revision = entry.get("revision")
    snapshot_path = entry.get("snapshot_path")
    if not all(isinstance(item, str) and item for item in (repo_id, revision, snapshot_path)):
      raise ValueError("hf cache ls returned invalid model metadata")
    refs_value = entry.get("refs", [])
    if not isinstance(refs_value, list) or not all(isinstance(item, str) for item in refs_value):
      raise ValueError("hf cache ls returned invalid model refs")
    models.append(
      CachedModel(
        repo_id=repo_id,
        revision=revision,
        snapshot_path=Path(snapshot_path),
        size=str(entry.get("size", "")),
        last_modified=str(entry.get("last_modified", "")),
        refs=tuple(refs_value),
      )
    )
  return models

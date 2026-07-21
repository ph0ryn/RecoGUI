"""Read-only discovery of models in the shared Hugging Face cache."""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from threading import Lock
from typing import Any

from huggingface_hub import scan_cache_dir


class ModelState(StrEnum):
  UNSELECTED = "unselected"
  UNAVAILABLE = "unavailable"
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
  supported_languages: tuple[str, ...]

  def public(self) -> dict[str, object]:
    return {
      "repoId": self.repo_id,
      "revision": self.revision,
      "size": self.size,
      "lastModified": self.last_modified,
      "refs": list(self.refs),
      "supportedLanguages": list(self.supported_languages),
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


CacheScanner = Callable[[], Any]


class ModelManager:
  """Resolve cached model snapshots without modifying the Hugging Face cache."""

  def __init__(
    self,
    *,
    selected: ModelReference | None = None,
    cache_scanner: CacheScanner | None = None,
  ) -> None:
    self.selected = selected
    self._models: dict[tuple[str, str], CachedModel] = {}
    self._state = ModelState.UNSELECTED if selected is None else ModelState.UNAVAILABLE
    self._error: str | None = None
    self._cache_scanner = cache_scanner or scan_cache_dir
    self._lock = Lock()

  def snapshot(self) -> ModelSnapshot:
    with self._lock:
      return ModelSnapshot(self._state, self.selected, self._error)

  def list_models(self) -> list[dict[str, object]]:
    try:
      models = _parse_models(self._cache_scanner())
    except (OSError, RuntimeError, ValueError, TypeError) as exc:
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
      elif (selected.repo_id, selected.revision) not in self._models or not self._models[
        (selected.repo_id, selected.revision)
      ].snapshot_path.is_dir():
        self._state = ModelState.UNAVAILABLE
      else:
        self._state = ModelState.READY
      self._error = None
    return [model.public() for model in models]

  def resolve(self, reference: ModelReference) -> Path | None:
    with self._lock:
      model = self._models.get((reference.repo_id, reference.revision))
    if model is None or not model.snapshot_path.is_dir():
      return None
    return model.snapshot_path

  def supported_languages(self, reference: ModelReference) -> tuple[str, ...]:
    """Return canonical language names declared by one cached model revision."""

    with self._lock:
      model = self._models.get((reference.repo_id, reference.revision))
    if model is None:
      return ()
    return model.supported_languages

  def select(self, reference: ModelReference) -> None:
    """Select one cached revision without loading its ASR runtime."""

    with self._lock:
      model = self._models.get((reference.repo_id, reference.revision))
      if model is None or not model.snapshot_path.is_dir():
        raise ValueError("The selected model revision is not available in the Hugging Face cache")
      self.selected = reference
      self._state = ModelState.READY
      self._error = None

  def restore(self, snapshot: ModelSnapshot) -> None:
    """Restore an in-memory selection after its durable write fails."""

    with self._lock:
      self.selected = snapshot.selected
      self._state = snapshot.state
      self._error = snapshot.error


def _parse_models(value: Any) -> list[CachedModel]:
  repos = getattr(value, "repos", None)
  if repos is None:
    raise ValueError("Hugging Face cache scan returned invalid metadata")
  models: list[CachedModel] = []
  for repo in repos:
    if getattr(repo, "repo_type", None) != "model":
      continue
    for revision in repo.revisions:
      models.append(
        CachedModel(
          repo_id=repo.repo_id,
          revision=revision.commit_hash,
          snapshot_path=revision.snapshot_path,
          size=_format_size(revision.size_on_disk),
          last_modified=datetime.fromtimestamp(revision.last_modified, UTC).isoformat(),
          refs=tuple(sorted(revision.refs)),
          supported_languages=_read_supported_languages(revision.snapshot_path),
        )
      )
  return sorted(models, key=lambda model: (model.repo_id, model.revision))


def _read_supported_languages(snapshot_path: Path) -> tuple[str, ...]:
  try:
    value = json.loads((snapshot_path / "config.json").read_text())
  except (OSError, json.JSONDecodeError, TypeError):
    return ()
  languages = value.get("support_languages") if isinstance(value, dict) else None
  if not isinstance(languages, list):
    return ()
  return tuple(
    dict.fromkeys(language.strip() for language in languages if isinstance(language, str) and language.strip())
  )


def _format_size(size: int) -> str:
  value = float(size)
  for unit in ("B", "KB", "MB", "GB", "TB"):
    if value < 1000 or unit == "TB":
      return f"{value:.1f}{unit}" if unit != "B" else f"{size}B"
    value /= 1000
  raise AssertionError("unreachable")

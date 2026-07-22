"""Read-only discovery of ASR revisions in the shared Hugging Face cache."""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from threading import Lock
from typing import Any

from huggingface_hub import scan_cache_dir


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


CacheScanner = Callable[[], Any]


class ModelCatalog:
  """Refresh and resolve cached revisions without selecting or downloading models."""

  def __init__(self, *, cache_scanner: CacheScanner | None = None) -> None:
    self._models: dict[tuple[str, str], CachedModel] = {}
    self._error: str | None = None
    self._cache_scanner = cache_scanner or scan_cache_dir
    self._lock = Lock()

  @property
  def error(self) -> str | None:
    with self._lock:
      return self._error

  def refresh(self) -> list[dict[str, object]]:
    """Refresh the immutable revision catalog and return public metadata."""

    try:
      models = _parse_models(self._cache_scanner())
    except (OSError, RuntimeError, ValueError, TypeError) as exc:
      with self._lock:
        self._models.clear()
        self._error = str(exc)
      return []
    with self._lock:
      self._models = {(model.repo_id, model.revision): model for model in models}
      self._error = None
    return [model.public() for model in models]

  def resolve(self, reference: ModelReference) -> Path | None:
    """Resolve one exact cached revision to a private local snapshot path."""

    with self._lock:
      model = self._models.get((reference.repo_id, reference.revision))
    if model is None or not model.snapshot_path.is_dir():
      return None
    return model.snapshot_path


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

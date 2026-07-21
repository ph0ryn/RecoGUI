from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from reco.model_manager import ModelManager, ModelReference, ModelState


def cache_info(*entries: tuple[str, str, Path], repo_type: str = "model") -> SimpleNamespace:
  revisions = tuple(
    SimpleNamespace(
      commit_hash=revision,
      snapshot_path=snapshot,
      size_on_disk=2_500_000_000,
      last_modified=1_700_000_000,
      refs=frozenset({"main"}),
    )
    for _, revision, snapshot in entries
  )
  repos = (SimpleNamespace(repo_id=entries[0][0], repo_type=repo_type, revisions=revisions),) if entries else ()
  return SimpleNamespace(repos=repos)


def test_lists_every_cached_model_revision_without_exposing_paths(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  manager = ModelManager(
    tmp_path,
    cache_scanner=lambda: cache_info(("owner/non-mlx-model", "commit", snapshot)),
  )

  assert manager.list_models() == [
    {
      "repoId": "owner/non-mlx-model",
      "revision": "commit",
      "size": "2.5GB",
      "lastModified": "2023-11-14T22:13:20+00:00",
      "refs": ["main"],
    }
  ]


def test_selected_snapshot_is_resolved_privately(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  reference = ModelReference("owner/model", "commit")
  manager = ModelManager(
    tmp_path,
    selected=reference,
    cache_scanner=lambda: cache_info(("owner/model", "commit", snapshot)),
  )

  manager.list_models()

  assert manager.resolve(reference) == snapshot
  assert manager.snapshot().state is ModelState.READY
  assert "snapshot" not in str(manager.snapshot().public())


def test_select_updates_availability_without_loading_a_runtime(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  manager = ModelManager(
    tmp_path,
    cache_scanner=lambda: cache_info(("owner/model", "commit", snapshot)),
  )
  reference = ModelReference("owner/model", "commit")

  manager.list_models()
  manager.select(reference)

  assert manager.snapshot().selected == reference
  assert manager.snapshot().state is ModelState.READY
  assert "loading" not in {state.value for state in ModelState}


def test_missing_selection_does_not_replace_the_current_model(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  current = ModelReference("owner/model", "commit")
  manager = ModelManager(
    tmp_path,
    selected=current,
    cache_scanner=lambda: cache_info((current.repo_id, current.revision, snapshot)),
  )
  manager.list_models()

  with pytest.raises(ValueError, match="not available"):
    manager.select(ModelReference("owner/missing", "missing"))

  assert manager.snapshot().selected == current
  assert manager.snapshot().state is ModelState.READY


def test_cache_scan_failures_are_error_state(tmp_path: Path) -> None:
  def fail() -> SimpleNamespace:
    raise RuntimeError("broken")

  manager = ModelManager(
    tmp_path,
    cache_scanner=lambda: SimpleNamespace(),
  )
  manager.list_models()
  assert manager.snapshot().state is ModelState.ERROR

  manager = ModelManager(
    tmp_path,
    cache_scanner=fail,
  )
  manager.list_models()
  assert manager.snapshot().state is ModelState.ERROR

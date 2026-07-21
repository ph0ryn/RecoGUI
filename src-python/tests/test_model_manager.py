from __future__ import annotations

import json
import subprocess
from pathlib import Path

from reco.model_manager import ModelManager, ModelReference, ModelState


def result(stdout: str = "[]", *, returncode: int = 0, stderr: str = "") -> subprocess.CompletedProcess[str]:
  return subprocess.CompletedProcess([], returncode, stdout, stderr)


def test_missing_cli_is_a_typed_state(tmp_path: Path) -> None:
  manager = ModelManager(tmp_path, executable_resolver=lambda: None)

  assert manager.list_models() == []
  assert manager.snapshot().state is ModelState.CLI_MISSING


def test_lists_every_cached_model_revision_without_exposing_paths(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  payload = [
    {
      "repo_type": "model",
      "repo_id": "owner/non-mlx-model",
      "revision": "commit",
      "snapshot_path": str(snapshot),
      "size": "2.5G",
      "last_modified": "2 months ago",
      "refs": ["main"],
    },
    {
      "repo_type": "dataset",
      "repo_id": "owner/data",
      "revision": "data-commit",
      "snapshot_path": str(snapshot),
    },
  ]
  calls: list[list[str]] = []

  def run(argv: list[str]) -> subprocess.CompletedProcess[str]:
    calls.append(argv)
    return result(json.dumps(payload))

  manager = ModelManager(tmp_path, cli_runner=run, executable_resolver=lambda: Path("/bin/hf"))

  assert manager.list_models() == [
    {
      "repoId": "owner/non-mlx-model",
      "revision": "commit",
      "size": "2.5G",
      "lastModified": "2 months ago",
      "refs": ["main"],
    }
  ]
  assert calls == [["/bin/hf", "cache", "ls", "--revisions", "--format", "json"]]


def test_selected_snapshot_is_resolved_privately(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  payload = [
    {
      "repo_type": "model",
      "repo_id": "owner/model",
      "revision": "commit",
      "snapshot_path": str(snapshot),
    }
  ]
  reference = ModelReference("owner/model", "commit")
  manager = ModelManager(
    tmp_path,
    selected=reference,
    cli_runner=lambda argv: result(json.dumps(payload)),
    executable_resolver=lambda: Path("/bin/hf"),
  )

  manager.list_models()

  assert manager.resolve(reference) == snapshot
  assert "snapshot" not in str(manager.snapshot().public())


def test_cli_and_json_failures_are_error_state(tmp_path: Path) -> None:
  manager = ModelManager(
    tmp_path,
    cli_runner=lambda argv: result("not-json"),
    executable_resolver=lambda: Path("/bin/hf"),
  )
  manager.list_models()
  assert manager.snapshot().state is ModelState.ERROR

  manager = ModelManager(
    tmp_path,
    cli_runner=lambda argv: result(returncode=1, stderr="broken"),
    executable_resolver=lambda: Path("/bin/hf"),
  )
  manager.list_models()
  assert manager.snapshot().state is ModelState.ERROR

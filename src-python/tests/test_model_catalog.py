from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

from reco_worker.model_catalog import ModelCatalog, ModelReference


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


def test_refresh_lists_every_revision_without_exposing_snapshot_paths(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  catalog = ModelCatalog(cache_scanner=lambda: cache_info(("owner/model", "commit", snapshot)))

  models = catalog.refresh()

  assert models == [
    {
      "repoId": "owner/model",
      "revision": "commit",
      "size": "2.5GB",
      "lastModified": "2023-11-14T22:13:20+00:00",
      "refs": ["main"],
      "supportedLanguages": [],
    }
  ]
  assert "snapshot" not in str(models).lower()
  assert catalog.resolve(ModelReference("owner/model", "commit")) == snapshot


def test_refresh_reads_unique_supported_languages(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  (snapshot / "config.json").write_text('{"support_languages":["Japanese","English","Japanese"]}')
  catalog = ModelCatalog(cache_scanner=lambda: cache_info(("owner/model", "commit", snapshot)))

  assert catalog.refresh()[0]["supportedLanguages"] == ["Japanese", "English"]


def test_refresh_failure_clears_stale_paths_and_exposes_an_error(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  scans: list[object] = [cache_info(("owner/model", "commit", snapshot)), SimpleNamespace()]
  catalog = ModelCatalog(cache_scanner=lambda: scans.pop(0))
  reference = ModelReference("owner/model", "commit")

  catalog.refresh()
  assert catalog.resolve(reference) == snapshot

  assert catalog.refresh() == []
  assert catalog.error == "Hugging Face cache scan returned invalid metadata"
  assert catalog.resolve(reference) is None

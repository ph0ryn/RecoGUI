from __future__ import annotations

from types import SimpleNamespace

import pytest


@pytest.fixture(autouse=True)
def isolate_hugging_face_cache(monkeypatch: pytest.MonkeyPatch) -> None:
  """Keep tests independent from the machine's shared Hugging Face cache."""

  monkeypatch.setattr("reco_worker.model_catalog.scan_cache_dir", lambda: SimpleNamespace(repos=()))

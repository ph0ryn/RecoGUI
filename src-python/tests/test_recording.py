from __future__ import annotations

from hashlib import sha256
from pathlib import Path

from reco.audio import AudioFileIdentity
from reco.recording import fingerprint_file_snapshot


def test_file_fingerprint_is_content_based_and_does_not_expose_the_path(tmp_path: Path) -> None:
  path = tmp_path / "private lecture name.wav"
  contents = b"deterministic audio fixture"
  path.write_bytes(contents)

  snapshot = fingerprint_file_snapshot(path)

  assert snapshot.value == f"sha256:{sha256(contents).hexdigest()}"
  assert str(path) not in snapshot.value
  assert snapshot.identity == AudioFileIdentity.from_stat(path.stat())

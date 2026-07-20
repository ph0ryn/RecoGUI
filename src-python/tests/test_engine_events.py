from __future__ import annotations

from collections.abc import Mapping
from pathlib import Path
from threading import Lock

import pytest

from reco.engine import RecoEngine, SessionControl
from reco.models import SplitReason, TranscriptionDiagnostics, TranscriptSegment, VadDiagnostics
from reco.repository import NewSession, RecordingRepository, RepositoryError, SessionState


def segment() -> TranscriptSegment:
  return TranscriptSegment(
    index=0,
    start_sample=0,
    end_sample=16_000,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text="hello",
    raw_text="hello",
    vad=VadDiagnostics(0.8, 0.9, 0.75),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=4),
  )


def engine_with_repository(
  repository: RecordingRepository,
) -> tuple[RecoEngine, list[tuple[str, str | None, Mapping[str, object]]]]:
  events: list[tuple[str, str | None, Mapping[str, object]]] = []
  engine = RecoEngine.__new__(RecoEngine)
  engine.repository = repository
  engine._event_callback = lambda event, session_id, payload: events.append((event, session_id, payload))
  return engine, events


def test_persisted_segment_event_contains_committed_receipt(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(NewSession("file", "audio.wav", "model", None, "Japanese", 16_000, "Audio"))
  repository.set_state(session_id, SessionState.RUNNING)
  engine, events = engine_with_repository(repository)

  engine._persist_segment(session_id, segment())

  assert len(events) == 1
  event, emitted_session_id, payload = events[0]
  assert event == "segment.persisted"
  assert emitted_session_id == session_id
  assert payload == {
    "segment": {"segmentIndex": 0, "startSample": 0, "endSample": 16_000, "text": "hello"},
    "rowVersion": 3,
    "totalSegments": 1,
    "recognizedSegments": 1,
    "characters": 5,
    "mediaDurationMs": 1000,
  }
  assert repository.get_session(session_id)["row_version"] == payload["rowVersion"]


def test_persisted_segment_event_is_not_emitted_when_commit_fails(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(NewSession("file", "audio.wav", "model", None, "Japanese", 16_000, "Audio"))
  repository.set_state(session_id, SessionState.STOPPED, end_reason="userStop")
  engine, events = engine_with_repository(repository)

  with pytest.raises(RepositoryError, match="not writable"):
    engine._persist_segment(session_id, segment())

  assert events == []


def test_state_event_contains_the_committed_revision_and_aggregates(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(NewSession("file", "audio.wav", "model", None, "Japanese", 16_000, "Audio"))
  engine, events = engine_with_repository(repository)
  engine._lock = Lock()
  engine._active_session_id = session_id
  engine._active_control = SessionControl()

  result = engine.stop_session(session_id)

  assert result == {
    "sessionId": session_id,
    "state": "stopping",
    "rowVersion": 2,
    "totalSegments": 0,
    "recognizedSegments": 0,
    "characters": 0,
    "mediaDurationMs": 0,
    "endedAt": None,
  }
  assert events == [
    (
      "session.stateChanged",
      session_id,
      {
        "state": "stopping",
        "rowVersion": 2,
        "totalSegments": 0,
        "recognizedSegments": 0,
        "characters": 0,
        "mediaDurationMs": 0,
        "endedAt": None,
        "reason": "userStop",
      },
    )
  ]

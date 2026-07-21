from __future__ import annotations

from collections.abc import Mapping
from pathlib import Path
from threading import Barrier, Lock, Thread
from types import SimpleNamespace
from typing import cast

import pytest

import reco.engine as engine_module
from reco.engine import EngineCommandError, ModelRuntime, RecoEngine, SessionControl
from reco.model_manager import ModelReference, ModelState
from reco.models import SplitReason, TranscriptionDiagnostics, TranscriptSegment, VadDiagnostics
from reco.pipeline import TranscriptionProgress
from reco.recording import fingerprint_file_snapshot
from reco.repository import NewQueueItem, NewSession, RecordingRepository, RepositoryError, SessionState


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


def test_list_audio_inputs_marks_the_default_input(monkeypatch: pytest.MonkeyPatch) -> None:
  devices = [
    {"name": "Output only", "max_input_channels": 0},
    {"name": "USB microphone", "max_input_channels": 1},
    {"name": "MacBook microphone", "max_input_channels": 2},
  ]
  monkeypatch.setattr(engine_module.sd, "query_devices", lambda: devices)
  monkeypatch.setattr(engine_module.sd, "default", SimpleNamespace(device=(2, 0)))

  engine = RecoEngine.__new__(RecoEngine)

  assert engine.list_audio_inputs() == [
    {"id": 1, "name": "USB microphone", "channels": 1, "isDefault": False},
    {"id": 2, "name": "MacBook microphone", "channels": 2, "isDefault": True},
  ]


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


def test_file_progress_event_includes_total_audio_duration(tmp_path: Path) -> None:
  engine, events = engine_with_repository(RecordingRepository(tmp_path / "reco.sqlite3"))
  progress = TranscriptionProgress("chunk", 2_500, 3, 2, 10, 1, 2)

  engine._publish_progress("session-1", progress, 10_000)

  assert events == [
    (
      "session.progress",
      "session-1",
      {
        "processedAudioMs": 2_500,
        "totalAudioMs": 10_000,
        "totalSegments": 3,
        "recognizedSegments": 2,
        "queueDepth": 1,
      },
    )
  ]


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


def test_pause_marks_the_active_session_as_pausing(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(
    NewSession("microphone", "Microphone", "model", None, "Japanese", 16_000, "Recording")
  )
  engine, events = engine_with_repository(repository)
  control = SessionControl()
  engine._lock = Lock()
  engine._active_session_id = session_id
  engine._active_control = control

  result = engine.pause_session(session_id)

  assert result["state"] == "pausing"
  assert control.stop_requested()
  assert control.stop_reason == "userPause"
  assert repository.get_session(session_id)["state"] == "pausing"
  assert events[-1][0] == "session.stateChanged"


def test_resume_is_rejected_while_another_session_is_active(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(
    NewSession("microphone", "Microphone", "model", None, "Japanese", 16_000, "Recording")
  )
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 16_000)
  engine, _ = engine_with_repository(repository)
  engine._lock = Lock()
  engine._shutting_down = False
  engine._active_session_id = "another-session"

  with pytest.raises(EngineCommandError, match="already active"):
    engine.resume_session(session_id)


def test_queue_start_is_rejected_while_a_session_is_active(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  engine.model_manager.selected = ModelReference("owner/model", "revision")
  engine.model_manager._state = ModelState.READY
  engine._active_session_id = "active-session"

  with pytest.raises(EngineCommandError, match="already active"):
    engine.start_queue()

  assert engine.queue_state()["autoAdvanceEnabled"] is False


def test_enqueue_starts_first_file_immediately_when_engine_and_queue_are_idle(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  first = tmp_path / "first.wav"
  second = tmp_path / "second.wav"
  first.touch()
  second.touch()

  class DormantThread:
    def __init__(self, **options: object) -> None:
      del options

    def start(self) -> None:
      return None

  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)
  engine.model_manager.selected = ModelReference("owner/model", "revision")
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine_module, "Thread", DormantThread)

  snapshot = engine.enqueue_files(
    [
      {"path": str(first), "displayName": first.name},
      {"path": str(second), "displayName": second.name},
    ]
  )

  sessions = engine.repository.list_sessions().items
  assert len(sessions) == 1
  assert sessions[0]["source_display_name"] == first.name
  assert engine._active_session_id == sessions[0]["session_id"]
  assert snapshot["autoAdvanceEnabled"] is True
  assert [item["displayName"] for item in cast(list[dict[str, object]], snapshot["items"])] == [second.name]


def test_enqueue_only_appends_when_waiting_items_already_exist(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  waiting = tmp_path / "waiting.wav"
  added = tmp_path / "added.wav"
  waiting.touch()
  added.touch()
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)
  engine.repository.enqueue_files((NewQueueItem(str(waiting), waiting.name, fingerprint_file_snapshot(waiting).value),))

  snapshot = engine.enqueue_files([{"path": str(added), "displayName": added.name}])

  assert engine._active_session_id is None
  assert snapshot["autoAdvanceEnabled"] is False
  assert [item["displayName"] for item in cast(list[dict[str, object]], snapshot["items"])] == [
    waiting.name,
    added.name,
  ]


def test_enqueue_keeps_files_waiting_when_model_is_unavailable_or_engine_is_shutting_down(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  missing_model = tmp_path / "missing-model.wav"
  shutdown_file = tmp_path / "shutdown.wav"
  missing_model.touch()
  shutdown_file.touch()
  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)

  unavailable_engine = RecoEngine(tmp_path / "unavailable.sqlite3", tmp_path / "models")
  unavailable = unavailable_engine.enqueue_files([{"path": str(missing_model)}])

  shutdown_engine = RecoEngine(tmp_path / "shutdown.sqlite3", tmp_path / "models")
  shutdown_engine._shutting_down = True
  shutdown = shutdown_engine.enqueue_files([{"path": str(shutdown_file)}])

  assert unavailable_engine._active_session_id is None
  assert unavailable["autoAdvanceEnabled"] is False
  assert len(cast(list[object], unavailable["items"])) == 1
  assert shutdown_engine._active_session_id is None
  assert shutdown["autoAdvanceEnabled"] is False
  assert len(cast(list[object], shutdown["items"])) == 1


def test_failed_model_selection_is_persisted_and_disables_the_previous_runtime(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "assets")
  paused_session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", "old-model", "old-revision", "Japanese", 16_000, "Recording")
  )
  engine.repository.set_state(paused_session_id, SessionState.PAUSING)
  engine.repository.pause_session(paused_session_id, 0)
  previous_closed = False

  class PreviousRuntime:
    def close(self) -> None:
      nonlocal previous_closed
      previous_closed = True

  class FailingRuntime:
    def __init__(self, path: Path) -> None:
      assert path == snapshot

    def acquire(self) -> None:
      raise RuntimeError("incompatible model")

    def close(self) -> None:
      return None

  engine.runtime = cast(ModelRuntime, PreviousRuntime())
  monkeypatch.setattr(engine.model_manager, "resolve", lambda reference: snapshot)
  monkeypatch.setattr(engine_module, "ModelRuntime", FailingRuntime)

  with pytest.raises(EngineCommandError, match="incompatible model"):
    engine.select_model("owner/model", "commit")

  assert previous_closed is True
  assert engine.runtime is None
  assert engine.repository.get_selected_model() == ("owner/model", "commit")
  assert engine.model_manager.snapshot().state is ModelState.ERROR
  assert engine.model_manager.snapshot().selected == ModelReference("owner/model", "commit")


def test_model_selection_is_rejected_while_a_session_or_queue_is_active(tmp_path: Path) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "assets")
  engine._active_session_id = "active"
  with pytest.raises(EngineCommandError) as active:
    engine.select_model("owner/model", "commit")
  assert active.value.code == "session_active"

  engine._active_session_id = None
  engine._queue_auto_advance = True
  with pytest.raises(EngineCommandError) as queue:
    engine.select_model("owner/model", "commit")
  assert queue.value.code == "queue_active"


def test_concurrent_idle_enqueues_create_only_one_active_session(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  files = [tmp_path / "one.wav", tmp_path / "two.wav"]
  for path in files:
    path.touch()

  class DormantThread:
    def __init__(self, **options: object) -> None:
      del options

    def start(self) -> None:
      return None

  barrier = Barrier(2)
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)
  monkeypatch.setattr(engine_module, "Thread", DormantThread)

  del barrier
  engine.model_manager.selected = ModelReference("owner/model", "revision")
  engine.model_manager._state = ModelState.READY
  workers = [
    Thread(target=engine.enqueue_files, args=([{"path": str(path), "displayName": path.name}],)) for path in files
  ]
  for worker in workers:
    worker.start()
  for worker in workers:
    worker.join()

  assert len(engine.repository.list_sessions().items) == 1
  assert len(cast(list[object], engine.queue_state()["items"])) == 1


def test_restarting_engine_recognizes_paused_file_session_as_queue_origin(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  source = tmp_path / "audio.wav"
  source.write_bytes(b"stable audio identity")
  fingerprint = fingerprint_file_snapshot(source)
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(
    NewSession(
      "file",
      source.name,
      "model",
      None,
      "Japanese",
      16_000,
      "Audio",
      source_fingerprint=fingerprint.value,
      source_path=str(source),
    )
  )
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 1)

  class DormantThread:
    def __init__(self, **options: object) -> None:
      del options

    def start(self) -> None:
      return None

  engine = RecoEngine(database, tmp_path / "models")
  monkeypatch.setattr(engine_module, "Thread", DormantThread)

  engine.resume_session(session_id)

  assert engine._active_queue_origin is True
  assert engine.queue_state()["autoAdvanceEnabled"] is True


def test_scheduler_marks_missing_item_invalid_and_stops_without_creating_history(tmp_path: Path) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  engine.repository.enqueue_files(
    (NewQueueItem(str(tmp_path / "missing.wav"), "missing.wav", "sha256:missing", "missing"),)
  )
  engine._queue_auto_advance = True

  engine._schedule_next_queue_item()

  snapshot = engine.queue_state()
  assert snapshot["autoAdvanceEnabled"] is False
  assert cast(list[dict[str, object]], snapshot["items"])[0]["state"] == "invalid"
  assert engine.repository.list_sessions().items == ()


def test_drained_pause_persists_the_resume_sample(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  class FakeRuntime:
    def acquire(self) -> tuple[object, object]:
      return object(), object()

    def invalidate(self) -> None:
      return None

  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", "model", None, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  control.request_stop("userPause")
  engine.runtime = cast(ModelRuntime, FakeRuntime())
  engine.model_manager.selected = ModelReference("model", "revision")
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine_module, "ensure_silero_vad_asset", lambda path: path)
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())

  def drain_transcription(*args: object, **options: object) -> SimpleNamespace:
    del args, options
    engine.repository.set_state(session_id, SessionState.PAUSING)
    return SimpleNamespace(timing=SimpleNamespace(media_duration_ms=1_000))

  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    drain_transcription,
  )

  engine._run_session(session_id, None, None, None, control)

  session = engine.repository.get_session(session_id)
  assert session["state"] == "paused"
  assert session["resume_sample"] == 16_000

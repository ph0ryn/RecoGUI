from __future__ import annotations

from collections.abc import Callable, Mapping
from pathlib import Path
from threading import Barrier, Event, Lock, Thread
from types import SimpleNamespace
from typing import cast

import pytest

import reco.engine as engine_module
from reco.engine import EngineCommandError, ModelRuntime, RecoEngine, SessionControl
from reco.host_pcm import HostPcmBroker, HostPcmInput
from reco.model_manager import ModelReference, ModelState
from reco.models import SplitReason, TranscriptionDiagnostics, TranscriptSegment, VadDiagnostics
from reco.pipeline import TranscriptionProgress
from reco.recording import fingerprint_file_snapshot
from reco.repository import (
  NewQueueItem,
  NewSession,
  RecordingRepository,
  RepositoryError,
  SessionMutationReceipt,
  SessionState,
)


class FakeHostPcmInput:
  def await_start(self) -> None:
    return None

  def drain_and_release(self) -> None:
    return None


class FakeHostPcmBroker:
  def input(self, session_id: str, generation: int, start_sample: int, source_kind: str) -> HostPcmInput:
    del session_id, generation, start_sample, source_kind
    return cast(HostPcmInput, FakeHostPcmInput())


@pytest.fixture(autouse=True)
def provide_host_pcm_to_engine(monkeypatch: pytest.MonkeyPatch) -> None:
  original_init = RecoEngine.__init__

  def initialize(
    self: RecoEngine,
    database: Path,
    vad_model: Path,
    event_callback: Callable[[str, str | None, Mapping[str, object]], None] | None = None,
    host_pcm_broker: HostPcmBroker | None = None,
  ) -> None:
    original_init(
      self,
      database,
      vad_model,
      event_callback,
      host_pcm_broker or cast(HostPcmBroker, FakeHostPcmBroker()),
    )

  monkeypatch.setattr(RecoEngine, "__init__", initialize)


def segment() -> TranscriptSegment:
  return TranscriptSegment(
    index=0,
    start_sample=0,
    end_sample=16_000,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text="hello",
    raw_text="hello",
    language="Japanese",
    vad=VadDiagnostics(0.8, 0.9, 0.75),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=4),
  )


def test_language_validation_rejects_arbitrary_values(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  reference = ModelReference("owner/model", "revision")
  monkeypatch.setattr(engine.model_manager, "supported_languages", lambda selected: ("Japanese", "English"))

  assert engine._validate_language(reference, None) is None
  assert engine._validate_language(reference, "English") == "English"
  with pytest.raises(EngineCommandError, match="does not support"):
    engine._validate_language(reference, "arbitrary")


def engine_with_repository(
  repository: RecordingRepository,
) -> tuple[RecoEngine, list[tuple[str, str | None, Mapping[str, object]]]]:
  events: list[tuple[str, str | None, Mapping[str, object]]] = []
  engine = RecoEngine.__new__(RecoEngine)
  engine.repository = repository
  engine._event_callback = lambda event, session_id, payload: events.append((event, session_id, payload))
  engine._active_queue_origin = False
  engine._queue_auto_advance = False
  engine._shutting_down = False
  return engine, events


def test_engine_startup_does_not_construct_a_model_runtime(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  repository.set_selected_model("owner/model", "commit")
  monkeypatch.setattr(
    engine_module,
    "ModelRuntime",
    lambda path: pytest.fail(f"engine startup unexpectedly loaded {path}"),
  )

  engine = RecoEngine(database, tmp_path / "models")

  assert engine.runtime is None


def test_model_runtime_close_invalidates_worker_and_clears_mlx_memory(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  runtime = ModelRuntime(tmp_path / "snapshot")
  calls: list[str] = []
  monkeypatch.setattr(runtime, "invalidate", lambda: calls.append("invalidate"))
  monkeypatch.setattr(engine_module.gc, "collect", lambda: calls.append("gc"))
  monkeypatch.setattr(engine_module, "_clear_mlx_cache", lambda: calls.append("mlx"))

  runtime.close()

  assert calls == ["invalidate", "gc", "mlx"]


def test_new_microphone_session_persists_the_host_device_uid(
  tmp_path: Path,
  monkeypatch: pytest.MonkeyPatch,
) -> None:
  captured: list[tuple[object, ...]] = []

  class CapturingThread:
    def __init__(self, *, target: object, args: tuple[object, ...], **options: object) -> None:
      del target, options
      captured.append(args)

    def start(self) -> None:
      return None

  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  engine.model_manager.selected = ModelReference("owner/model", "revision")
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine_module, "Thread", CapturingThread)

  result = engine.start_session(
    {
      "language": None,
      "source": {"type": "microphone", "deviceId": "studio-uid", "displayName": "Studio Mic"},
    }
  )

  session = engine.repository.get_session(str(result["sessionId"]))
  assert session["source_device_id"] == "studio-uid"
  assert session["source_display_name"] == "Studio Mic"
  assert captured[0][2] == "studio-uid"


def test_microphone_resume_passes_the_saved_uid_to_the_host(
  tmp_path: Path,
  monkeypatch: pytest.MonkeyPatch,
) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(
    NewSession(
      "microphone",
      "Studio Mic",
      "owner/model",
      "revision",
      "Japanese",
      16_000,
      "Recording",
      source_device_id="studio-uid",
    )
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 0)
  captured: list[tuple[object, ...]] = []

  class CapturingThread:
    def __init__(self, *, target: object, args: tuple[object, ...], **options: object) -> None:
      del target, options
      captured.append(args)

    def start(self) -> None:
      return None

  engine = RecoEngine(database, tmp_path / "models")
  monkeypatch.setattr(engine_module, "Thread", CapturingThread)

  engine.resume_session(session_id)

  assert captured[0][2] == "studio-uid"


def test_failed_live_session_resume_has_a_stable_nonrecoverable_error(tmp_path: Path) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("systemAudio", "Desktop Audio", "model", "revision", "Japanese", 16_000, "Recording")
  )
  engine.repository.set_state(session_id, SessionState.RUNNING)
  engine.repository.fail_session(session_id, "capture_failed", "Capture stopped")

  with pytest.raises(EngineCommandError) as failure:
    engine.resume_session(session_id)

  assert failure.value.code == "session_not_resumable"
  assert failure.value.recoverable is False


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
    "segment": {
      "segmentIndex": 0,
      "startSample": 0,
      "endSample": 16_000,
      "language": "Japanese",
      "text": "hello",
    },
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
  engine._active_queue_origin = True

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


def test_stop_finalizes_a_paused_session_without_restarting_it(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(
    NewSession("microphone", "Microphone", "model", "revision", "Japanese", 16_000, "Recording")
  )
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 16_000)
  engine, events = engine_with_repository(repository)
  engine._lock = Lock()
  engine._active_session_id = None
  engine._active_control = None
  engine._paused_queue_sessions = set()

  result = engine.stop_session(session_id)

  assert result["state"] == "stopped"
  assert repository.get_session(session_id)["end_reason"] == "userStop"
  assert [event for event, _, _ in events] == ["session.completed", "history.changed"]


def test_stop_finalizes_pause_before_worker_cleanup_without_changing_control(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(
    NewSession("microphone", "Microphone", "model", "revision", "Japanese", 16_000, "Recording")
  )
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 16_000)
  engine, events = engine_with_repository(repository)
  control = SessionControl()
  control.request_stop("userPause")
  engine._lock = Lock()
  engine._active_session_id = session_id
  engine._active_control = control
  engine._paused_queue_sessions = {session_id}

  result = engine.stop_session(session_id)

  assert result["state"] == "stopped"
  assert control.stop_reason == "userPause"
  assert session_id not in engine._paused_queue_sessions
  assert [event for event, _, _ in events] == ["session.completed", "history.changed"]


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


def test_model_selection_persists_cached_reference_without_loading_runtime(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "assets")
  reference = ModelReference("owner/model", "commit")

  def select(selected: ModelReference) -> None:
    assert selected == reference
    engine.model_manager.selected = selected
    engine.model_manager._state = ModelState.READY

  monkeypatch.setattr(engine.model_manager, "select", select)
  monkeypatch.setattr(
    engine_module,
    "ModelRuntime",
    lambda path: pytest.fail(f"model selection unexpectedly loaded {path}"),
  )

  result = engine.select_model(reference.repo_id, reference.revision)

  assert result == {
    "status": "ready",
    "selected": {"repoId": reference.repo_id, "revision": reference.revision},
  }
  assert engine.runtime is None
  assert engine.repository.get_selected_model() == (reference.repo_id, reference.revision)


def test_model_selection_rolls_back_when_persistence_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "assets")
  previous = ModelReference("owner/previous", "previous-revision")
  replacement = ModelReference("owner/replacement", "replacement-revision")
  engine.model_manager.selected = previous
  engine.model_manager._state = ModelState.READY

  def select(reference: ModelReference) -> None:
    engine.model_manager.selected = reference
    engine.model_manager._state = ModelState.READY

  monkeypatch.setattr(engine.model_manager, "select", select)
  monkeypatch.setattr(
    engine.repository,
    "set_selected_model",
    lambda repo_id, revision: (_ for _ in ()).throw(RepositoryError("write failed")),
  )

  with pytest.raises(RepositoryError, match="write failed"):
    engine.select_model(replacement.repo_id, replacement.revision)

  assert engine.model_manager.snapshot().selected == previous
  assert engine.model_manager.snapshot().state is ModelState.READY


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


def test_queue_pause_between_validation_and_claim_prevents_session_start(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  source = tmp_path / "audio.wav"
  source.write_bytes(b"stable")
  fingerprint = fingerprint_file_snapshot(source)
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  engine.model_manager.selected = reference
  engine.model_manager._state = ModelState.READY
  engine.repository.enqueue_files((NewQueueItem(str(source), source.name, fingerprint.value),))
  engine._queue_auto_advance = True
  reached_claim_boundary = Event()
  continue_claim = Event()

  def blocking_cast(type_: object, value: object) -> object:
    if type_ is ModelReference:
      reached_claim_boundary.set()
      assert continue_claim.wait(5)
    return value

  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)
  monkeypatch.setattr(engine_module, "cast", blocking_cast)
  scheduler = Thread(target=engine._schedule_next_queue_item)
  scheduler.start()
  assert reached_claim_boundary.wait(5)

  engine.pause_queue()
  continue_claim.set()
  scheduler.join(5)

  assert scheduler.is_alive() is False
  assert engine.repository.list_sessions().items == ()
  assert len(cast(list[object], engine.queue_state()["items"])) == 1
  assert engine._active_session_id is None


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
      "revision",
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
      ordering.append("start")
      return None

  engine = RecoEngine(database, tmp_path / "models")
  ordering: list[str] = []
  engine._event_callback = lambda event, session_id, payload: ordering.append(event)
  monkeypatch.setattr(engine_module, "Thread", DormantThread)

  engine.resume_session(session_id)

  assert engine._active_queue_origin is True
  assert engine.queue_state()["autoAdvanceEnabled"] is True
  assert ordering[:2] == ["session.stateChanged", "start"]


def test_failed_file_session_retries_from_its_committed_checkpoint(
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
      "revision",
      "Japanese",
      16_000,
      "Audio",
      source_fingerprint=fingerprint.value,
      source_path=str(source),
    )
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment())
  repository.fail_session(session_id, "transcription_failed", "Broken pipe")
  captured: list[tuple[object, ...]] = []

  class CapturingThread:
    def __init__(self, *, target: object, args: tuple[object, ...], **options: object) -> None:
      del target, options
      captured.append(args)

    def start(self) -> None:
      return None

  engine = RecoEngine(database, tmp_path / "models")
  monkeypatch.setattr(engine_module, "Thread", CapturingThread)

  engine.resume_session(session_id)

  assert captured[0][5] == 16_000
  assert captured[0][6] == 1
  assert captured[0][8] == "Japanese"
  assert captured[0][9] is SessionState.FAILED
  assert engine.repository.get_session(session_id)["state"] == "preparing"


def test_failed_file_retry_reports_an_unavailable_source_without_losing_its_checkpoint(tmp_path: Path) -> None:
  source = tmp_path / "missing.wav"
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(
    NewSession(
      "file",
      source.name,
      "model",
      "revision",
      "Japanese",
      16_000,
      "Audio",
      source_fingerprint="sha256:missing",
      source_path=str(source),
    )
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment())
  repository.fail_session(session_id, "transcription_failed", "Broken pipe")
  engine = RecoEngine(database, tmp_path / "models")

  with pytest.raises(EngineCommandError) as failure:
    engine.resume_session(session_id)

  assert failure.value.code == "resume_source_unavailable"
  session = engine.repository.get_session(session_id)
  assert session["state"] == "failed"
  assert session["resume_sample"] == 16_000
  assert engine._active_session_id is None


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
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", "model", "revision", "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  engine.model_manager.selected = ModelReference("model", "revision")
  engine.model_manager._state = ModelState.READY
  closed_states: list[str] = []

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      closed_states.append(str(engine.repository.get_session(session_id)["state"]))

  runtime = FakeRuntime()
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = ModelReference("model", "revision")
  monkeypatch.setattr(engine, "_acquire_runtime", lambda reference, language: (runtime, object(), object()))
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())

  def drain_transcription(*args: object, **options: object) -> SimpleNamespace:
    del args, options
    engine.repository.set_state(session_id, SessionState.PAUSING)
    control.request_stop("userPause")
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
  assert closed_states == ["paused"]
  assert engine.runtime is None


def test_session_loads_runtime_on_demand_and_releases_it_after_completion(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  created: list[Path] = []
  closed: list[Path] = []

  class FakeRuntime:
    def __init__(self, model_path: Path) -> None:
      self.model_path = model_path
      created.append(model_path)

    def acquire(self, language: str | None) -> tuple[object, object]:
      del language
      return object(), object()

    def close(self) -> None:
      closed.append(self.model_path)

  monkeypatch.setattr(engine.model_manager, "resolve", lambda requested: snapshot if requested == reference else None)
  monkeypatch.setattr(engine_module, "ModelRuntime", FakeRuntime)
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())
  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    lambda *args, **options: SimpleNamespace(timing=SimpleNamespace(media_duration_ms=0)),
  )
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(session_id, None, None, None, control, model_reference=reference)

  assert created == [snapshot]
  assert closed == [snapshot]
  assert engine.runtime is None
  assert engine.repository.get_session(session_id)["state"] == "completed"


def test_session_drops_local_model_references_before_runtime_release(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  import weakref

  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()

  class Resource:
    pass

  references: list[weakref.ReferenceType[Resource]] = []

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      assert all(reference() is None for reference in references)

  runtime = FakeRuntime()

  def acquire(reference: ModelReference, language: str | None) -> tuple[FakeRuntime, Resource, Resource]:
    del language
    service = Resource()
    worker = Resource()
    references.extend((weakref.ref(service), weakref.ref(worker)))
    engine.runtime = cast(ModelRuntime, runtime)
    engine._runtime_reference = reference
    return runtime, service, worker

  monkeypatch.setattr(engine, "_acquire_runtime", acquire)
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())
  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    lambda *args, **options: SimpleNamespace(timing=SimpleNamespace(media_duration_ms=0)),
  )
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(session_id, None, None, None, control, model_reference=reference)

  assert all(reference() is None for reference in references)


def test_runtime_release_failure_does_not_keep_the_active_slot(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()

  class FailingRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      raise RuntimeError("cleanup failed")

  runtime = FailingRuntime()
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = reference
  monkeypatch.setattr(engine, "_acquire_runtime", lambda requested, language: (runtime, object(), object()))
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())
  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    lambda *args, **options: SimpleNamespace(timing=SimpleNamespace(media_duration_ms=0)),
  )
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(session_id, None, None, None, control, model_reference=reference)

  assert engine.repository.get_session(session_id)["state"] == "completed"
  assert engine._active_session_id is None
  assert engine.runtime is None


@pytest.mark.parametrize(
  ("outcome", "expected_state"),
  [("stop", "stopped"), ("cancel", "stopped"), ("error", "failed")],
)
def test_terminal_outcomes_release_runtime_once(
  outcome: str, expected_state: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / f"{outcome}.sqlite3", tmp_path / f"{outcome}-models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  close_count = 0

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      nonlocal close_count
      close_count += 1

  runtime = FakeRuntime()
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = reference
  monkeypatch.setattr(engine, "_acquire_runtime", lambda requested, language: (runtime, object(), object()))
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())

  def finish(*args: object, **options: object) -> SimpleNamespace:
    del args, options
    if outcome == "error":
      raise RuntimeError("pipeline failed")
    if outcome == "cancel":
      control.request_cancel()
    else:
      control.request_stop("userStop")
    return SimpleNamespace(timing=SimpleNamespace(media_duration_ms=0))

  monkeypatch.setattr(engine_module, "run_transcription", finish)
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(session_id, None, None, None, control, model_reference=reference)

  assert engine.repository.get_session(session_id)["state"] == expected_state
  assert close_count == 1
  assert engine.runtime is None


def test_live_capture_starts_before_running_and_stops_before_error_drain(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "capture.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("systemAudio", "Desktop Audio", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  trace: list[str] = []

  class TracedInput:
    def await_start(self) -> None:
      trace.append("start")

    def drain_and_release(self) -> None:
      trace.append("drain")

  class TracedBroker:
    def input(self, captured_session_id: str, generation: int, start_sample: int, source_kind: str) -> HostPcmInput:
      assert (captured_session_id, generation, start_sample, source_kind) == (session_id, 0, 0, "systemAudio")
      trace.append("input")
      return cast(HostPcmInput, TracedInput())

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      return None

  runtime = FakeRuntime()
  engine.host_pcm_broker = cast(HostPcmBroker, TracedBroker())
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = reference
  engine._event_callback = lambda event, _session_id, _payload: trace.append(event)
  engine._active_session_id = session_id
  engine._active_control = control
  monkeypatch.setattr(engine, "_acquire_runtime", lambda requested, language: (runtime, object(), object()))
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())

  def fail_pipeline(*args: object, **options: object) -> None:
    del args, options
    trace.append("pipeline")
    raise RuntimeError("VAD failed")

  monkeypatch.setattr(engine_module, "run_transcription", fail_pipeline)

  engine._run_session(session_id, None, None, None, control, model_reference=reference, source_kind="systemAudio")

  assert trace.index("audio.captureRequested") < trace.index("start") < trace.index("session.stateChanged")
  assert trace.index("session.stateChanged") < trace.index("pipeline")
  assert trace.index("pipeline") < trace.index("audio.captureStopRequested") < trace.index("drain")
  failed_event_index = trace.index("session.failed")
  assert trace.index("drain") < failed_event_index
  assert engine.repository.get_session(session_id)["state"] == "failed"
  assert session_id not in engine._capture_generations


def test_stop_arriving_during_capture_start_is_resent_after_start_and_drained(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "start-race.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  waiting_in_start = Event()
  release_start = Event()
  trace: list[str] = []

  class RacingInput:
    def await_start(self) -> None:
      trace.append("awaitStart")
      waiting_in_start.set()
      assert release_start.wait(1)
      trace.append("started")

    def drain_and_release(self) -> None:
      trace.append("drained")

  class RacingBroker:
    def input(self, captured_session_id: str, generation: int, start_sample: int, source_kind: str) -> HostPcmInput:
      assert (captured_session_id, generation, start_sample, source_kind) == (session_id, 0, 0, "microphone")
      return cast(HostPcmInput, RacingInput())

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      return None

  runtime = FakeRuntime()
  engine.host_pcm_broker = cast(HostPcmBroker, RacingBroker())
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = reference
  engine._event_callback = lambda event, _session_id, _payload: trace.append(event)
  engine._active_session_id = session_id
  engine._active_control = control
  monkeypatch.setattr(engine, "_acquire_runtime", lambda requested, language: (runtime, object(), object()))
  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    lambda *args, **options: pytest.fail("pipeline must not run after a start-time stop"),
  )

  session_thread = Thread(
    target=engine._run_session,
    args=(session_id, None, None, None, control),
    kwargs={"model_reference": reference, "source_kind": "microphone"},
  )
  session_thread.start()
  assert waiting_in_start.wait(1)

  engine.pause_session(session_id)
  release_start.set()
  session_thread.join(1)

  assert not session_thread.is_alive()
  assert trace.count("audio.captureStopRequested") == 2
  assert trace.index("audio.captureRequested") < trace.index("awaitStart")
  post_start_stop = trace.index("audio.captureStopRequested", trace.index("started"))
  assert trace.index("started") < post_start_stop < trace.index("drained")
  assert engine.repository.get_session(session_id)["state"] == "paused"
  assert session_id in engine._capture_generations


@pytest.mark.parametrize("source_kind", ["microphone", "file"])
def test_pause_cannot_overtake_the_preparing_to_running_transition(
  source_kind: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "running-race.sqlite3", tmp_path / "models")
  source_path = tmp_path / "audio.wav" if source_kind == "file" else None
  session_id = engine.repository.create_session(
    NewSession(
      source_kind,
      "audio.wav" if source_path is not None else "Microphone",
      reference.repo_id,
      reference.revision,
      "Japanese",
      16_000,
      "Recording",
      source_path=str(source_path) if source_path is not None else None,
    )
  )
  control = SessionControl()
  cas_entered = Event()
  release_cas = Event()
  pause_completed = Event()
  state_events: list[str] = []

  class StartedInput:
    def await_start(self) -> None:
      return None

    def drain_and_release(self) -> None:
      return None

  class StartedBroker:
    def input(self, session: str, generation: int, start: int, source: str) -> HostPcmInput:
      del session, generation, start, source
      return cast(HostPcmInput, StartedInput())

  class FakeRuntime:
    model_path = tmp_path / "snapshot"

    def close(self) -> None:
      return None

  original_start_running = engine.repository.start_running

  def blocked_start_running(captured_session_id: str) -> SessionMutationReceipt | None:
    cas_entered.set()
    assert release_cas.wait(1)
    return original_start_running(captured_session_id)

  runtime = FakeRuntime()
  engine.host_pcm_broker = cast(HostPcmBroker, StartedBroker())
  engine.runtime = cast(ModelRuntime, runtime)
  engine._runtime_reference = reference
  engine._active_session_id = session_id
  engine._active_control = control
  engine._active_queue_origin = source_path is not None
  engine._event_callback = lambda event, _session_id, payload: (
    state_events.append(str(payload["state"])) if event == "session.stateChanged" else None
  )
  monkeypatch.setattr(engine.repository, "start_running", blocked_start_running)
  monkeypatch.setattr(engine, "_acquire_runtime", lambda requested, language: (runtime, object(), object()))
  monkeypatch.setattr(engine_module, "audio_file_duration_ms", lambda path: 1_000)
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())

  def drained_pipeline(*args: object, **options: object) -> SimpleNamespace:
    del args, options
    assert pause_completed.wait(1)
    return SimpleNamespace(timing=SimpleNamespace(media_duration_ms=0))

  monkeypatch.setattr(engine_module, "run_transcription", drained_pipeline)
  session_thread = Thread(
    target=engine._run_session,
    args=(session_id, source_path, None, None, control),
    kwargs={"model_reference": reference, "source_kind": source_kind},
  )
  session_thread.start()
  assert cas_entered.wait(1)

  def pause() -> None:
    engine.pause_session(session_id)
    pause_completed.set()

  pause_thread = Thread(target=pause)
  pause_thread.start()
  release_cas.set()
  pause_thread.join(1)
  session_thread.join(1)

  assert not pause_thread.is_alive()
  assert not session_thread.is_alive()
  assert state_events == ["running", "pausing", "paused"]
  assert engine.repository.get_session(session_id)["state"] == "paused"


def test_new_session_model_load_failure_is_terminal(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  control = SessionControl()
  engine._active_session_id = session_id
  engine._active_control = control
  monkeypatch.setattr(engine.model_manager, "resolve", lambda requested: None)

  engine._run_session(session_id, None, None, None, control, model_reference=reference)

  session = engine.repository.get_session(session_id)
  assert session["state"] == "failed"
  assert session["error_code"] == "model_unavailable"
  assert engine.runtime is None
  assert engine._active_session_id is None


def test_shutdown_does_not_release_runtime_owned_by_a_live_session(tmp_path: Path) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", "model", "revision", "Japanese", 16_000, "Recording")
  )

  class BlockedThread:
    def join(self, timeout: float) -> None:
      assert timeout == 0

    def is_alive(self) -> bool:
      return True

  engine._active_session_id = session_id
  engine._active_control = SessionControl()
  engine._active_thread = cast(Thread, BlockedThread())
  closed = False

  class ActiveRuntime:
    def close(self) -> None:
      nonlocal closed
      closed = True

  engine.runtime = cast(ModelRuntime, ActiveRuntime())
  engine._runtime_reference = ModelReference("model", "revision")

  engine.shutdown(timeout=0)

  assert engine.repository.get_session(session_id)["state"] == "abandoned"
  assert closed is False
  assert engine.runtime is not None


def test_model_refresh_never_closes_an_active_runtime(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  closed = False

  class FakeRuntime:
    def close(self) -> None:
      nonlocal closed
      closed = True

  engine.runtime = cast(ModelRuntime, FakeRuntime())
  engine._runtime_reference = ModelReference("owner/model", "revision")
  engine._active_session_id = "active"
  monkeypatch.setattr(engine.model_manager, "list_models", lambda: [])

  engine.list_models()

  assert closed is False
  assert engine.runtime is not None


def test_resume_uses_the_session_model_and_preserves_pause_when_loading_fails(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  original = ModelReference("owner/original", "original-revision")
  selected = ModelReference("owner/default", "default-revision")
  session_id = repository.create_session(
    NewSession("microphone", "Microphone", original.repo_id, original.revision, "Japanese", 16_000, "Recording")
  )
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 32_000)
  captured: list[tuple[object, ...]] = []

  class CapturingThread:
    def __init__(self, *, target: object, args: tuple[object, ...], **options: object) -> None:
      del target, options
      captured.append(args)

    def start(self) -> None:
      return None

  engine = RecoEngine(database, tmp_path / "models")
  engine.model_manager.selected = selected
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine_module, "Thread", CapturingThread)
  monkeypatch.setattr(engine.model_manager, "resolve", lambda reference: None)

  engine.resume_session(session_id)
  assert captured[0][7] == original

  cast(Callable[..., None], engine._run_session)(*captured[0])

  session = engine.repository.get_session(session_id)
  assert session["state"] == "paused"
  assert session["resume_sample"] == 32_000
  assert session["error_code"] == "model_unavailable"
  assert engine._active_session_id is None
  assert engine.runtime is None


def test_resume_model_initialization_failure_preserves_the_checkpoint(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  engine.repository.set_state(session_id, SessionState.PAUSING)
  engine.repository.pause_session(session_id, 48_000)
  engine.repository.set_state(session_id, SessionState.PREPARING)
  control = SessionControl()

  class FailingRuntime:
    def __init__(self, model_path: Path) -> None:
      assert model_path == snapshot

    def acquire(self, language: str | None) -> tuple[object, object]:
      del language
      raise RuntimeError("incompatible model")

    def close(self) -> None:
      return None

  monkeypatch.setattr(engine.model_manager, "resolve", lambda requested: snapshot if requested == reference else None)
  monkeypatch.setattr(engine_module, "ModelRuntime", FailingRuntime)
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(
    session_id,
    None,
    None,
    None,
    control,
    initial_sample=48_000,
    model_reference=reference,
    resuming_from=SessionState.PAUSED,
  )

  session = engine.repository.get_session(session_id)
  assert session["state"] == "paused"
  assert session["resume_sample"] == 48_000
  assert session["error_code"] == "model_load_failed"
  assert engine._active_session_id is None


def test_failed_retry_model_initialization_failure_preserves_the_checkpoint(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  reference = ModelReference("owner/model", "revision")
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession("microphone", "Microphone", reference.repo_id, reference.revision, "Japanese", 16_000, "Recording")
  )
  engine.repository.set_state(session_id, SessionState.RUNNING)
  engine.repository.append_segment(session_id, segment())
  engine.repository.fail_session(session_id, "transcription_failed", "Broken pipe")
  engine.repository.set_state(session_id, SessionState.PREPARING)
  control = SessionControl()
  monkeypatch.setattr(engine.model_manager, "resolve", lambda requested: None)
  engine._active_session_id = session_id
  engine._active_control = control

  engine._run_session(
    session_id,
    None,
    None,
    None,
    control,
    initial_sample=16_000,
    segment_offset=1,
    model_reference=reference,
    resuming_from=SessionState.FAILED,
  )

  session = engine.repository.get_session(session_id)
  assert session["state"] == "failed"
  assert session["resume_sample"] == 16_000
  assert session["error_code"] == "model_unavailable"
  assert engine._active_session_id is None


def test_continuous_queue_reuses_runtime_until_the_last_file_finishes(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  first = tmp_path / "first.wav"
  second = tmp_path / "second.wav"
  first.write_bytes(b"first")
  second.write_bytes(b"second")
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  reference = ModelReference("owner/model", "revision")
  threads: list[tuple[object, tuple[object, ...]]] = []
  created: list[Path] = []
  closed: list[Path] = []

  class CapturingThread:
    def __init__(self, *, target: object, args: tuple[object, ...], **options: object) -> None:
      del options
      threads.append((target, args))

    def start(self) -> None:
      return None

  class FakeRuntime:
    def __init__(self, model_path: Path) -> None:
      self.model_path = model_path
      created.append(model_path)

    def acquire(self, language: str | None) -> tuple[object, object]:
      del language
      return object(), object()

    def close(self) -> None:
      closed.append(self.model_path)

  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  engine.model_manager.selected = reference
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine.model_manager, "resolve", lambda requested: snapshot if requested == reference else None)
  monkeypatch.setattr(engine_module, "ModelRuntime", FakeRuntime)
  monkeypatch.setattr(engine_module, "Thread", CapturingThread)
  monkeypatch.setattr(engine_module, "validate_audio_file", lambda path: None)
  monkeypatch.setattr(engine_module, "audio_file_duration_ms", lambda path: 1_000)
  monkeypatch.setattr(engine_module, "LocalAudioFileInput", lambda *args, **options: object())
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())
  monkeypatch.setattr(
    engine_module,
    "run_transcription",
    lambda *args, **options: SimpleNamespace(timing=SimpleNamespace(media_duration_ms=1_000)),
  )

  engine.enqueue_files(
    [
      {"path": str(first), "displayName": first.name},
      {"path": str(second), "displayName": second.name},
    ]
  )

  first_target, first_args = threads[0]
  cast(Callable[..., None], first_target)(*first_args)
  assert created == [snapshot]
  assert closed == []
  assert len(threads) == 2

  second_target, second_args = threads[1]
  cast(Callable[..., None], second_target)(*second_args)
  assert created == [snapshot]
  assert closed == [snapshot]
  assert engine.runtime is None
  assert engine.queue_state()["autoAdvanceEnabled"] is False

  threads.clear()
  created.clear()
  closed.clear()
  paused_engine = RecoEngine(tmp_path / "paused.sqlite3", tmp_path / "paused-models")
  paused_engine.model_manager.selected = reference
  paused_engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(
    paused_engine.model_manager,
    "resolve",
    lambda requested: snapshot if requested == reference else None,
  )
  paused_engine.enqueue_files(
    [
      {"path": str(first), "displayName": first.name},
      {"path": str(second), "displayName": second.name},
    ]
  )
  paused_engine.pause_queue()

  paused_target, paused_args = threads[0]
  cast(Callable[..., None], paused_target)(*paused_args)

  assert created == [snapshot]
  assert closed == [snapshot]
  assert paused_engine.runtime is None
  assert len(cast(list[object], paused_engine.queue_state()["items"])) == 1

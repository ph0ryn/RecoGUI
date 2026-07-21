from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from threading import Event, Lock
from types import SimpleNamespace
from typing import cast
from uuid import uuid4

import pytest

import reco.engine as engine_module
from reco.engine import ModelRuntime, RecoEngine, SessionControl
from reco.model_manager import ModelReference, ModelState
from reco.protocol import NdjsonWriter, Request
from reco.repository import ExportResult, NewSession, SessionMutationReceipt, SessionPage, SessionState
from reco.sidecar import SidecarServer


class StubRepository:
  def __init__(self) -> None:
    self.export_call: tuple[list[str], Path, str] | None = None
    self.states: list[tuple[str, SessionState]] = []
    self.search_options: dict[str, object] | None = None
    self.rename_call: tuple[str, str] | None = None
    self.render_call: tuple[list[str], str] | None = None

  def export_sessions(
    self,
    ids: list[str],
    destination: Path,
    export_format: str,
    **options: object,
  ) -> ExportResult:
    self.export_call = (ids, destination, export_format)
    progress = options.get("progress")
    if callable(progress):
      cast(Callable[[dict[str, object]], None], progress)(
        {"phase": "publishing", "completedItems": len(ids), "totalItems": len(ids)}
      )
    return ExportResult(tuple(ids), ())

  def set_state(self, session_id: str, state: SessionState) -> SessionMutationReceipt:
    self.states.append((session_id, state))
    return SessionMutationReceipt(state, 2, 0, 0, 0, 0, None)

  def search_sessions(self, query: str, **options: object) -> SessionPage:
    self.search_options = {"query": query, **options}
    return SessionPage((), None)

  def rename_session(self, session_id: str, title: str) -> dict[str, object]:
    self.rename_call = (session_id, title)
    return {"session_id": session_id, "title": title.strip(), "row_version": 4}

  def render_sessions(self, session_ids: list[str], export_format: str) -> str:
    self.render_call = (session_ids, export_format)
    return "rendered transcript"


class StubEngine:
  def __init__(self) -> None:
    self.repository = StubRepository()
    self.start_payload: object = None
    self.pause_call: str | None = None
    self.resume_call: str | None = None
    self.stop_call: tuple[str, str] | None = None
    self.queue_call: tuple[str, object] | None = None
    self.model_call: tuple[str, object] | None = None

  def state(self) -> dict[str, object]:
    return {"model": {"status": "unselected", "selected": None}}

  def list_models(self) -> dict[str, object]:
    self.model_call = ("list", None)
    return {"models": [], "state": {"status": "unselected", "selected": None}}

  def select_model(self, repo_id: str, revision: str) -> dict[str, object]:
    self.model_call = ("select", (repo_id, revision))
    return {"status": "ready", "selected": {"repoId": repo_id, "revision": revision}}

  def start_session(self, payload: object, requested_session_id: str | None) -> dict[str, object]:
    self.start_payload = payload
    return {"sessionId": requested_session_id or str(uuid4()), "state": "preparing"}

  def stop_session(self, session_id: str, *, reason: str) -> dict[str, object]:
    self.stop_call = (session_id, reason)
    return {"sessionId": session_id, "state": "stopping"}

  def pause_session(self, session_id: str) -> dict[str, object]:
    self.pause_call = session_id
    return {"sessionId": session_id, "state": "pausing"}

  def resume_session(self, session_id: str) -> dict[str, object]:
    self.resume_call = session_id
    return {"sessionId": session_id, "state": "preparing"}

  def queue_state(self) -> dict[str, object]:
    self.queue_call = ("state", None)
    return {"revision": 0, "autoAdvanceEnabled": False, "items": []}

  def enqueue_files(self, files: object) -> dict[str, object]:
    self.queue_call = ("enqueue", files)
    return self.queue_state()

  def reorder_queue(self, item_ids: object, revision: object) -> dict[str, object]:
    self.queue_call = ("reorder", (item_ids, revision))
    return {"revision": 2, "autoAdvanceEnabled": False, "items": []}

  def remove_queue_item(self, item_id: str) -> dict[str, object]:
    self.queue_call = ("remove", item_id)
    return {"revision": 2, "autoAdvanceEnabled": False, "items": []}

  def clear_queue(self) -> dict[str, object]:
    self.queue_call = ("clear", None)
    return {"revision": 2, "autoAdvanceEnabled": False, "items": []}

  def start_queue(self) -> dict[str, object]:
    self.queue_call = ("start", None)
    return {"revision": 2, "autoAdvanceEnabled": True, "items": []}

  def pause_queue(self) -> dict[str, object]:
    self.queue_call = ("pause", None)
    return {"revision": 2, "autoAdvanceEnabled": False, "items": []}


class StubWriter:
  def __init__(self) -> None:
    self.events: list[tuple[str, dict[str, object]]] = []
    self.completed = Event()

  def event(self, event: str, session_id: str | None, payload: dict[str, object]) -> None:
    del session_id
    self.events.append((event, payload))
    if event == "export.completed":
      self.completed.set()


def request(command: str, payload: dict[str, object], session_id: str | None = None) -> Request:
  return Request(str(uuid4()), session_id, 1, command, payload)


def server_with_stub() -> tuple[SidecarServer, StubEngine]:
  server = SidecarServer.__new__(SidecarServer)
  engine = StubEngine()
  server.engine = cast(RecoEngine, engine)
  server.writer = cast(NdjsonWriter, StubWriter())
  return server, engine


def test_session_start_accepts_nested_rust_source_payload() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())
  payload: dict[str, object] = {
    "source": {"type": "file", "path": "/resolved/audio.wav"},
    "title": "Audio",
  }

  result = server.dispatch(request("session.start", payload, session_id))

  assert engine.start_payload == payload
  assert result == {"sessionId": session_id, "state": "preparing"}


def test_model_cache_commands_cross_the_sidecar_contract() -> None:
  server, engine = server_with_stub()

  assert server.dispatch(request("model.getState", {}))["status"] == "unselected"
  assert server.dispatch(request("model.list", {}))["models"] == []
  selected = server.dispatch(request("model.select", {"repoId": "owner/model", "revision": "commit"}))

  assert selected["status"] == "ready"
  assert engine.model_call == ("select", ("owner/model", "commit"))


def test_system_sleep_reason_crosses_the_sidecar_contract() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())
  payload: dict[str, object] = {
    "sessionId": session_id,
    "reason": "systemSleep",
    "context": {"source": "macOSWorkspaceNotification"},
  }

  server.dispatch(request("session.stop", payload, session_id))

  assert engine.stop_call == (session_id, "systemSleep")


def test_pause_and_resume_cross_the_sidecar_contract() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())

  paused = server.dispatch(request("session.pause", {}, session_id))
  resumed = server.dispatch(request("session.resume", {}, session_id))

  assert paused == {"sessionId": session_id, "state": "pausing"}
  assert resumed == {"sessionId": session_id, "state": "preparing"}
  assert engine.pause_call == session_id
  assert engine.resume_call == session_id


def test_queue_commands_cross_the_sidecar_contract() -> None:
  server, engine = server_with_stub()
  files = [{"path": "/resolved/audio.wav", "displayName": "audio.wav"}]

  server.dispatch(request("queue.enqueueFiles", {"files": files}))
  assert engine.queue_call == ("state", None)
  server.dispatch(request("queue.reorder", {"revision": 1, "itemIds": ["item"]}))
  assert engine.queue_call == ("reorder", (["item"], 1))
  server.dispatch(request("queue.remove", {"itemId": "item"}))
  assert engine.queue_call == ("remove", "item")
  assert server.dispatch(request("queue.start", {}))["autoAdvanceEnabled"] is True
  assert server.dispatch(request("queue.pause", {}))["autoAdvanceEnabled"] is False


def test_export_uses_destination_but_does_not_return_the_path() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())
  destination = Path("/resolved/export.md")

  result = server.dispatch(
    request(
      "history.export",
      {
        "sessionIds": [session_id],
        "destination": str(destination),
        "format": "markdown",
        "overwrite": True,
      },
      session_id,
    )
  )

  assert result["accepted"] is True
  assert isinstance(result["operationId"], str)
  server._accept_export(result["operationId"])
  writer = cast(StubWriter, server.writer)
  assert writer.completed.wait(timeout=1)
  assert engine.repository.export_call == ([session_id], destination, "markdown")
  assert [event for event, _ in writer.events] == ["export.progress", "export.completed"]
  completed = writer.events[-1][1]
  assert completed["status"] == "completed"
  assert completed["succeededSessionIds"] == [session_id]
  assert str(destination) not in str(result)


def test_cancel_export_sets_the_active_operation_event() -> None:
  server, _ = server_with_stub()
  cancel = Event()
  server._export_lock = Lock()
  server._export_operations = {"operation-1": cancel}

  result = server.dispatch(request("history.cancelExport", {"operationId": "operation-1"}))

  assert result == {"operationId": "operation-1", "cancelRequested": True}
  assert cancel.is_set()


def test_search_accepts_rust_status_source_and_date_filters() -> None:
  server, engine = server_with_stub()

  result = server.dispatch(
    request(
      "history.search",
      {
        "query": "lecture",
        "cursor": "50",
        "limit": 25,
        "status": "completed",
        "source": "file",
        "startedAfter": "2026-07-01T00:00:00+00:00",
        "startedBefore": "2026-08-01T00:00:00+00:00",
      },
    )
  )

  assert result == {"items": [], "nextCursor": None}
  assert engine.repository.search_options == {
    "query": "lecture",
    "limit": 25,
    "cursor": 50,
    "states": (SessionState.COMPLETED,),
    "source_kind": "file",
    "started_after": "2026-07-01T00:00:00+00:00",
    "started_before": "2026-08-01T00:00:00+00:00",
  }


def test_history_rename_updates_the_repository_and_emits_history_changed() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())

  result = server.dispatch(request("history.rename", {"title": "Renamed"}, session_id))

  assert result == {"sessionId": session_id, "title": "Renamed", "rowVersion": 4}
  assert engine.repository.rename_call == (session_id, "Renamed")
  assert cast(StubWriter, server.writer).events == [("history.changed", {"sessionId": session_id})]


def test_history_render_returns_clipboard_content() -> None:
  server, engine = server_with_stub()
  session_id = str(uuid4())

  result = server.dispatch(request("history.render", {"sessionIds": [session_id], "format": "markdown"}))

  assert result == {"content": "rendered transcript"}
  assert engine.repository.render_call == ([session_id], "markdown")


def test_engine_state_does_not_expose_managed_model_path(tmp_path: Path) -> None:
  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")

  state = engine.state()

  assert "path" not in cast(dict[str, object], state["model"])


def test_stop_control_retains_system_sleep_reason_for_terminal_persistence() -> None:
  session_id = str(uuid4())
  repository = StubRepository()
  engine = RecoEngine.__new__(RecoEngine)
  control = SessionControl()
  engine.repository = cast(object, repository)
  engine._lock = Lock()
  engine._active_session_id = session_id
  engine._active_control = control
  engine._event_callback = None

  engine.stop_session(session_id, reason="systemSleep")

  assert control.stop_reason == "systemSleep"
  assert repository.states == [(session_id, SessionState.STOPPING)]


def test_system_sleep_is_persisted_as_terminal_reason(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  class FakeRuntime:
    def acquire(self) -> tuple[object, object]:
      return object(), object()

  engine = RecoEngine(tmp_path / "reco.sqlite3", tmp_path / "models")
  session_id = engine.repository.create_session(
    NewSession(
      source_kind="file",
      source_display_name="audio.wav",
      model="model",
      model_revision="revision",
      language="Japanese",
      sample_rate=16_000,
      title="Audio",
    )
  )
  control = SessionControl()
  control.request_stop("systemSleep")
  engine.runtime = cast(ModelRuntime, FakeRuntime())
  engine.model_manager.selected = ModelReference("model", "revision")
  engine.model_manager._state = ModelState.READY
  monkeypatch.setattr(engine_module, "audio_file_duration_ms", lambda path: 1_000)
  monkeypatch.setattr(engine_module, "ensure_silero_vad_asset", lambda path: path)
  monkeypatch.setattr(engine_module, "OnnxSileroProbabilityModel", lambda path: object())
  monkeypatch.setattr(engine_module, "SileroVadEngine", lambda **options: object())
  monkeypatch.setattr(engine_module, "run_transcription", lambda *args, **options: SimpleNamespace(total_segments=0))

  engine._run_session(session_id, tmp_path / "audio.wav", None, None, control)

  session = engine.repository.get_session(session_id)
  assert session["state"] == "stopped"
  assert session["end_reason"] == "systemSleep"

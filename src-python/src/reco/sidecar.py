"""RecoGUI headless engine NDJSON sidecar entrypoint."""

from __future__ import annotations

import argparse
import json
import logging
import sys
from collections.abc import Mapping
from dataclasses import asdict
from pathlib import Path
from threading import Event, Lock, Thread
from typing import Any, BinaryIO, cast
from uuid import uuid4

from reco.engine import EngineCommandError, RecoEngine, _camel
from reco.host_pcm import HostPcmBroker
from reco.protocol import (
  MAX_LINE_BYTES,
  PROTOCOL_VERSION,
  NdjsonWriter,
  ProtocolError,
  Request,
  error_payload,
  parse_request,
)
from reco.repository import ExportCancelled, ExportFailure, SessionState
from reco.vad import validate_silero_vad_asset

LOG = logging.getLogger("reco-engine")


class SidecarServer:
  """Strict request dispatcher whose stdout contains protocol messages only."""

  def __init__(
    self,
    database: Path,
    vad_model: Path,
    output: Any = None,
    audio_source: BinaryIO | None = None,
  ) -> None:
    self.writer = NdjsonWriter(output or sys.stdout)
    self.audio_broker = HostPcmBroker(audio_source) if audio_source is not None else None
    self.engine = RecoEngine(database, vad_model, self._event, self.audio_broker)
    self._stopped = Event()
    self._previous_request_sequence = 0
    self._export_lock = Lock()
    self._export_operations: dict[str, Event] = {}
    self._export_acceptance: dict[str, Event] = {}
    self._export_threads: dict[str, Thread] = {}

  def serve(self) -> int:
    """Serve requests until EOF or `engine.shutdown`."""

    heartbeat = Thread(target=self._heartbeat, daemon=True, name="reco-heartbeat")
    heartbeat.start()
    source = sys.stdin.buffer
    try:
      while not self._stopped.is_set():
        raw_line = source.readline(MAX_LINE_BYTES + 1)
        if not raw_line:
          break
        if len(raw_line) > MAX_LINE_BYTES and not raw_line.endswith(b"\n"):
          while chunk := source.readline(MAX_LINE_BYTES + 1):
            if chunk.endswith(b"\n"):
              break
          LOG.error("Discarded oversized protocol request")
          continue
        try:
          request = parse_request(raw_line, previous_sequence=self._previous_request_sequence)
          self._previous_request_sequence = request.sequence
          payload = self.dispatch(request)
          self.writer.response(request, payload)
          operation_id = payload.get("operationId")
          if payload.get("accepted") is True and isinstance(operation_id, str):
            self._accept_export(operation_id)
        except ProtocolError as exc:
          LOG.error("Protocol error %s: %s", exc.code, exc)
          if exc.request_id is not None:
            synthetic = Request(exc.request_id, None, self._previous_request_sequence + 1, "invalid", {})
            self.writer.response(
              synthetic,
              {},
              error=error_payload(exc.code, str(exc), recoverable=False),
            )
        except EngineCommandError as exc:
          self.writer.response(
            request,
            {},
            error=error_payload(exc.code, str(exc), recoverable=exc.recoverable, details=exc.details),
          )
        except (ValueError, KeyError) as exc:
          self.writer.response(request, {}, error=error_payload("invalid_payload", str(exc), recoverable=True))
        except BaseException as exc:
          LOG.exception("Unhandled engine command failure")
          self.writer.response(request, {}, error=error_payload("internal_error", str(exc), recoverable=True))
    finally:
      self._stopped.set()
      self._stop_exports()
      self.engine.shutdown()
    return 0

  def dispatch(self, request: Request) -> dict[str, object]:
    """Dispatch the complete public command set."""

    command = request.command
    payload = request.payload
    if command == "engine.getState":
      return self.engine.state()
    if command == "engine.shutdown":
      self.engine.shutdown()
      self._stopped.set()
      return {"state": "stopped"}
    if command == "model.getState":
      return _mapping(self.engine.state()["model"])
    if command == "model.list":
      return self.engine.list_models()
    if command == "model.select":
      repo_id = payload.get("repoId")
      revision = payload.get("revision")
      if not isinstance(repo_id, str) or not isinstance(revision, str):
        raise ValueError("repoId and revision must be strings")
      return self.engine.select_model(repo_id, revision)
    if command == "session.start":
      return self.engine.start_session(payload, request.session_id)
    if command == "session.stop":
      return self.engine.stop_session(
        _session_id(request, payload),
        reason=str(payload.get("reason", "userStop")),
      )
    if command == "session.pause":
      return self.engine.pause_session(_session_id(request, payload))
    if command == "session.resume":
      return self.engine.resume_session(_session_id(request, payload))
    if command == "queue.getState":
      return self.engine.queue_state()
    if command == "queue.enqueueFiles":
      return self.engine.enqueue_files(payload.get("files"), payload.get("language"))
    if command == "queue.reorder":
      return self.engine.reorder_queue(payload.get("itemIds"), payload.get("revision"))
    if command == "queue.remove":
      item_id = payload.get("itemId")
      if not isinstance(item_id, str) or not item_id:
        raise ValueError("itemId must be a non-empty string")
      return self.engine.remove_queue_item(item_id)
    if command == "queue.clear":
      return self.engine.clear_queue()
    if command == "queue.start":
      return self.engine.start_queue(payload.get("language"))
    if command == "queue.pause":
      return self.engine.pause_queue()
    if command == "history.list":
      states = tuple(SessionState(value) for value in _string_list(payload.get("states", []), allow_empty=True))
      page = self.engine.repository.list_sessions(
        limit=_integer(payload.get("limit", 50), "limit"),
        cursor=_optional_string(payload.get("cursor")),
        states=states,
        source_kind=_optional_string(payload.get("sourceKind")),
      )
      return {"items": _camel([_public_session(item) for item in page.items]), "nextCursor": page.next_cursor}
    if command == "history.get":
      result = self.engine.repository.get_session(
        _session_id(request, payload),
        segment_offset=_integer(payload.get("segmentOffset", 0), "segmentOffset"),
        segment_limit=_integer(payload.get("segmentLimit", 500), "segmentLimit"),
      )
      return _mapping(_camel(_public_session(result)))
    if command == "history.search":
      status = _optional_string(payload.get("status"))
      page = self.engine.repository.search_sessions(
        str(payload.get("query", "")),
        limit=_integer(payload.get("limit", 50), "limit"),
        cursor=_cursor_offset(payload.get("cursor")),
        states=() if status is None else (SessionState(status),),
        source_kind=_optional_string(payload.get("source")),
        started_after=_optional_string(payload.get("startedAfter")),
        started_before=_optional_string(payload.get("startedBefore")),
      )
      return {"items": _camel([_public_session(item) for item in page.items]), "nextCursor": page.next_cursor}
    if command == "history.rename":
      result = self.engine.repository.rename_session(
        _session_id(request, payload),
        str(payload.get("title", "")),
      )
      self._event("history.changed", str(result["session_id"]), {"sessionId": result["session_id"]})
      return _mapping(_camel(result))
    if command == "history.render":
      return {
        "content": self.engine.repository.render_sessions(
          _string_list(payload.get("sessionIds")),
          str(payload.get("format", "txt")),
        )
      }
    if command in {"history.delete", "history.deleteMany"}:
      ids = [_session_id(request, payload)] if command == "history.delete" else _string_list(payload.get("sessionIds"))
      return {"deleted": self.engine.repository.delete_sessions(ids)}
    if command in {"history.export", "history.exportMany"}:
      ids = _string_list(payload.get("sessionIds"))
      destination = Path(str(payload["destination"]))
      return self._start_export(
        ids,
        destination,
        str(payload.get("format", "txt")),
        overwrite=_boolean(payload.get("overwrite", False), "overwrite"),
      )
    if command == "history.cancelExport":
      return self._cancel_export(str(payload.get("operationId", "")))
    raise EngineCommandError("unknown_command", f"Unknown engine command: {command}", recoverable=False)

  def _start_export(
    self,
    session_ids: list[str],
    destination: Path,
    export_format: str,
    *,
    overwrite: bool,
  ) -> dict[str, object]:
    operation_id = str(uuid4())
    cancel_event = Event()
    accepted_event = Event()
    export_lock, operations = self._export_state()
    with export_lock:
      operations[operation_id] = cancel_event
      self._export_acceptance[operation_id] = accepted_event

    def progress(payload: dict[str, object]) -> None:
      self._event("export.progress", None, {"operationId": operation_id, **payload})

    def export() -> None:
      accepted_event.wait()
      try:
        result = self.engine.repository.export_sessions(
          session_ids,
          destination,
          export_format,
          overwrite=overwrite,
          cancel_event=cancel_event,
          progress=progress,
        )
        failures = [_camel(asdict(failure)) for failure in result.failures]
        status = "completed" if not failures else ("partial" if result.exported_session_ids else "failed")
        self._event(
          "export.completed",
          None,
          {
            "operationId": operation_id,
            "status": status,
            "succeededSessionIds": list(result.exported_session_ids),
            "failures": failures,
          },
        )
      except ExportCancelled:
        self._event(
          "export.completed",
          None,
          {
            "operationId": operation_id,
            "status": "cancelled",
            "succeededSessionIds": [],
            "failures": [],
          },
        )
      except BaseException as exc:
        failures = [_camel(asdict(ExportFailure(session_id, "export_failed", str(exc)))) for session_id in session_ids]
        self._event(
          "export.completed",
          None,
          {
            "operationId": operation_id,
            "status": "failed",
            "succeededSessionIds": [],
            "failures": failures,
          },
        )
      finally:
        with export_lock:
          operations.pop(operation_id, None)
          self._export_acceptance.pop(operation_id, None)
          self._export_threads.pop(operation_id, None)

    thread = Thread(target=export, daemon=True, name=f"reco-export-{operation_id}")
    with export_lock:
      self._export_threads[operation_id] = thread
    thread.start()
    return {"accepted": True, "operationId": operation_id}

  def _cancel_export(self, operation_id: str) -> dict[str, object]:
    if not operation_id:
      raise ValueError("operationId is required")
    export_lock, operations = self._export_state()
    with export_lock:
      cancel_event = operations.get(operation_id)
    if cancel_event is None:
      return {"operationId": operation_id, "cancelRequested": False, "reason": "operation_not_active"}
    cancel_event.set()
    return {"operationId": operation_id, "cancelRequested": True}

  def _accept_export(self, operation_id: str) -> None:
    export_lock, _ = self._export_state()
    with export_lock:
      accepted = self._export_acceptance.get(operation_id)
    if accepted is not None:
      accepted.set()

  def _stop_exports(self) -> None:
    export_lock, operations = self._export_state()
    with export_lock:
      active = [
        (operations[operation_id], self._export_acceptance[operation_id], thread)
        for operation_id, thread in self._export_threads.items()
        if operation_id in operations and operation_id in self._export_acceptance
      ]
    for cancel, accepted, _ in active:
      cancel.set()
      accepted.set()
    for _, _, thread in active:
      thread.join(timeout=5)

  def _export_state(self) -> tuple[Lock, dict[str, Event]]:
    if not hasattr(self, "_export_lock"):
      self._export_lock = Lock()
      self._export_operations = {}
    if not hasattr(self, "_export_acceptance"):
      self._export_acceptance = {}
    if not hasattr(self, "_export_threads"):
      self._export_threads = {}
    return self._export_lock, self._export_operations

  def _event(self, event: str, session_id: str | None, payload: Mapping[str, object]) -> None:
    self.writer.event(event, session_id, dict(payload))

  def _heartbeat(self) -> None:
    while not self._stopped.wait(2.0):
      self._event("engine.heartbeat", None, self.engine.state())


def build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(prog="reco-engine")
  subparsers = parser.add_subparsers(dest="subcommand", required=True)
  serve = subparsers.add_parser("serve")
  serve.add_argument("--protocol-version", type=int, required=True)
  serve.add_argument("--database", type=Path, required=True)
  serve.add_argument("--vad-model", type=Path, required=True)
  serve.add_argument("--logs-directory", type=Path, required=True)
  serve.add_argument("--audio-fd", type=int, required=True)
  return parser


def main(argv: list[str] | None = None) -> int:
  args = build_parser().parse_args(argv)
  if args.protocol_version != PROTOCOL_VERSION:
    print(f"Unsupported protocol version: {args.protocol_version}", file=sys.stderr)
    return 2
  args.logs_directory.mkdir(parents=True, exist_ok=True)
  logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="%(asctime)s %(levelname)s %(message)s")
  with open(args.audio_fd, "rb", buffering=0, closefd=True) as audio_source:
    return SidecarServer(args.database, validate_silero_vad_asset(args.vad_model), audio_source=audio_source).serve()


def _session_id(request: Request, payload: Mapping[str, object]) -> str:
  value = request.session_id or payload.get("sessionId")
  if not isinstance(value, str) or not value:
    raise ValueError("sessionId is required")
  return value


def _string_list(value: object, *, allow_empty: bool = False) -> list[str]:
  if (
    not isinstance(value, list)
    or (not allow_empty and not value)
    or not all(isinstance(item, str) and item for item in value)
  ):
    raise ValueError("sessionIds must be a non-empty string array")
  return cast(list[str], value)


def _integer(value: object, name: str) -> int:
  if not isinstance(value, int) or isinstance(value, bool):
    raise ValueError(f"{name} must be an integer")
  return value


def _cursor_offset(value: object) -> int:
  if value is None:
    return 0
  if not isinstance(value, str) or not value.isdecimal():
    raise ValueError("cursor must be a non-negative integer string or null")
  return int(value)


def _boolean(value: object, name: str) -> bool:
  if not isinstance(value, bool):
    raise ValueError(f"{name} must be a boolean")
  return value


def _optional_string(value: object) -> str | None:
  if value is None:
    return None
  if not isinstance(value, str):
    raise ValueError("Expected a string or null")
  return value


def _mapping(value: object) -> dict[str, object]:
  if not isinstance(value, dict):
    raise TypeError("Expected a mapping")
  return cast(dict[str, object], value)


def _public_session(value: Mapping[str, object]) -> dict[str, object]:
  private_fields = {"source_path", "source_device_id", "resume_sample"}
  result = {key: item for key, item in value.items() if key not in private_fields}
  serialized_languages = result.pop("detected_languages_json", "[]")
  try:
    detected_languages = json.loads(str(serialized_languages))
  except json.JSONDecodeError:
    detected_languages = []
  result["detected_languages"] = (
    detected_languages
    if isinstance(detected_languages, list) and all(isinstance(item, str) for item in detected_languages)
    else []
  )
  return result


if __name__ == "__main__":
  raise SystemExit(main())

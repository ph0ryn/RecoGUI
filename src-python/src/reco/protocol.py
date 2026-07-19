"""Versioned UTF-8 NDJSON protocol primitives."""

from __future__ import annotations

import json
from dataclasses import dataclass
from threading import Lock
from typing import TextIO, cast
from uuid import UUID

PROTOCOL_VERSION = 1
MAX_LINE_BYTES = 8 * 1024 * 1024


class ProtocolError(ValueError):
  """Invalid incoming protocol message with a stable error code."""

  def __init__(self, code: str, message: str, *, request_id: str | None = None) -> None:
    super().__init__(message)
    self.code = code
    self.request_id = request_id


@dataclass(frozen=True)
class Request:
  """Validated request envelope."""

  request_id: str
  session_id: str | None
  sequence: int
  command: str
  payload: dict[str, object]


class NdjsonWriter:
  """Single synchronized stdout writer and sequence source."""

  def __init__(self, output: TextIO) -> None:
    self.output = output
    self._sequence = 0
    self._lock = Lock()

  def response(
    self,
    request: Request,
    payload: dict[str, object],
    *,
    error: dict[str, object] | None = None,
  ) -> None:
    self._write(
      {
        "protocolVersion": PROTOCOL_VERSION,
        "type": "response",
        "requestId": request.request_id,
        "sessionId": request.session_id,
        "ok": error is None,
        "error": error,
        "payload": payload,
      }
    )

  def event(self, event: str, session_id: str | None, payload: dict[str, object]) -> None:
    self._write(
      {
        "protocolVersion": PROTOCOL_VERSION,
        "type": "event",
        "requestId": None,
        "sessionId": session_id,
        "event": event,
        "payload": payload,
      }
    )

  def _write(self, envelope: dict[str, object]) -> None:
    with self._lock:
      self._sequence += 1
      envelope["sequence"] = self._sequence
      encoded = json.dumps(envelope, ensure_ascii=False, separators=(",", ":"))
      if len(encoded.encode("utf-8")) > MAX_LINE_BYTES:
        raise ProtocolError("output_too_large", "Protocol output exceeds 8 MiB")
      self.output.write(encoded + "\n")
      self.output.flush()


def parse_request(raw_line: bytes, *, previous_sequence: int) -> Request:
  """Parse and validate one complete request line."""

  if len(raw_line) > MAX_LINE_BYTES:
    raise ProtocolError("line_too_large", "Protocol request exceeds 8 MiB")
  try:
    value = json.loads(raw_line.decode("utf-8"))
  except UnicodeDecodeError as exc:
    raise ProtocolError("invalid_utf8", "Protocol request is not UTF-8") from exc
  except json.JSONDecodeError as exc:
    raise ProtocolError("invalid_json", f"Protocol request is not valid JSON: {exc.msg}") from exc
  if not isinstance(value, dict):
    raise ProtocolError("invalid_envelope", "Protocol request must be an object")
  request_id = value.get("requestId")
  if not isinstance(request_id, str) or not _is_uuid(request_id):
    raise ProtocolError("invalid_request_id", "requestId must be a UUID")
  if value.get("protocolVersion") != PROTOCOL_VERSION:
    raise ProtocolError(
      "unsupported_protocol", f"Only protocol version {PROTOCOL_VERSION} is supported", request_id=request_id
    )
  if value.get("type") != "request":
    raise ProtocolError("invalid_message_type", "Incoming messages must have type=request", request_id=request_id)
  session_id = value.get("sessionId")
  if session_id is not None and (not isinstance(session_id, str) or not _is_uuid(session_id)):
    raise ProtocolError("invalid_session_id", "sessionId must be a UUID or null", request_id=request_id)
  sequence = value.get("sequence")
  if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= previous_sequence:
    raise ProtocolError("invalid_sequence", "Request sequence must increase monotonically", request_id=request_id)
  command = value.get("command")
  payload = value.get("payload")
  if not isinstance(command, str) or not command.strip() or not isinstance(payload, dict):
    raise ProtocolError(
      "invalid_request", "command must be non-empty and payload must be an object", request_id=request_id
    )
  expected = {"protocolVersion", "type", "requestId", "sessionId", "sequence", "command", "payload"}
  if set(value) != expected:
    raise ProtocolError("invalid_envelope", "Request contains missing or unknown properties", request_id=request_id)
  return Request(request_id=request_id, session_id=session_id, sequence=sequence, command=command, payload=payload)


def parse_protocol_message(raw: bytes) -> dict[str, object]:
  """Validate a canonical request, response, or event fixture."""

  if len(raw) > MAX_LINE_BYTES:
    raise ProtocolError("line_too_large", "Protocol message exceeds 8 MiB")
  try:
    value = json.loads(raw.decode("utf-8"))
  except (UnicodeDecodeError, json.JSONDecodeError) as exc:
    raise ProtocolError("invalid_json", "Protocol fixture is not valid UTF-8 JSON") from exc
  if not isinstance(value, dict):
    raise ProtocolError("invalid_envelope", "Protocol fixture must be an object")
  message_type = value.get("type")
  if message_type == "request":
    parse_request(raw, previous_sequence=0)
    return value
  if value.get("protocolVersion") != PROTOCOL_VERSION:
    raise ProtocolError("unsupported_protocol", f"Only protocol version {PROTOCOL_VERSION} is supported")
  if not isinstance(value.get("sequence"), int) or value["sequence"] < 1:
    raise ProtocolError("invalid_sequence", "Protocol sequence must be a positive integer")
  if not isinstance(value.get("payload"), dict):
    raise ProtocolError("invalid_payload", "Protocol payload must be an object")
  session_id = value.get("sessionId")
  if session_id is not None and (not isinstance(session_id, str) or not _is_uuid(session_id)):
    raise ProtocolError("invalid_session_id", "sessionId must be a UUID or null")
  request_id = value.get("requestId")
  if message_type == "response":
    if not isinstance(request_id, str) or not _is_uuid(request_id):
      raise ProtocolError("invalid_request_id", "Response requestId must be a UUID")
    if not isinstance(value.get("ok"), bool):
      raise ProtocolError("invalid_response", "Response ok must be boolean")
    error = value.get("error")
    if error is not None:
      _validate_error(error)
    expected = {"protocolVersion", "type", "requestId", "sessionId", "sequence", "ok", "error", "payload"}
  elif message_type == "event":
    if request_id is not None or not isinstance(value.get("event"), str) or not value["event"]:
      raise ProtocolError("invalid_event", "Event requires a name and null requestId")
    expected = {"protocolVersion", "type", "requestId", "sessionId", "sequence", "event", "payload"}
  else:
    raise ProtocolError("invalid_message_type", "Unknown protocol message type")
  if set(value) != expected:
    raise ProtocolError("invalid_envelope", "Protocol fixture contains missing or unknown properties")
  return value


def error_payload(
  code: str, message: str, *, recoverable: bool, details: dict[str, object] | None = None
) -> dict[str, object]:
  """Build the canonical response error object."""

  return {"code": code, "message": message, "recoverable": recoverable, "details": details}


def _is_uuid(value: str) -> bool:
  try:
    UUID(value)
  except ValueError:
    return False
  return True


def _validate_error(value: object) -> None:
  if not isinstance(value, dict):
    raise ProtocolError("invalid_error", "Response error must be an object or null")
  value = cast(dict[str, object], value)
  expected = {"code", "message", "recoverable", "details"}
  if (
    set(value) != expected
    or not isinstance(value.get("code"), str)
    or not value["code"]
    or not isinstance(value.get("message"), str)
    or not isinstance(value.get("recoverable"), bool)
    or (value.get("details") is not None and not isinstance(value.get("details"), dict))
  ):
    raise ProtocolError("invalid_error", "Response error does not match the canonical shape")

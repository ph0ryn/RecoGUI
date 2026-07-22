from __future__ import annotations

import io
import json
from pathlib import Path
from uuid import uuid4

import pytest

from reco.protocol import PROTOCOL_VERSION, NdjsonWriter, ProtocolError, Request, parse_protocol_message, parse_request


def request_line(*, sequence: int = 1, version: int = PROTOCOL_VERSION) -> bytes:
  return (
    json.dumps(
      {
        "protocolVersion": version,
        "type": "request",
        "requestId": str(uuid4()),
        "sessionId": None,
        "sequence": sequence,
        "command": "engine.getState",
        "payload": {},
      }
    ).encode()
    + b"\n"
  )


def test_request_fixture_is_validated() -> None:
  request = parse_request(request_line(), previous_sequence=0)
  assert request.command == "engine.getState"
  assert request.sequence == 1


def test_sequence_must_increase() -> None:
  with pytest.raises(ProtocolError, match="increase"):
    parse_request(request_line(sequence=2), previous_sequence=2)


def test_unknown_protocol_version_has_stable_error() -> None:
  with pytest.raises(ProtocolError) as failure:
    parse_request(request_line(version=1), previous_sequence=0)
  assert failure.value.code == "unsupported_protocol"


def test_writer_uses_one_monotonic_sequence_for_events_and_responses() -> None:
  output = io.StringIO()
  writer = NdjsonWriter(output)
  request = Request(str(uuid4()), None, 1, "engine.getState", {})

  writer.event("engine.heartbeat", None, {})
  writer.response(request, {"engineState": "idle"})

  messages = [json.loads(line) for line in output.getvalue().splitlines()]
  assert [message["sequence"] for message in messages] == [1, 2]
  assert messages[1]["error"] is None


def test_repository_root_shared_protocol_fixtures_parse() -> None:
  fixtures_directory = Path(__file__).resolve().parents[2] / "protocol" / "fixtures"
  fixtures = sorted(fixtures_directory.glob("*.json"))

  parsed = [parse_protocol_message(fixture.read_bytes()) for fixture in fixtures]

  assert fixtures
  assert {message["type"] for message in parsed} == {"request", "response", "event"}

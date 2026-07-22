from __future__ import annotations

import io
import json
import struct
from pathlib import Path

import pytest

from reco_worker.protocol import (
  MAGIC,
  MAX_BINARY_BYTES,
  MAX_JSON_BYTES,
  PROTOCOL_VERSION,
  AsrProtocolError,
  FrameKind,
  encode_frame,
  read_frame,
)

HEADER = struct.Struct("<4sHHII")
FIXTURE_PATH = Path(__file__).resolve().parents[2] / "fixtures" / "rasr-v1" / "frames.json"


class FragmentedReader(io.BytesIO):
  def __init__(self, value: bytes, fragment_size: int) -> None:
    super().__init__(value)
    self.fragment_size = fragment_size

  def read(self, size: int | None = -1) -> bytes:
    requested = -1 if size is None else size
    return super().read(min(requested, self.fragment_size))


def test_header_is_exactly_sixteen_little_endian_bytes() -> None:
  encoded = encode_frame(FrameKind.REQUEST, {"requestId": "r", "operation": "shutdown"})

  assert HEADER.size == 16
  assert encoded[:16] == HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, 40, 0)


def test_round_trip_survives_every_single_byte_read_boundary() -> None:
  encoded = encode_frame(FrameKind.REQUEST, {"message": "日本語"}, b"\x00\x01\x02")

  frame = read_frame(FragmentedReader(encoded, fragment_size=1))

  assert frame is not None
  assert frame.kind is FrameKind.REQUEST
  assert frame.metadata == {"message": "日本語"}
  assert frame.binary == b"\x00\x01\x02"


def test_json_and_binary_wire_limits_are_inclusive() -> None:
  metadata = b"{}" + b" " * (MAX_JSON_BYTES - 2)
  binary = b"\x00" * MAX_BINARY_BYTES
  encoded = HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, len(metadata), len(binary)) + metadata + binary

  frame = read_frame(io.BytesIO(encoded))

  assert frame is not None
  assert frame.metadata == {}
  assert frame.binary == binary


@pytest.mark.parametrize(
  ("metadata_length", "binary_length", "code"),
  [
    (MAX_JSON_BYTES + 1, 0, "jsonTooLarge"),
    (0, MAX_BINARY_BYTES + 1, "binaryTooLarge"),
  ],
)
def test_oversized_lengths_are_rejected_before_payload_reads(
  metadata_length: int,
  binary_length: int,
  code: str,
) -> None:
  header_only = HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, metadata_length, binary_length)

  with pytest.raises(AsrProtocolError) as raised:
    read_frame(io.BytesIO(header_only))

  assert raised.value.code == code


@pytest.mark.parametrize(
  ("header", "payload", "code"),
  [
    (HEADER.pack(b"NOPE", PROTOCOL_VERSION, FrameKind.REQUEST, 2, 0), b"{}", "invalidMagic"),
    (HEADER.pack(MAGIC, 2, FrameKind.REQUEST, 2, 0), b"{}", "unsupportedVersion"),
    (HEADER.pack(MAGIC, PROTOCOL_VERSION, 99, 2, 0), b"{}", "unknownFrameKind"),
    (HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, 1, 0), b"{", "invalidJson"),
    (HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, 2, 0), b"[]", "invalidMetadata"),
    (HEADER.pack(MAGIC, PROTOCOL_VERSION, FrameKind.REQUEST, 2, 1), b"{}", "unexpectedEof"),
  ],
)
def test_malformed_frames_fail_with_stable_protocol_codes(header: bytes, payload: bytes, code: str) -> None:
  with pytest.raises(AsrProtocolError) as raised:
    read_frame(io.BytesIO(header + payload))

  assert raised.value.code == code


def test_encoder_rejects_payloads_above_the_wire_limits() -> None:
  with pytest.raises(AsrProtocolError, match="JSON metadata") as json_error:
    encode_frame(FrameKind.REQUEST, {"value": "x" * MAX_JSON_BYTES})
  assert json_error.value.code == "jsonTooLarge"

  with pytest.raises(AsrProtocolError, match="Binary payload") as binary_error:
    encode_frame(FrameKind.REQUEST, {}, b"\x00" * (MAX_BINARY_BYTES + 1))
  assert binary_error.value.code == "binaryTooLarge"


def test_clean_eof_is_distinct_from_a_truncated_header() -> None:
  assert read_frame(io.BytesIO()) is None
  with pytest.raises(AsrProtocolError) as raised:
    read_frame(io.BytesIO(b"RASR"))
  assert raised.value.code == "unexpectedEof"


def test_shared_rasr_v1_valid_frames_round_trip_exact_metadata() -> None:
  fixture = json.loads(FIXTURE_PATH.read_text())

  assert fixture["protocolVersion"] == PROTOCOL_VERSION
  assert bytes.fromhex(fixture["magicHex"]) == MAGIC
  assert fixture["limits"] == {
    "jsonBytes": MAX_JSON_BYTES,
    "binaryBytes": MAX_BINARY_BYTES,
    "segmentBytes": 3_840_000,
  }
  for case in fixture["valid"]:
    encoded = encode_frame(
      FrameKind(case["kind"]),
      case["metadata"],
      bytes.fromhex(case["binaryHex"]),
    )
    decoded = read_frame(FragmentedReader(encoded, fragment_size=3))
    assert decoded is not None, case["name"]
    assert decoded.kind is FrameKind(case["kind"]), case["name"]
    assert decoded.metadata == case["metadata"], case["name"]
    assert decoded.binary == bytes.fromhex(case["binaryHex"]), case["name"]

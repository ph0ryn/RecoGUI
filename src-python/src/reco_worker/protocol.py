"""Framed binary protocol used by the isolated ASR worker."""

from __future__ import annotations

import json
import struct
from collections.abc import Mapping
from dataclasses import dataclass
from enum import IntEnum
from threading import Lock
from typing import BinaryIO, cast

MAGIC = b"RASR"
PROTOCOL_VERSION = 1
MAX_JSON_BYTES = 64 * 1024
MAX_BINARY_BYTES = 4 * 1024 * 1024

_HEADER = struct.Struct("<4sHHII")


class FrameKind(IntEnum):
  """Wire-level frame kinds shared with the Rust host."""

  HELLO = 1
  REQUEST = 2
  RESPONSE = 3
  HEARTBEAT = 4


class AsrProtocolError(ValueError):
  """Malformed or unsupported RASR data."""

  def __init__(self, code: str, message: str) -> None:
    super().__init__(message)
    self.code = code


@dataclass(frozen=True)
class Frame:
  """One decoded RASR frame."""

  kind: FrameKind
  metadata: dict[str, object]
  binary: bytes = b""


class FrameWriter:
  """Serialize complete frames without interleaving concurrent writers."""

  def __init__(self, stream: BinaryIO) -> None:
    self._stream = stream
    self._lock = Lock()

  def write(
    self,
    kind: FrameKind,
    metadata: Mapping[str, object],
    binary: bytes | bytearray | memoryview = b"",
  ) -> None:
    metadata_bytes = _encode_metadata(metadata)
    binary_view = memoryview(binary).cast("B")
    if len(binary_view) > MAX_BINARY_BYTES:
      raise AsrProtocolError("binaryTooLarge", f"Binary payload exceeds {MAX_BINARY_BYTES} bytes")
    header = _HEADER.pack(MAGIC, PROTOCOL_VERSION, int(kind), len(metadata_bytes), len(binary_view))
    with self._lock:
      _write_all(self._stream, header)
      _write_all(self._stream, metadata_bytes)
      _write_all(self._stream, binary_view)
      self._stream.flush()


def read_frame(stream: BinaryIO) -> Frame | None:
  """Read one frame, returning ``None`` only for a clean stream EOF."""

  header = _read_exact(stream, _HEADER.size, allow_clean_eof=True)
  if header is None:
    return None
  magic, version, raw_kind, metadata_length, binary_length = _HEADER.unpack(header)
  if magic != MAGIC:
    raise AsrProtocolError("invalidMagic", "RASR frame magic is invalid")
  if version != PROTOCOL_VERSION:
    raise AsrProtocolError("unsupportedVersion", f"Only RASR version {PROTOCOL_VERSION} is supported")
  try:
    kind = FrameKind(raw_kind)
  except ValueError as exc:
    raise AsrProtocolError("unknownFrameKind", f"Unknown RASR frame kind: {raw_kind}") from exc

  # Lengths are validated before allocating metadata or binary payload buffers.
  if metadata_length > MAX_JSON_BYTES:
    raise AsrProtocolError("jsonTooLarge", f"JSON metadata exceeds {MAX_JSON_BYTES} bytes")
  if binary_length > MAX_BINARY_BYTES:
    raise AsrProtocolError("binaryTooLarge", f"Binary payload exceeds {MAX_BINARY_BYTES} bytes")

  metadata_raw = _read_exact(stream, metadata_length)
  binary = _read_exact(stream, binary_length)
  assert metadata_raw is not None
  assert binary is not None
  try:
    decoded = json.loads(metadata_raw.decode("utf-8"))
  except UnicodeDecodeError as exc:
    raise AsrProtocolError("invalidUtf8", "RASR metadata is not UTF-8") from exc
  except json.JSONDecodeError as exc:
    raise AsrProtocolError("invalidJson", f"RASR metadata is not valid JSON: {exc.msg}") from exc
  if not isinstance(decoded, dict):
    raise AsrProtocolError("invalidMetadata", "RASR metadata must be a JSON object")
  if not all(isinstance(key, str) for key in decoded):
    raise AsrProtocolError("invalidMetadata", "RASR metadata keys must be strings")
  return Frame(kind, cast(dict[str, object], decoded), binary)


def encode_frame(
  kind: FrameKind,
  metadata: Mapping[str, object],
  binary: bytes | bytearray | memoryview = b"",
) -> bytes:
  """Encode one frame for fixtures and transport tests."""

  metadata_bytes = _encode_metadata(metadata)
  binary_bytes = bytes(binary)
  if len(binary_bytes) > MAX_BINARY_BYTES:
    raise AsrProtocolError("binaryTooLarge", f"Binary payload exceeds {MAX_BINARY_BYTES} bytes")
  return (
    _HEADER.pack(MAGIC, PROTOCOL_VERSION, int(kind), len(metadata_bytes), len(binary_bytes))
    + metadata_bytes
    + binary_bytes
  )


def _encode_metadata(metadata: Mapping[str, object]) -> bytes:
  try:
    encoded = json.dumps(dict(metadata), ensure_ascii=False, separators=(",", ":")).encode("utf-8")
  except (TypeError, ValueError) as exc:
    raise AsrProtocolError("invalidMetadata", f"RASR metadata cannot be encoded: {exc}") from exc
  if len(encoded) > MAX_JSON_BYTES:
    raise AsrProtocolError("jsonTooLarge", f"JSON metadata exceeds {MAX_JSON_BYTES} bytes")
  return encoded


def _read_exact(stream: BinaryIO, size: int, *, allow_clean_eof: bool = False) -> bytes | None:
  chunks = bytearray()
  while len(chunks) < size:
    chunk = stream.read(size - len(chunks))
    if not chunk:
      if allow_clean_eof and not chunks:
        return None
      raise AsrProtocolError("unexpectedEof", "RASR stream ended within a frame")
    chunks.extend(chunk)
  return bytes(chunks)


def _write_all(stream: BinaryIO, value: bytes | memoryview) -> None:
  view = memoryview(value).cast("B")
  written = 0
  while written < len(view):
    count = stream.write(view[written:])
    if count is None:
      count = 0
    if count <= 0:
      raise AsrProtocolError("writeFailed", "RASR stream did not accept a complete frame")
    written += count

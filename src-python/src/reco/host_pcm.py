"""Strict host-to-sidecar PCM transport for live audio sources."""

from __future__ import annotations

import json
import struct
from collections.abc import Iterator
from dataclasses import dataclass
from enum import IntEnum
from threading import Lock
from typing import BinaryIO, cast
from uuid import UUID

import numpy as np

from reco.audio import SAMPLE_RATE, VAD_FRAME_SAMPLES, AudioChunk, AudioInput, AudioStream, SourceMetadata
from reco.errors import RecoError

PCM_MAGIC = b"RPCM"
PCM_WIRE_VERSION = 1
PCM_HEADER = struct.Struct("<4sHH16sIQQIHHIII")
PCM_HEADER_BYTES = PCM_HEADER.size
PCM_SAMPLE_FORMAT_F32LE = 1
PCM_ERROR_MAX_BYTES = 4 * 1024


class PcmRecordKind(IntEnum):
  """Record discriminants shared with the Rust host."""

  START = 1
  DATA = 2
  END = 3
  ERROR = 4


class HostPcmError(RecoError):
  """Stable host capture or PCM transport failure."""

  def __init__(self, code: str, message: str) -> None:
    super().__init__(message)
    self.code = code


@dataclass(frozen=True)
class PcmHeader:
  """Decoded fixed-width PCM record header."""

  kind: PcmRecordKind
  session_id: UUID
  generation: int
  sequence: int
  start_sample: int
  sample_rate: int
  channels: int
  sample_format: int
  sample_count: int
  payload_length: int
  flags: int


class HostPcmBroker:
  """Serialize ownership of the inherited host PCM stream."""

  def __init__(self, source: BinaryIO) -> None:
    self._source = source
    self._lock = Lock()
    self._active: HostPcmInput | None = None
    self._closed = False

  def input(
    self,
    session_id: str,
    generation: int,
    start_sample: int,
    source_kind: str,
  ) -> HostPcmInput:
    if source_kind not in {"microphone", "systemAudio"}:
      raise ValueError(f"Unsupported host PCM source kind: {source_kind}")
    if generation < 0 or start_sample < 0:
      raise ValueError("PCM generation and start sample must not be negative")
    try:
      parsed_session_id = UUID(session_id)
    except ValueError as exc:
      raise ValueError("PCM session ID must be a UUID") from exc
    with self._lock:
      if self._closed:
        raise HostPcmError("audio_transport_closed", "The host PCM transport is closed")
      if self._active is not None:
        raise HostPcmError("audio_transport_busy", "Another live audio session owns the PCM transport")
      audio_input = HostPcmInput(self, parsed_session_id, generation, start_sample, source_kind)
      self._active = audio_input
      return audio_input

  def close(self) -> None:
    with self._lock:
      self._closed = True

  def _read_exactly(self, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
      chunk = self._source.read(remaining)
      if not chunk:
        received = size - remaining
        raise HostPcmError(
          "audio_transport_eof",
          f"Host PCM transport ended after {received} of {size} expected bytes",
        )
      chunks.append(chunk)
      remaining -= len(chunk)
    return b"".join(chunks)

  def _read_header(self) -> PcmHeader:
    values = PCM_HEADER.unpack(self._read_exactly(PCM_HEADER_BYTES))
    magic = cast(bytes, values[0])
    version = cast(int, values[1])
    kind_value = cast(int, values[2])
    if magic != PCM_MAGIC:
      raise HostPcmError("audio_invalid_magic", "Host PCM record has an invalid magic value")
    if version != PCM_WIRE_VERSION:
      raise HostPcmError(
        "audio_unsupported_wire_version",
        f"Only host PCM wire version {PCM_WIRE_VERSION} is supported",
      )
    try:
      kind = PcmRecordKind(kind_value)
    except ValueError as exc:
      raise HostPcmError("audio_invalid_record_kind", f"Unknown host PCM record kind: {kind_value}") from exc
    return PcmHeader(
      kind=kind,
      session_id=UUID(bytes=cast(bytes, values[3])),
      generation=cast(int, values[4]),
      sequence=cast(int, values[5]),
      start_sample=cast(int, values[6]),
      sample_rate=cast(int, values[7]),
      channels=cast(int, values[8]),
      sample_format=cast(int, values[9]),
      sample_count=cast(int, values[10]),
      payload_length=cast(int, values[11]),
      flags=cast(int, values[12]),
    )

  def _release(self, audio_input: HostPcmInput) -> None:
    with self._lock:
      if self._active is audio_input:
        self._active = None
        audio_input._released = True


class HostPcmInput(AudioInput):
  """One validated live PCM generation received from the Rust host."""

  def __init__(
    self,
    broker: HostPcmBroker,
    session_id: UUID,
    generation: int,
    start_sample: int,
    source_kind: str,
  ) -> None:
    self._broker = broker
    self._session_id = session_id
    self._generation = generation
    self._initial_sample = start_sample
    self._expected_sample = start_sample
    self._expected_sequence = 0
    self._started = False
    self._opened = False
    self._released = False

    self.source_kind = source_kind

  def await_start(self) -> None:
    """Consume and validate START before the durable session enters running."""

    if self._started:
      return
    try:
      header = self._broker._read_header()
      self._validate_identity_and_sequence(header)
      if header.kind is PcmRecordKind.ERROR:
        raise self._read_host_error(header)
      if header.kind is not PcmRecordKind.START:
        raise HostPcmError("audio_start_required", "The first host PCM record must be START or ERROR")
      self._validate_control_record(header)
      self._started = True
      self._expected_sequence += 1
    except BaseException:
      self._broker._release(self)
      raise

  def open(self) -> AudioStream:
    if self._opened:
      raise HostPcmError("audio_input_reopened", "A host PCM input generation can only be opened once")
    self.await_start()
    self._opened = True
    return AudioStream(
      source=SourceMetadata(kind=self.source_kind),
      chunks=self._chunks(),
      finite=False,
      drain_on_stop=True,
    )

  def _chunks(self) -> Iterator[AudioChunk]:
    partial_frame_seen = False
    terminal_seen = False
    try:
      while True:
        header = self._broker._read_header()
        self._validate_identity_and_sequence(header)
        if partial_frame_seen and header.kind is not PcmRecordKind.END:
          raise HostPcmError(
            "audio_data_after_partial_frame",
            "Host PCM data followed a final partial frame instead of END",
          )
        if header.kind is PcmRecordKind.DATA:
          samples = self._read_data(header)
          partial_frame_seen = header.sample_count < VAD_FRAME_SAMPLES
          self._expected_sequence += 1
          self._expected_sample += header.sample_count
          yield AudioChunk(samples=samples, sample_rate=SAMPLE_RATE, start_sample=header.start_sample)
          continue
        if header.kind is PcmRecordKind.END:
          self._validate_control_record(header)
          self._expected_sequence += 1
          terminal_seen = True
          return
        if header.kind is PcmRecordKind.ERROR:
          error = self._read_host_error(header)
          terminal_seen = True
          raise error
        raise HostPcmError("audio_unexpected_start", "Host PCM START may only appear once per generation")
    finally:
      if terminal_seen:
        self._broker._release(self)

  def drain_and_release(self) -> None:
    """Drain after the host has been told to stop, then release stream ownership."""

    if self._released:
      return
    try:
      while True:
        header = self._broker._read_header()
        self._validate_identity_and_sequence(header)
        if header.kind is PcmRecordKind.DATA:
          self._read_data(header)
          self._expected_sample += header.sample_count
          self._expected_sequence += 1
          continue
        if header.kind is PcmRecordKind.ERROR:
          self._read_host_error(header)
          return
        if header.kind is PcmRecordKind.END:
          self._validate_control_record(header)
          return
        raise HostPcmError("audio_unexpected_start", "Host PCM START may only appear once per generation")
    except HostPcmError:
      pass
    finally:
      self._broker._release(self)

  def _validate_identity_and_sequence(self, header: PcmHeader) -> None:
    if header.session_id != self._session_id:
      raise HostPcmError("audio_session_mismatch", "Host PCM record belongs to a different session")
    if header.generation != self._generation:
      raise HostPcmError("audio_generation_mismatch", "Host PCM record belongs to a different generation")
    if header.sequence != self._expected_sequence:
      raise HostPcmError(
        "audio_sequence_mismatch",
        f"Host PCM sequence must be {self._expected_sequence}, got {header.sequence}",
      )
    if header.start_sample != self._expected_sample:
      raise HostPcmError(
        "audio_sample_gap",
        f"Host PCM start sample must be {self._expected_sample}, got {header.start_sample}",
      )
    if header.flags != 0:
      raise HostPcmError("audio_unsupported_flags", f"Host PCM record has unsupported flags: {header.flags}")

  @staticmethod
  def _validate_format(header: PcmHeader) -> None:
    if header.sample_rate != SAMPLE_RATE:
      raise HostPcmError("audio_sample_rate_mismatch", f"Host PCM sample rate must be {SAMPLE_RATE} Hz")
    if header.channels != 1:
      raise HostPcmError("audio_channel_mismatch", "Host PCM must contain exactly one channel")
    if header.sample_format != PCM_SAMPLE_FORMAT_F32LE:
      raise HostPcmError("audio_format_mismatch", "Host PCM must use f32 little-endian samples")

  def _validate_control_record(self, header: PcmHeader) -> None:
    self._validate_format(header)
    if header.sample_count != 0 or header.payload_length != 0:
      raise HostPcmError("audio_invalid_control_record", "Host PCM control records must have an empty payload")

  def _read_data(self, header: PcmHeader) -> np.ndarray:
    self._validate_format(header)
    if not 1 <= header.sample_count <= VAD_FRAME_SAMPLES:
      raise HostPcmError(
        "audio_invalid_sample_count",
        f"Host PCM DATA must contain between 1 and {VAD_FRAME_SAMPLES} samples",
      )
    expected_bytes = header.sample_count * np.dtype("<f4").itemsize
    if header.payload_length != expected_bytes:
      raise HostPcmError(
        "audio_payload_length_mismatch",
        f"Host PCM DATA payload must contain {expected_bytes} bytes",
      )
    payload = self._broker._read_exactly(header.payload_length)
    return np.frombuffer(payload, dtype="<f4").astype(np.float32, copy=True)

  def _read_host_error(self, header: PcmHeader) -> HostPcmError:
    self._validate_format(header)
    if header.sample_count != 0 or not 1 <= header.payload_length <= PCM_ERROR_MAX_BYTES:
      raise HostPcmError("audio_invalid_error_record", "Host PCM ERROR payload has an invalid length")
    payload = self._broker._read_exactly(header.payload_length)
    try:
      value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
      raise HostPcmError("audio_invalid_error_record", "Host PCM ERROR payload must be UTF-8 JSON") from exc
    if not isinstance(value, dict):
      raise HostPcmError("audio_invalid_error_record", "Host PCM ERROR payload must be an object")
    code = value.get("code")
    message = value.get("message")
    if not isinstance(code, str) or not code.strip() or not isinstance(message, str) or not message.strip():
      raise HostPcmError("audio_invalid_error_record", "Host PCM ERROR requires non-empty code and message")
    return HostPcmError(code, message)

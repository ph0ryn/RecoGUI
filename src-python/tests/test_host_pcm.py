from __future__ import annotations

import io
import json
from collections.abc import Generator
from typing import cast
from uuid import UUID, uuid4

import numpy as np
import pytest

from reco.host_pcm import (
  PCM_HEADER,
  HostPcmBroker,
  HostPcmError,
  PcmRecordKind,
)


def record(
  kind: PcmRecordKind,
  session_id: UUID,
  *,
  samples: np.ndarray | None = None,
  error: dict[str, object] | None = None,
  overrides: dict[str, object] | None = None,
  **fields: object,
) -> bytes:
  values = dict(fields)
  values.update(overrides or {})
  if samples is not None:
    payload = np.asarray(samples, dtype="<f4").tobytes()
    count = samples.size
  elif error is not None:
    payload = json.dumps(error, separators=(",", ":")).encode()
    count = 0
  else:
    payload = b""
    count = 0
  sample_count = values.get("sample_count")
  payload_length = values.get("payload_length")
  header = PCM_HEADER.pack(
    cast(bytes, values.get("magic", b"RPCM")),
    int(cast(int, values.get("version", 1))),
    int(kind),
    session_id.bytes,
    int(cast(int, values.get("generation", 0))),
    int(cast(int, values.get("sequence", 0))),
    int(cast(int, values.get("start_sample", 0))),
    int(cast(int, values.get("sample_rate", 16_000))),
    int(cast(int, values.get("channels", 1))),
    int(cast(int, values.get("sample_format", 1))),
    count if sample_count is None else int(cast(int, sample_count)),
    len(payload) if payload_length is None else int(cast(int, payload_length)),
    int(cast(int, values.get("flags", 0))),
  )
  return header + payload


class FragmentedReader(io.BytesIO):
  def read(self, size: int | None = -1) -> bytes:
    limit = -1 if size is None else size
    return super().read(min(limit, 3))


def test_host_pcm_stream_accepts_fragmented_headers_full_frames_and_terminal_tail() -> None:
  session_id = uuid4()
  full = np.linspace(-1, 1, 512, dtype=np.float32)
  tail = np.array([0.25, -0.5, 0.75], dtype=np.float32)
  source = FragmentedReader(
    record(PcmRecordKind.START, session_id)
    + record(PcmRecordKind.DATA, session_id, sequence=1, samples=full)
    + record(PcmRecordKind.DATA, session_id, sequence=2, start_sample=512, samples=tail)
    + record(PcmRecordKind.END, session_id, sequence=3, start_sample=515)
  )
  broker = HostPcmBroker(source)
  audio_input = broker.input(str(session_id), 0, 0, "systemAudio")

  audio_input.await_start()
  stream = audio_input.open()
  chunks = list(stream.chunks)

  assert stream.source.kind == "systemAudio"
  assert stream.finite is False
  assert stream.drain_on_stop is True
  assert [chunk.start_sample for chunk in chunks] == [0, 512]
  np.testing.assert_array_equal(chunks[0].samples, full)
  np.testing.assert_array_equal(chunks[1].samples, tail)


@pytest.mark.parametrize(
  ("replacement", "code"),
  [
    ({"magic": b"FAIL"}, "audio_invalid_magic"),
    ({"version": 2}, "audio_unsupported_wire_version"),
    ({"sample_rate": 48_000}, "audio_sample_rate_mismatch"),
    ({"channels": 2}, "audio_channel_mismatch"),
    ({"sample_format": 9}, "audio_format_mismatch"),
    ({"flags": 1}, "audio_unsupported_flags"),
    ({"sample_count": 1}, "audio_invalid_control_record"),
  ],
)
def test_start_record_rejects_invalid_contract(replacement: dict[str, object], code: str) -> None:
  session_id = uuid4()
  source = io.BytesIO(record(PcmRecordKind.START, session_id, overrides=replacement))
  audio_input = HostPcmBroker(source).input(str(session_id), 0, 0, "microphone")

  with pytest.raises(HostPcmError) as failure:
    audio_input.await_start()

  assert failure.value.code == code


@pytest.mark.parametrize(
  ("record_options", "code"),
  [
    ({"sequence": 2}, "audio_sequence_mismatch"),
    ({"start_sample": 1}, "audio_sample_gap"),
    ({"generation": 1}, "audio_generation_mismatch"),
    ({"payload_length": 8}, "audio_payload_length_mismatch"),
    ({"sample_count": 513}, "audio_invalid_sample_count"),
  ],
)
def test_data_record_rejects_order_identity_and_payload_mismatches(
  record_options: dict[str, object], code: str
) -> None:
  session_id = uuid4()
  stream = record(PcmRecordKind.START, session_id) + record(
    PcmRecordKind.DATA,
    session_id,
    samples=np.zeros(1, dtype=np.float32),
    sequence=1,
    overrides=record_options,
  )
  audio_input = HostPcmBroker(io.BytesIO(stream)).input(str(session_id), 0, 0, "microphone")

  with pytest.raises(HostPcmError) as failure:
    list(audio_input.open().chunks)

  assert failure.value.code == code


def test_record_from_another_session_is_rejected() -> None:
  expected = uuid4()
  source = io.BytesIO(record(PcmRecordKind.START, uuid4()))

  with pytest.raises(HostPcmError) as failure:
    HostPcmBroker(source).input(str(expected), 0, 0, "microphone").await_start()

  assert failure.value.code == "audio_session_mismatch"


def test_data_after_partial_frame_is_rejected() -> None:
  session_id = uuid4()
  source = io.BytesIO(
    record(PcmRecordKind.START, session_id)
    + record(PcmRecordKind.DATA, session_id, sequence=1, samples=np.zeros(10, dtype=np.float32))
    + record(PcmRecordKind.DATA, session_id, sequence=2, start_sample=10, samples=np.zeros(1, dtype=np.float32))
  )

  with pytest.raises(HostPcmError) as failure:
    list(HostPcmBroker(source).input(str(session_id), 0, 0, "microphone").open().chunks)

  assert failure.value.code == "audio_data_after_partial_frame"


def test_host_error_preserves_stable_code_and_message() -> None:
  session_id = uuid4()
  source = io.BytesIO(
    record(PcmRecordKind.START, session_id)
    + record(
      PcmRecordKind.ERROR,
      session_id,
      sequence=1,
      error={"code": "capture_overflow", "message": "Capture buffer overflowed"},
    )
  )

  with pytest.raises(HostPcmError, match="Capture buffer overflowed") as failure:
    list(HostPcmBroker(source).input(str(session_id), 0, 0, "microphone").open().chunks)

  assert failure.value.code == "capture_overflow"


def test_partial_transport_eof_has_a_stable_error() -> None:
  session_id = uuid4()
  audio_input = HostPcmBroker(io.BytesIO(record(PcmRecordKind.START, session_id)[:10])).input(
    str(session_id), 0, 0, "microphone"
  )

  with pytest.raises(HostPcmError) as failure:
    audio_input.await_start()

  assert failure.value.code == "audio_transport_eof"


def test_early_close_requires_explicit_drain_before_the_broker_can_be_reused() -> None:
  session_id = uuid4()
  next_session_id = uuid4()
  source = io.BytesIO(
    record(PcmRecordKind.START, session_id)
    + record(PcmRecordKind.DATA, session_id, sequence=1, samples=np.zeros(512, dtype=np.float32))
    + record(PcmRecordKind.END, session_id, sequence=2, start_sample=512)
    + record(PcmRecordKind.START, next_session_id)
    + record(PcmRecordKind.END, next_session_id, sequence=1)
  )
  broker = HostPcmBroker(source)
  first = broker.input(str(session_id), 0, 0, "microphone")
  chunks = first.open().chunks
  next(chunks)
  cast(Generator[object, None, None], chunks).close()

  with pytest.raises(HostPcmError) as busy:
    broker.input(str(next_session_id), 0, 0, "microphone")
  assert busy.value.code == "audio_transport_busy"

  first.drain_and_release()
  assert list(broker.input(str(next_session_id), 0, 0, "microphone").open().chunks) == []

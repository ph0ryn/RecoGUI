from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import cast

import numpy as np
import pytest
import soundfile as sf
import soxr

from reco.audio import (
  AUDIO_QUEUE_MAX_FRAMES,
  SAMPLE_RATE,
  VAD_FRAME_SAMPLES,
  AudioChunk,
  AudioFileIdentity,
  LocalAudioFileInput,
  MicrophoneInput,
  SourceMetadata,
  normalize_channels,
  resolve_microphone_device_name,
  validate_audio_file,
)
from reco.errors import RecoError


def test_validate_audio_file_rejects_missing_file(tmp_path: Path) -> None:
  with pytest.raises(RecoError, match="does not exist"):
    validate_audio_file(tmp_path / "missing.wav")


def test_audio_domain_objects_reject_invalid_stream_metadata() -> None:
  with pytest.raises(ValueError, match="source kind"):
    SourceMetadata(kind="")
  with pytest.raises(ValueError, match="sample rate"):
    AudioChunk(samples=np.ones(10, dtype=np.float32), sample_rate=0, start_sample=0)
  with pytest.raises(ValueError, match="start sample"):
    AudioChunk(samples=np.ones(10, dtype=np.float32), sample_rate=SAMPLE_RATE, start_sample=-1)
  with pytest.raises(ValueError, match="non-empty mono"):
    AudioChunk(samples=np.array([], dtype=np.float32), sample_rate=SAMPLE_RATE, start_sample=0)


def test_input_adapters_reject_incompatible_frame_contracts(tmp_path: Path) -> None:
  for invalid_size in (VAD_FRAME_SAMPLES // 2, VAD_FRAME_SAMPLES * 2):
    with pytest.raises(ValueError, match="exactly 512"):
      LocalAudioFileInput(tmp_path / "audio.wav", frame_samples=invalid_size)
    with pytest.raises(ValueError, match="exactly 512"):
      MicrophoneInput(frame_samples=invalid_size)
  with pytest.raises(ValueError, match="internal"):
    MicrophoneInput(sample_rate=44_100)


def test_validate_audio_file_rejects_unsupported_extension(tmp_path: Path) -> None:
  path = tmp_path / "audio.txt"
  path.write_text("not audio")

  with pytest.raises(RecoError, match="Unsupported audio extension"):
    validate_audio_file(path)


def test_validate_audio_file_rejects_corrupt_supported_file(tmp_path: Path) -> None:
  path = tmp_path / "audio.wav"
  path.write_text("not audio")

  with pytest.raises(RecoError, match="Could not read audio file"):
    validate_audio_file(path)


def test_normalize_channels_converts_stereo_to_mono() -> None:
  stereo = np.array([[1.0, -1.0], [0.5, 0.25]], dtype=np.float32)

  mono = normalize_channels(stereo)

  np.testing.assert_allclose(mono, np.array([0.0, 0.375], dtype=np.float32))


def test_local_audio_file_input_streams_resampled_frames_without_losing_tail(tmp_path: Path) -> None:
  path = tmp_path / "tone.wav"
  sf.write(path, np.zeros(4_410, dtype=np.float32), 44_100, format="WAV")

  stream = LocalAudioFileInput(path).open()
  chunks = list(stream.chunks)

  assert all(chunk.sample_rate == SAMPLE_RATE for chunk in chunks)
  assert [chunk.start_sample for chunk in chunks] == [0, 512, 1_024, 1_536]
  assert 1_590 <= sum(chunk.samples.size for chunk in chunks) <= 1_610


def test_streaming_resampler_preserves_nonzero_signal_values(tmp_path: Path) -> None:
  path = tmp_path / "signal.wav"
  signal = np.linspace(-0.75, 0.75, 4_410, dtype=np.float32)
  sf.write(path, signal, 44_100, format="WAV", subtype="FLOAT")

  chunks = list(LocalAudioFileInput(path).open().chunks)
  streamed = np.concatenate([chunk.samples for chunk in chunks])
  expected = soxr.resample(signal, 44_100, SAMPLE_RATE).astype(np.float32)

  assert streamed.size == expected.size
  np.testing.assert_allclose(streamed, expected, rtol=1e-5, atol=2e-6)


def test_resampler_initialization_failure_uses_the_cli_error_contract(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  path = tmp_path / "signal.wav"
  sf.write(path, np.zeros(100, dtype=np.float32), 44_100, format="WAV")

  def fail_resampler(*args: object, **kwargs: object) -> None:
    del args, kwargs
    raise ValueError("unsupported resampler")

  monkeypatch.setattr("reco.audio.soxr.ResampleStream", fail_resampler)

  with pytest.raises(RecoError, match="Could not read audio file"):
    list(LocalAudioFileInput(path).open().chunks)


def test_recorded_file_identity_rejects_path_replacement_before_streaming(tmp_path: Path) -> None:
  path = tmp_path / "signal.wav"
  sf.write(path, np.zeros(512, dtype=np.float32), SAMPLE_RATE, format="WAV")
  expected_identity = AudioFileIdentity.from_stat(path.stat())
  path.unlink()
  sf.write(path, np.ones(512, dtype=np.float32), SAMPLE_RATE, format="WAV")

  with pytest.raises(RecoError, match="changed after it was fingerprinted"):
    list(LocalAudioFileInput(path, expected_identity=expected_identity).open().chunks)


def test_local_audio_file_input_streams_frames_without_losing_tail(tmp_path: Path) -> None:
  path = tmp_path / "tone.wav"
  sf.write(path, np.zeros(VAD_FRAME_SAMPLES + 10, dtype=np.float32), SAMPLE_RATE, format="WAV")

  stream = LocalAudioFileInput(path).open()
  chunks = list(stream.chunks)

  assert stream.finite is True
  assert chunks[0].start_sample == 0
  assert chunks[0].samples.size == VAD_FRAME_SAMPLES
  assert chunks[1].start_sample == VAD_FRAME_SAMPLES
  assert chunks[1].samples.size == 10


def test_microphone_input_source_uses_input_device_name(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("reco.audio.resolve_microphone_device_name", lambda device=None: "Studio Mic")

  stream = MicrophoneInput().open()

  assert stream.source.kind == "microphone"
  assert stream.source.path == "Studio Mic"


def test_resolve_microphone_device_name_queries_default_input(monkeypatch: pytest.MonkeyPatch) -> None:
  calls: list[tuple[int | str | None, str]] = []

  def fake_query_devices(device: int | str | None = None, kind: str | None = None) -> dict[str, object]:
    assert kind is not None
    calls.append((device, kind))
    return {"name": "MacBook Pro Microphone"}

  monkeypatch.setattr("reco.audio.sd.query_devices", fake_query_devices)

  assert resolve_microphone_device_name() == "MacBook Pro Microphone"
  assert calls == [(None, "input")]


def test_microphone_callback_reports_bounded_buffer_overflow_without_blocking(
  monkeypatch: pytest.MonkeyPatch,
) -> None:
  class FloodingInputStream:
    def __init__(self, **kwargs: object) -> None:
      self.callback = cast(Callable[..., None], kwargs["callback"])

    def __enter__(self) -> FloodingInputStream:
      assert callable(self.callback)
      for _ in range(AUDIO_QUEUE_MAX_FRAMES + 1):
        self.callback(np.zeros((VAD_FRAME_SAMPLES, 1), dtype=np.float32), VAD_FRAME_SAMPLES, None, 0)
      return self

    def __exit__(self, *args: object) -> None:
      del args

  monkeypatch.setattr("reco.audio.sd.InputStream", FloodingInputStream)
  stream = MicrophoneInput().open()

  with pytest.raises(RecoError, match="bounded"):
    next(stream.chunks)


def test_microphone_callback_rejects_unexpected_frame_sizes(monkeypatch: pytest.MonkeyPatch) -> None:
  class ShortInputStream:
    def __init__(self, **kwargs: object) -> None:
      self.callback = cast(Callable[..., None], kwargs["callback"])

    def __enter__(self) -> ShortInputStream:
      samples = np.zeros((VAD_FRAME_SAMPLES // 2, 1), dtype=np.float32)
      self.callback(samples, samples.size, None, 0)
      return self

    def __exit__(self, *args: object) -> None:
      del args

  monkeypatch.setattr("reco.audio.sd.InputStream", ShortInputStream)

  with pytest.raises(RecoError, match="returned 256 samples"):
    next(MicrophoneInput().open().chunks)

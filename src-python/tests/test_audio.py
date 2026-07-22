from __future__ import annotations

from pathlib import Path

import numpy as np
import pytest
import soundfile as sf
import soxr

from reco.audio import (
  SAMPLE_RATE,
  VAD_FRAME_SAMPLES,
  AudioChunk,
  AudioFileIdentity,
  LocalAudioFileInput,
  SourceMetadata,
  audio_file_duration_ms,
  normalize_channels,
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


def test_audio_file_duration_uses_source_metadata(tmp_path: Path) -> None:
  path = tmp_path / "tone.wav"
  sf.write(path, np.zeros(44_100, dtype=np.float32), 44_100, format="WAV")

  assert audio_file_duration_ms(path) == 1_000


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


def test_local_audio_file_input_resumes_at_a_frame_checkpoint(tmp_path: Path) -> None:
  path = tmp_path / "tone.wav"
  signal = np.arange(VAD_FRAME_SAMPLES * 3, dtype=np.float32) / 10_000
  sf.write(path, signal, SAMPLE_RATE, format="WAV", subtype="FLOAT")

  chunks = list(LocalAudioFileInput(path, start_sample=VAD_FRAME_SAMPLES).open().chunks)

  assert [chunk.start_sample for chunk in chunks] == [VAD_FRAME_SAMPLES, VAD_FRAME_SAMPLES * 2]
  np.testing.assert_allclose(chunks[0].samples, signal[VAD_FRAME_SAMPLES : VAD_FRAME_SAMPLES * 2])


def test_local_audio_file_input_resumes_at_an_unaligned_checkpoint_without_losing_samples(tmp_path: Path) -> None:
  path = tmp_path / "tone.wav"
  signal = np.arange(VAD_FRAME_SAMPLES * 3, dtype=np.float32) / 10_000
  sf.write(path, signal, SAMPLE_RATE, format="WAV", subtype="FLOAT")
  checkpoint = VAD_FRAME_SAMPLES + 123

  chunks = list(LocalAudioFileInput(path, start_sample=checkpoint).open().chunks)

  assert [chunk.start_sample for chunk in chunks] == [checkpoint, checkpoint + VAD_FRAME_SAMPLES]
  assert [chunk.samples.size for chunk in chunks] == [VAD_FRAME_SAMPLES, VAD_FRAME_SAMPLES - 123]
  np.testing.assert_allclose(np.concatenate([chunk.samples for chunk in chunks]), signal[checkpoint:])

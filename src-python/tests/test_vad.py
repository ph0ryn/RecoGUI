from __future__ import annotations

import hashlib
from collections import deque
from pathlib import Path

import numpy as np
import pytest

from reco.audio import SAMPLE_RATE, VAD_FRAME_SAMPLES, AudioChunk
from reco.config import VadConfig
from reco.errors import RecoError
from reco.models import SpeechSegment, SplitReason
from reco.vad import (
  SILERO_VAD_SHA256,
  OnnxSileroProbabilityModel,
  SileroVadEngine,
  pad_frame,
  validate_silero_vad_asset,
)


class ProbabilityModel:
  def __init__(self, probabilities: list[float]) -> None:
    self.probabilities = deque(probabilities)
    self.reset_count = 0

  def reset_states(self) -> None:
    self.reset_count += 1

  def __call__(self, samples: np.ndarray, sample_rate: int) -> float:
    assert samples.shape == (VAD_FRAME_SAMPLES,)
    assert sample_rate == SAMPLE_RATE
    return self.probabilities.popleft()


class FailingProbabilityModel(ProbabilityModel):
  def __call__(self, samples: np.ndarray, sample_rate: int) -> float:
    del samples, sample_rate
    raise RuntimeError("VAD exploded")


class FakeOnnxSession:
  def __init__(self, outputs: list[float]) -> None:
    self.outputs = deque(outputs)
    self.inputs: list[dict[str, np.ndarray]] = []

  def run(self, output_names: object, inputs: dict[str, np.ndarray]) -> tuple[np.ndarray, np.ndarray]:
    assert output_names is None
    self.inputs.append(inputs)
    state = np.full((2, 1, 128), len(self.inputs), dtype=np.float32)
    return np.array([[self.outputs.popleft()]], dtype=np.float32), state


def frame(index: int, *, size: int = VAD_FRAME_SAMPLES) -> AudioChunk:
  return AudioChunk(
    samples=np.full(size, index + 1, dtype=np.float32),
    sample_rate=SAMPLE_RATE,
    start_sample=index * VAD_FRAME_SAMPLES,
  )


def process(engine: SileroVadEngine, frame_count: int) -> list[SpeechSegment]:
  segments: list[SpeechSegment] = []
  for index in range(frame_count):
    segments.extend(engine.process_frame(frame(index)))
  return segments


def test_pad_frame_preserves_observed_samples_and_pads_only_for_inference() -> None:
  samples = np.arange(10, dtype=np.float32)

  padded = pad_frame(samples)

  assert padded.shape == (VAD_FRAME_SAMPLES,)
  np.testing.assert_array_equal(padded[: samples.size], samples)
  np.testing.assert_array_equal(padded[samples.size :], 0)


def test_onnx_wrapper_preserves_silero_context_state_and_numerical_tolerance() -> None:
  session = FakeOnnxSession([0.3141592, 0.2718282])
  model = OnnxSileroProbabilityModel(Path("unused.onnx"), session=session)
  samples = np.arange(VAD_FRAME_SAMPLES, dtype=np.float32) / VAD_FRAME_SAMPLES

  actual = np.array([model(samples, SAMPLE_RATE), model(samples, SAMPLE_RATE)])

  expected = np.array([0.3141592, 0.2718282])
  assert np.max(np.abs(actual - expected)) <= 1e-5
  assert session.inputs[0]["input"].shape == (1, 576)
  np.testing.assert_array_equal(session.inputs[1]["input"][:, :64], samples[-64:].reshape(1, -1))
  np.testing.assert_array_equal(session.inputs[1]["state"], np.ones((2, 1, 128), dtype=np.float32))

  model.reset_states()
  np.testing.assert_array_equal(model._state, np.zeros((2, 1, 128), dtype=np.float32))


def test_bundled_onnx_asset_has_the_expected_hash() -> None:
  asset = Path(__file__).resolve().parents[1] / "assets" / "silero_vad.onnx"

  assert validate_silero_vad_asset(asset) == asset
  assert hashlib.sha256(asset.read_bytes()).hexdigest() == SILERO_VAD_SHA256


def test_invalid_onnx_asset_is_rejected(tmp_path: Path) -> None:
  asset = tmp_path / "silero_vad.onnx"
  asset.write_bytes(b"invalid")

  with pytest.raises(RecoError, match="SHA-256"):
    validate_silero_vad_asset(asset)


def test_silence_boundary_includes_exact_pre_and_post_roll() -> None:
  model = ProbabilityModel([0.0, 0.0, 0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=64,
      min_speech_duration_ms=32,
      speech_pad_ms=64,
      target_segment_duration_ms=1_000,
      max_segment_duration_ms=2_000,
    ),
  )
  engine.reset()

  segments = process(engine, 6)

  assert len(segments) == 1
  segment = segments[0]
  assert segment.split_reason is SplitReason.SILENCE
  assert segment.start_sample == 0
  assert segment.end_sample == 6 * VAD_FRAME_SAMPLES
  assert segment.audio.size == segment.end_sample - segment.start_sample
  assert segment.audio[0] == 1
  assert segment.audio[-1] == 6
  assert segment.vad.peak_probability == pytest.approx(0.9)


def test_speech_starting_at_sample_zero_is_not_treated_as_missing() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=64,
      min_speech_duration_ms=32,
      speech_pad_ms=0,
      target_segment_duration_ms=1_000,
      max_segment_duration_ms=2_000,
    ),
  )
  engine.reset()

  segments = process(engine, 4)

  assert len(segments) == 1
  assert segments[0].start_sample == 0
  assert segments[0].end_sample == 2 * VAD_FRAME_SAMPLES


def test_adaptive_split_carries_every_sample_without_gap_or_overlap() -> None:
  model = ProbabilityModel([0.9] * 5)
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_speech_duration_ms=32,
      speech_pad_ms=0,
      target_segment_duration_ms=64,
      max_segment_duration_ms=96,
    ),
  )
  engine.reset()

  segments = process(engine, 5)
  segments.extend(engine.flush(finalize_open_segment=True))

  assert [segment.split_reason for segment in segments] == [
    SplitReason.ADAPTIVE_SPLIT,
    SplitReason.END_OF_INPUT,
  ]
  assert [(segment.start_sample, segment.end_sample) for segment in segments] == [
    (0, 3 * VAD_FRAME_SAMPLES),
    (3 * VAD_FRAME_SAMPLES, 5 * VAD_FRAME_SAMPLES),
  ]
  assert sum(segment.audio.size for segment in segments) == 5 * VAD_FRAME_SAMPLES


def test_adaptive_split_uses_the_first_low_probability_boundary_after_target() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.2, 0.9])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_speech_duration_ms=32,
      speech_pad_ms=0,
      target_segment_duration_ms=64,
      max_segment_duration_ms=128,
    ),
  )
  engine.reset()

  segments = process(engine, 4)
  segments.extend(engine.flush(finalize_open_segment=True))

  expected_split = 2 * VAD_FRAME_SAMPLES + VAD_FRAME_SAMPLES // 2
  assert segments[0].end_sample == expected_split
  assert segments[1].start_sample == expected_split
  assert segments[0].audio.size + segments[1].audio.size == 4 * VAD_FRAME_SAMPLES


def test_low_probability_boundary_never_exceeds_a_non_aligned_hard_limit() -> None:
  model = ProbabilityModel([0.9, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_speech_duration_ms=1,
      speech_pad_ms=0,
      target_segment_duration_ms=45,
      max_segment_duration_ms=45,
    ),
  )
  engine.reset()

  segments = process(engine, 2)

  assert len(segments) == 1
  assert segments[0].split_reason is SplitReason.ADAPTIVE_SPLIT
  assert segments[0].end_sample == 720


def test_late_silence_after_forced_split_never_emits_zero_length_segment() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=64,
      min_speech_duration_ms=32,
      speech_pad_ms=0,
      target_segment_duration_ms=64,
      max_segment_duration_ms=64,
    ),
  )
  engine.reset()

  segments = process(engine, 4)
  segments.extend(engine.flush(finalize_open_segment=True))

  assert len(segments) == 1
  assert segments[0].duration_ms > 0
  assert segments[0].audio.size == 2 * VAD_FRAME_SAMPLES


def test_confirmed_adaptive_continuation_keeps_a_short_final_tail() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=64,
      min_speech_duration_ms=64,
      speech_pad_ms=0,
      target_segment_duration_ms=96,
      max_segment_duration_ms=96,
    ),
  )
  engine.reset()

  segments = process(engine, 6)

  assert [segment.split_reason for segment in segments] == [
    SplitReason.ADAPTIVE_SPLIT,
    SplitReason.SILENCE,
  ]
  assert [(segment.start_sample, segment.end_sample) for segment in segments] == [
    (0, 3 * VAD_FRAME_SAMPLES),
    (3 * VAD_FRAME_SAMPLES, 4 * VAD_FRAME_SAMPLES),
  ]


def test_adaptive_split_does_not_emit_a_silence_only_continuation() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=64,
      min_speech_duration_ms=64,
      speech_pad_ms=0,
      target_segment_duration_ms=96,
      max_segment_duration_ms=96,
    ),
  )
  engine.reset()

  segments = process(engine, 5)

  assert len(segments) == 1
  assert segments[0].split_reason is SplitReason.ADAPTIVE_SPLIT
  assert segments[0].end_sample == 3 * VAD_FRAME_SAMPLES


def test_terminal_spike_shorter_than_minimum_speech_is_discarded() -> None:
  model = ProbabilityModel([0.9])
  engine = SileroVadEngine(model=model)
  engine.reset()

  assert process(engine, 1) == []
  assert engine.flush(finalize_open_segment=True) == []


def test_terminal_silence_does_not_make_a_short_spike_pass_minimum_speech() -> None:
  model = ProbabilityModel([0.9, 0.0, 0.0, 0.0, 0.0])
  engine = SileroVadEngine(model=model)
  engine.reset()

  assert process(engine, 5) == []
  assert engine.flush(finalize_open_segment=True) == []


def test_incomplete_terminal_silence_is_trimmed_to_trailing_padding() -> None:
  model = ProbabilityModel([0.9, 0.9, 0.9, 0.0, 0.0])
  engine = SileroVadEngine(
    model=model,
    config=VadConfig(
      min_silence_duration_ms=800,
      min_speech_duration_ms=32,
      speech_pad_ms=32,
    ),
  )
  engine.reset()

  assert process(engine, 5) == []
  segments = engine.flush(finalize_open_segment=True)

  assert len(segments) == 1
  assert segments[0].split_reason is SplitReason.END_OF_INPUT
  assert segments[0].end_sample == 4 * VAD_FRAME_SAMPLES
  assert segments[0].audio.size == 4 * VAD_FRAME_SAMPLES


def test_partial_eof_is_not_extended_by_vad_padding() -> None:
  model = ProbabilityModel([0.9])
  engine = SileroVadEngine(model=model, config=VadConfig(min_speech_duration_ms=1))
  engine.reset()
  chunk = AudioChunk(samples=np.ones(20, dtype=np.float32), sample_rate=SAMPLE_RATE, start_sample=0)

  assert engine.process_frame(chunk) == []
  segments = engine.flush(finalize_open_segment=True)

  assert len(segments) == 1
  assert segments[0].start_sample == 0
  assert segments[0].end_sample == 20
  assert segments[0].audio.size == 20


def test_partial_frame_must_be_the_last_frame_in_the_stream() -> None:
  model = ProbabilityModel([0.0])
  engine = SileroVadEngine(model=model)
  engine.reset()
  engine.process_frame(AudioChunk(samples=np.zeros(20, dtype=np.float32), sample_rate=SAMPLE_RATE, start_sample=0))

  with pytest.raises(ValueError, match="must be the final frame"):
    engine.process_frame(
      AudioChunk(
        samples=np.zeros(VAD_FRAME_SAMPLES, dtype=np.float32),
        sample_rate=SAMPLE_RATE,
        start_sample=20,
      )
    )


def test_noncontiguous_audio_is_rejected_instead_of_corrupting_timestamps() -> None:
  model = ProbabilityModel([0.0, 0.0])
  engine = SileroVadEngine(model=model)
  engine.reset()
  engine.process_frame(frame(0))

  with pytest.raises(ValueError, match="must be contiguous"):
    engine.process_frame(
      AudioChunk(
        samples=np.zeros(VAD_FRAME_SAMPLES, dtype=np.float32),
        sample_rate=SAMPLE_RATE,
        start_sample=2 * VAD_FRAME_SAMPLES,
      )
    )


def test_vad_inference_failure_uses_the_cli_error_contract() -> None:
  engine = SileroVadEngine(model=FailingProbabilityModel([]))
  engine.reset()

  with pytest.raises(RecoError, match="VAD exploded"):
    engine.process_frame(frame(0))

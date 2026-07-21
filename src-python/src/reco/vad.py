"""Sample-accurate streaming voice activity detection and segmentation."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
from hashlib import sha256
from pathlib import Path
from typing import Any, Protocol

import numpy as np

from reco.audio import SAMPLE_RATE, VAD_FRAME_SAMPLES, AudioChunk, FloatArray, ensure_float32
from reco.config import DEFAULT_VAD_CONFIG, VadConfig
from reco.errors import RecoError
from reco.models import SpeechSegment, SplitReason, VadDiagnostics


class VadEngine(Protocol):
  """Streaming speech segment detector."""

  def reset(self) -> None:
    """Reset all stream state."""

  def process_frame(self, chunk: AudioChunk) -> list[SpeechSegment]:
    """Process one contiguous normalized frame."""

  def flush(self, finalize_open_segment: bool) -> list[SpeechSegment]:
    """Finish the current stream."""


SILERO_VAD_SHA256 = "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3"


class SileroProbabilityModel(Protocol):
  """Minimal NumPy interface shared by ONNX and deterministic test doubles."""

  def __call__(self, samples: FloatArray, sample_rate: int) -> float:
    """Return one speech probability."""

  def reset_states(self) -> None:
    """Reset recurrent inference state."""


class OnnxSileroProbabilityModel:
  """Stateful Silero v6 ONNX wrapper without a Torch runtime dependency."""

  def __init__(self, model_path: Path, *, session: Any | None = None) -> None:
    if session is None:
      try:
        import onnxruntime as ort
      except ImportError as exc:
        raise RecoError("onnxruntime is required for Silero VAD") from exc
      try:
        session = ort.InferenceSession(str(model_path), providers=["CPUExecutionProvider"])
      except Exception as exc:
        raise RecoError(f"Could not load Silero VAD ONNX model: {exc}") from exc
    self._session = session
    self._state = np.zeros((2, 1, 128), dtype=np.float32)
    self._context = np.zeros((1, 64), dtype=np.float32)

  def reset_states(self) -> None:
    self._state = np.zeros((2, 1, 128), dtype=np.float32)
    self._context = np.zeros((1, 64), dtype=np.float32)

  def __call__(self, samples: FloatArray, sample_rate: int) -> float:
    if sample_rate != SAMPLE_RATE or samples.shape != (VAD_FRAME_SAMPLES,):
      raise ValueError(f"Silero VAD requires exactly {VAD_FRAME_SAMPLES} samples at {SAMPLE_RATE} Hz")
    current = np.concatenate((self._context, samples.reshape(1, -1)), axis=1).astype(np.float32, copy=False)
    try:
      output, self._state = self._session.run(
        None,
        {"input": current, "state": self._state, "sr": np.array(sample_rate, dtype=np.int64)},
      )
    except Exception as exc:
      raise RecoError(f"Silero VAD inference failed: {exc}") from exc
    self._context = current[:, -64:]
    return float(np.asarray(output).reshape(-1)[0])


@dataclass(frozen=True)
class _BufferedFrame:
  samples: FloatArray
  start_sample: int
  speech_probability: float

  @property
  def end_sample(self) -> int:
    return self.start_sample + self.samples.size


@dataclass
class SileroVadEngine:
  """Silero probability stream with sample-accurate adaptive segmentation."""

  model: SileroProbabilityModel
  config: VadConfig = DEFAULT_VAD_CONFIG
  _history: deque[_BufferedFrame] = field(default_factory=deque, init=False)
  _active_frames: list[_BufferedFrame] = field(default_factory=list, init=False)
  _active: bool = field(default=False, init=False)
  _active_confirmed: bool = field(default=False, init=False)
  _has_speech_evidence: bool = field(default=False, init=False)
  _speech_started_sample: int | None = field(default=None, init=False)
  _silence_started_sample: int | None = field(default=None, init=False)
  _last_end_sample: int | None = field(default=None, init=False)
  _last_frame_was_partial: bool = field(default=False, init=False)

  def reset(self) -> None:
    reset_states = getattr(self.model, "reset_states", None)
    if callable(reset_states):
      reset_states()
    self._history.clear()
    self._active_frames = []
    self._active = False
    self._active_confirmed = False
    self._has_speech_evidence = False
    self._speech_started_sample = None
    self._silence_started_sample = None
    self._last_end_sample = None
    self._last_frame_was_partial = False

  def process_frame(self, chunk: AudioChunk) -> list[SpeechSegment]:
    if chunk.sample_rate != SAMPLE_RATE:
      raise ValueError(f"VAD requires {SAMPLE_RATE} Hz audio, got {chunk.sample_rate}")
    if chunk.samples.size > VAD_FRAME_SAMPLES:
      raise ValueError(f"VAD frame cannot exceed {VAD_FRAME_SAMPLES} samples")
    if self._last_frame_was_partial:
      raise ValueError("A partial VAD frame must be the final frame in a stream")
    if self._last_end_sample is not None and chunk.start_sample != self._last_end_sample:
      raise ValueError(
        f"VAD audio must be contiguous: expected sample {self._last_end_sample}, got {chunk.start_sample}"
      )

    samples = ensure_float32(chunk.samples)
    self._last_end_sample = chunk.start_sample + samples.size
    self._last_frame_was_partial = samples.size < VAD_FRAME_SAMPLES
    if not samples.size:
      return []

    probability = self._speech_probability(samples)
    frame = _BufferedFrame(samples=samples, start_sample=chunk.start_sample, speech_probability=probability)

    if not self._active:
      self._append_history(frame)
      if probability >= self.config.start_threshold:
        self._start_segment(frame.start_sample)
      return []

    self._active_frames.append(frame)
    if probability >= self.config.end_threshold:
      self._has_speech_evidence = True
    if probability < self.config.end_threshold:
      if self._silence_started_sample is None:
        self._silence_started_sample = frame.start_sample
    elif probability >= self.config.start_threshold:
      self._silence_started_sample = None

    if self._reached_silence(frame.end_sample):
      return self._finish_silence(frame.end_sample)

    active_start = self._active_start_sample
    if active_start is None:
      return []
    frame_midpoint = frame.start_sample + frame.samples.size // 2
    target_sample = active_start + self._target_segment_samples
    if frame_midpoint >= target_sample and probability < self.config.end_threshold:
      split_sample = min(frame_midpoint, active_start + self._max_segment_samples)
      segment = self._finalize(split_sample, SplitReason.ADAPTIVE_SPLIT, keep_active=True)
      return [segment] if segment is not None else []
    if frame.end_sample - active_start >= self._max_segment_samples:
      split_sample = active_start + self._max_segment_samples
      segment = self._finalize(split_sample, SplitReason.ADAPTIVE_SPLIT, keep_active=True)
      return [segment] if segment is not None else []

    return []

  def flush(self, finalize_open_segment: bool) -> list[SpeechSegment]:
    segments: list[SpeechSegment] = []
    if finalize_open_segment and self._active_frames:
      end_sample = self._active_frames[-1].end_sample
      speech_started = self._speech_started_sample
      speech_end = end_sample if self._silence_started_sample is None else self._silence_started_sample
      has_speech = (self._active_confirmed and self._has_speech_evidence) or (
        speech_started is not None and speech_end - speech_started >= self._min_speech_samples
      )
      if has_speech:
        padded_end_sample = (
          end_sample
          if self._silence_started_sample is None
          else min(end_sample, self._silence_started_sample + self._pad_samples)
        )
        segment = self._finalize(padded_end_sample, SplitReason.END_OF_INPUT, keep_active=False)
        if segment is not None:
          segments.append(segment)
    self.reset()
    return segments

  @property
  def _active_start_sample(self) -> int | None:
    return self._active_frames[0].start_sample if self._active_frames else None

  @property
  def _pad_samples(self) -> int:
    return round(self.config.speech_pad_ms * SAMPLE_RATE / 1000)

  @property
  def _min_speech_samples(self) -> int:
    return round(self.config.min_speech_duration_ms * SAMPLE_RATE / 1000)

  @property
  def _min_silence_samples(self) -> int:
    return round(self.config.min_silence_duration_ms * SAMPLE_RATE / 1000)

  @property
  def _target_segment_samples(self) -> int:
    return round(self.config.target_segment_duration_ms * SAMPLE_RATE / 1000)

  @property
  def _max_segment_samples(self) -> int:
    return round(self.config.max_segment_duration_ms * SAMPLE_RATE / 1000)

  def _speech_probability(self, samples: FloatArray) -> float:
    try:
      prediction = self.model(pad_frame(samples), SAMPLE_RATE)
    except Exception as exc:
      raise RecoError(f"Silero VAD inference failed: {exc}") from exc
    return min(1.0, max(0.0, float(prediction)))

  def _append_history(self, frame: _BufferedFrame) -> None:
    self._history.append(frame)
    history_start = frame.end_sample - self._pad_samples - VAD_FRAME_SAMPLES
    while self._history and self._history[0].end_sample <= history_start:
      self._history.popleft()
    if self._history and self._history[0].start_sample < history_start:
      first = self._history.popleft()
      offset = history_start - first.start_sample
      self._history.appendleft(
        _BufferedFrame(
          samples=ensure_float32(first.samples[offset:]),
          start_sample=history_start,
          speech_probability=first.speech_probability,
        )
      )

  def _start_segment(self, speech_started_sample: int) -> None:
    self._active = True
    self._active_confirmed = False
    self._has_speech_evidence = True
    self._active_frames = list(self._history)
    self._history.clear()
    self._speech_started_sample = speech_started_sample
    self._silence_started_sample = None

  def _reached_silence(self, current_end_sample: int) -> bool:
    return (
      self._silence_started_sample is not None
      and current_end_sample - self._silence_started_sample >= self._min_silence_samples
    )

  def _finish_silence(self, current_end_sample: int) -> list[SpeechSegment]:
    if self._silence_started_sample is None:
      return []
    cut_sample = min(current_end_sample, self._silence_started_sample + self._pad_samples)
    speech_started = self._speech_started_sample
    if speech_started is None:
      speech_started = cut_sample
    has_speech = (self._active_confirmed and self._has_speech_evidence) or (
      self._silence_started_sample - speech_started >= self._min_speech_samples
    )
    if not has_speech:
      _, remaining = _partition_frames(self._active_frames, cut_sample)
      self._reset_active(remaining)
      return []
    segment = self._finalize(cut_sample, SplitReason.SILENCE, keep_active=False)
    return [segment] if segment is not None else []

  def _finalize(
    self,
    end_sample: int,
    split_reason: SplitReason,
    *,
    keep_active: bool,
  ) -> SpeechSegment | None:
    left, right = _partition_frames(self._active_frames, end_sample)
    if not left:
      self._reset_active(right)
      return None

    start_sample = left[0].start_sample
    audio = ensure_float32(np.concatenate([frame.samples for frame in left]))
    if not audio.size or end_sample <= start_sample:
      self._reset_active(right)
      return None

    probabilities = np.array([frame.speech_probability for frame in left], dtype=np.float64)
    weights = np.array([frame.samples.size for frame in left], dtype=np.float64)
    speech_samples = sum(frame.samples.size for frame in left if frame.speech_probability >= self.config.end_threshold)
    segment = SpeechSegment(
      start_sample=start_sample,
      audio=audio,
      sample_rate=SAMPLE_RATE,
      split_reason=split_reason,
      vad=VadDiagnostics(
        mean_probability=float(np.average(probabilities, weights=weights)),
        peak_probability=float(probabilities.max(initial=0.0)),
        speech_ratio=speech_samples / audio.size,
      ),
    )

    if keep_active:
      self._active = True
      self._active_confirmed = True
      self._active_frames = right
      self._has_speech_evidence = any(frame.speech_probability >= self.config.end_threshold for frame in right)
      self._speech_started_sample = end_sample
      self._silence_started_sample = _trailing_silence_start(right, self.config.end_threshold)
      self._history.clear()
    else:
      self._reset_active(right)
    return segment

  def _reset_active(self, remaining: list[_BufferedFrame]) -> None:
    self._active = False
    self._active_confirmed = False
    self._has_speech_evidence = False
    self._active_frames = []
    self._speech_started_sample = None
    self._silence_started_sample = None
    self._history.clear()
    for frame in remaining:
      self._append_history(frame)


def _partition_frames(
  frames: list[_BufferedFrame],
  end_sample: int,
) -> tuple[list[_BufferedFrame], list[_BufferedFrame]]:
  left: list[_BufferedFrame] = []
  right: list[_BufferedFrame] = []
  for frame in frames:
    if frame.end_sample <= end_sample:
      left.append(frame)
      continue
    if frame.start_sample >= end_sample:
      right.append(frame)
      continue
    offset = end_sample - frame.start_sample
    left.append(
      _BufferedFrame(
        samples=ensure_float32(frame.samples[:offset]),
        start_sample=frame.start_sample,
        speech_probability=frame.speech_probability,
      )
    )
    right.append(
      _BufferedFrame(
        samples=ensure_float32(frame.samples[offset:]),
        start_sample=end_sample,
        speech_probability=frame.speech_probability,
      )
    )
  return left, right


def validate_silero_vad_asset(path: Path) -> Path:
  """Validate the immutable Silero ONNX resource bundled beside the sidecar."""

  if not path.is_file() or _file_sha256(path) != SILERO_VAD_SHA256:
    raise RecoError("Bundled Silero VAD asset failed SHA-256 verification")
  return path


def _file_sha256(path: Path) -> str:
  digest = sha256()
  with path.open("rb") as source:
    while block := source.read(1024 * 1024):
      digest.update(block)
  return digest.hexdigest()


def _trailing_silence_start(frames: list[_BufferedFrame], threshold: float) -> int | None:
  silence_start: int | None = None
  for frame in frames:
    if frame.speech_probability < threshold:
      if silence_start is None:
        silence_start = frame.start_sample
    else:
      silence_start = None
  return silence_start


def pad_frame(samples: FloatArray) -> FloatArray:
  """Pad a partial finite-input frame to Silero's 512-sample window."""

  if samples.size == VAD_FRAME_SAMPLES:
    return samples
  if samples.size > VAD_FRAME_SAMPLES:
    raise ValueError(f"VAD frame cannot exceed {VAD_FRAME_SAMPLES} samples")
  padded = np.zeros(VAD_FRAME_SAMPLES, dtype=np.float32)
  padded[: samples.size] = samples
  return padded

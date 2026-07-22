"""Input adapters and audio normalization utilities for Reco."""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass
from os import fstat, stat_result
from pathlib import Path
from typing import Any

import numpy as np
import soundfile as sf
import soxr
from numpy.typing import NDArray

from reco.errors import RecoError

SAMPLE_RATE = 16_000
VAD_FRAME_SAMPLES = 512
SUPPORTED_AUDIO_EXTENSIONS = frozenset(
  {
    ".aif",
    ".aiff",
    ".au",
    ".caf",
    ".flac",
    ".mp3",
    ".ogg",
    ".wav",
  }
)

FloatArray = NDArray[np.float32]


@dataclass(frozen=True)
class SourceMetadata:
  """Metadata that identifies an audio source."""

  kind: str
  path: str | None = None

  def __post_init__(self) -> None:
    if not self.kind.strip():
      raise ValueError("Audio source kind must not be empty")
    if self.path is not None and not self.path.strip():
      raise ValueError("Audio source path must not be empty when provided")


@dataclass(frozen=True)
class AudioChunk:
  """A normalized audio chunk with an absolute start sample."""

  samples: FloatArray
  sample_rate: int
  start_sample: int

  def __post_init__(self) -> None:
    if self.sample_rate <= 0:
      raise ValueError("Audio chunk sample rate must be positive")
    if self.start_sample < 0:
      raise ValueError("Audio chunk start sample must not be negative")
    if self.samples.ndim != 1 or not self.samples.size:
      raise ValueError("Audio chunk samples must be non-empty mono audio")


@dataclass(frozen=True)
class AudioFileIdentity:
  """Filesystem identity used to bind a fingerprint to streamed audio."""

  device: int
  inode: int
  size: int
  modified_ns: int

  @classmethod
  def from_stat(cls, value: stat_result) -> AudioFileIdentity:
    return cls(
      device=value.st_dev,
      inode=value.st_ino,
      size=value.st_size,
      modified_ns=value.st_mtime_ns,
    )


@dataclass(frozen=True)
class AudioStream:
  """Audio chunks plus their source metadata."""

  source: SourceMetadata
  chunks: Iterator[AudioChunk]
  finite: bool
  drain_on_stop: bool = False


class AudioInput:
  """Base interface for input adapters and audio normalization."""

  def open(self) -> AudioStream:
    raise NotImplementedError


class LocalAudioFileInput(AudioInput):
  """Finite local audio file input adapter for file transcription."""

  def __init__(
    self,
    path: Path,
    frame_samples: int = VAD_FRAME_SAMPLES,
    expected_identity: AudioFileIdentity | None = None,
    start_sample: int = 0,
  ) -> None:
    if frame_samples != VAD_FRAME_SAMPLES:
      raise ValueError(f"Audio file frame size must be exactly {VAD_FRAME_SAMPLES} samples")
    self.path = path
    self.frame_samples = frame_samples
    self.expected_identity = expected_identity
    if start_sample < 0:
      raise ValueError("Audio file start sample must not be negative")
    self.start_sample = start_sample

  def open(self) -> AudioStream:
    validate_audio_file(self.path)
    chunks = iter_audio_file_frames(
      self.path,
      self.frame_samples,
      expected_identity=self.expected_identity,
      start_sample=self.start_sample,
    )
    return AudioStream(source=SourceMetadata(kind="file", path=str(self.path)), chunks=chunks, finite=True)


def validate_audio_file(path: Path) -> None:
  """Validate local file existence and supported extension."""

  if not path.exists():
    raise RecoError(f"Audio file does not exist: {path}")
  if not path.is_file():
    raise RecoError(f"Audio path is not a file: {path}")
  if path.suffix.lower() not in SUPPORTED_AUDIO_EXTENSIONS:
    supported = ", ".join(sorted(SUPPORTED_AUDIO_EXTENSIONS))
    raise RecoError(f"Unsupported audio extension '{path.suffix}'. Supported extensions: {supported}")
  try:
    info = sf.info(path)
  except RuntimeError as exc:
    raise RecoError(f"Could not read audio file: {path}") from exc
  if info.samplerate <= 0 or info.channels <= 0:
    raise RecoError(f"Audio file has invalid stream metadata: {path}")


def audio_file_duration_ms(path: Path) -> int:
  """Return the source audio duration for finite-file progress reporting."""

  try:
    info = sf.info(path)
  except RuntimeError as exc:
    raise RecoError(f"Could not read audio file: {path}") from exc
  if info.samplerate <= 0 or info.frames < 0:
    raise RecoError(f"Audio file has invalid stream metadata: {path}")
  return round(info.frames * 1_000 / info.samplerate)


def iter_audio_file_frames(
  path: Path,
  frame_samples: int = VAD_FRAME_SAMPLES,
  *,
  expected_identity: AudioFileIdentity | None = None,
  start_sample: int = 0,
) -> Iterator[AudioChunk]:
  """Stream a local audio file as normalized 16 kHz mono frames."""

  try:
    with path.open("rb") as raw_source:
      opened_identity = AudioFileIdentity.from_stat(fstat(raw_source.fileno()))
      if expected_identity is not None and opened_identity != expected_identity:
        raise RecoError(f"Audio file changed after it was fingerprinted: {path}")
      with sf.SoundFile(raw_source) as source:
        source_rate = source.samplerate
        resampler = soxr.ResampleStream(source_rate, SAMPLE_RATE, 1, dtype="float32")
        pending = np.array([], dtype=np.float32)
        cursor_sample = start_sample
        remaining_to_skip = start_sample
        blocks = source.blocks(
          blocksize=max(frame_samples * 16, frame_samples),
          dtype="float32",
          always_2d=True,
        )
        for block in blocks:
          mono = normalize_channels(block)
          resampled = _resample_stream_chunk(resampler, mono, last=False, source_rate=source_rate)
          if remaining_to_skip >= resampled.size:
            remaining_to_skip -= resampled.size
            continue
          if remaining_to_skip:
            resampled = ensure_float32(resampled[remaining_to_skip:])
            remaining_to_skip = 0
          frames, pending, cursor_sample = split_complete_frames(pending, resampled, cursor_sample, frame_samples)
          yield from frames

        flushed = _resample_stream_chunk(
          resampler,
          np.array([], dtype=np.float32),
          last=True,
          source_rate=source_rate,
        )
        if remaining_to_skip >= flushed.size:
          remaining_to_skip -= flushed.size
          flushed = np.array([], dtype=np.float32)
        elif remaining_to_skip:
          flushed = ensure_float32(flushed[remaining_to_skip:])
          remaining_to_skip = 0
        frames, pending, cursor_sample = split_complete_frames(pending, flushed, cursor_sample, frame_samples)
        yield from frames
        if pending.size:
          yield AudioChunk(samples=ensure_float32(pending), sample_rate=SAMPLE_RATE, start_sample=cursor_sample)
      completed_identity = AudioFileIdentity.from_stat(fstat(raw_source.fileno()))
      if expected_identity is not None and completed_identity != expected_identity:
        raise RecoError(f"Audio file changed while it was being transcribed: {path}")
  except (OSError, RuntimeError, ValueError) as exc:
    raise RecoError(f"Could not read audio file: {path}") from exc


def split_complete_frames(
  pending: FloatArray,
  incoming: FloatArray,
  start_sample: int,
  frame_samples: int,
) -> tuple[list[AudioChunk], FloatArray, int]:
  """Split complete frames from pending streaming audio."""

  if frame_samples <= 0:
    raise ValueError("Audio frame size must be positive")
  if start_sample < 0:
    raise ValueError("Audio start sample must not be negative")
  combined = ensure_float32(np.concatenate([pending, incoming])) if pending.size else ensure_float32(incoming)
  cursor = 0
  frames: list[AudioChunk] = []
  while combined.size - cursor >= frame_samples:
    frame = combined[cursor : cursor + frame_samples]
    frames.append(AudioChunk(samples=ensure_float32(frame), sample_rate=SAMPLE_RATE, start_sample=start_sample))
    start_sample += frame.size
    cursor += frame_samples
  return frames, ensure_float32(combined[cursor:]), start_sample


def normalize_channels(samples: NDArray[np.float32] | NDArray[np.float64]) -> FloatArray:
  """Convert audio samples to mono float32."""

  array = np.asarray(samples, dtype=np.float32)
  if array.ndim == 1:
    return array
  if array.ndim == 2:
    return np.mean(array, axis=1, dtype=np.float32)
  raise RecoError(f"Unsupported audio shape: {array.shape}")


def _resample_stream_chunk(
  resampler: soxr.ResampleStream,
  samples: FloatArray,
  *,
  last: bool,
  source_rate: int,
) -> FloatArray:
  if source_rate == SAMPLE_RATE:
    return ensure_float32(samples)
  return ensure_float32(resampler.resample_chunk(samples, last=last))


def ensure_float32(samples: NDArray[Any]) -> FloatArray:
  """Return a contiguous float32 array."""

  return np.ascontiguousarray(samples, dtype=np.float32)

"""Input adapters and audio normalization utilities for Reco."""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass
from os import fstat, stat_result
from pathlib import Path
from queue import Empty, Full, Queue
from typing import Any

import numpy as np
import sounddevice as sd
import soundfile as sf
import soxr
from numpy.typing import NDArray

from reco.core_audio import CoreAudioDevice, list_core_audio_devices
from reco.errors import RecoError

SAMPLE_RATE = 16_000
VAD_FRAME_SAMPLES = 512
AUDIO_QUEUE_MAX_FRAMES = 64
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


class AudioInput:
  """Base interface for input adapters and audio normalization."""

  def open(self) -> AudioStream:
    raise NotImplementedError


@dataclass(frozen=True)
class MicrophoneDevice:
  """Persistent microphone UID resolved to the current PortAudio index."""

  uid: str
  index: int
  name: str
  channels: int
  is_default: bool


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


class MicrophoneInput(AudioInput):
  """Long-running microphone input adapter for realtime CLI sessions."""

  def __init__(
    self,
    sample_rate: int = SAMPLE_RATE,
    frame_samples: int = VAD_FRAME_SAMPLES,
    device: int | str | None = None,
    start_sample: int = 0,
  ) -> None:
    if sample_rate != SAMPLE_RATE:
      raise ValueError(f"Microphone input must use the internal {SAMPLE_RATE} Hz sample rate")
    if frame_samples != VAD_FRAME_SAMPLES:
      raise ValueError(f"Microphone frame size must be exactly {VAD_FRAME_SAMPLES} samples")
    self.sample_rate = sample_rate
    self.frame_samples = frame_samples
    self.device = device
    if start_sample < 0:
      raise ValueError("Microphone start sample must not be negative")
    self.start_sample = start_sample

  def open(self) -> AudioStream:
    return AudioStream(
      source=SourceMetadata(kind="microphone", path=resolve_microphone_device_name(self.device)),
      chunks=self._chunks(),
      finite=False,
    )

  def _chunks(self) -> Iterator[AudioChunk]:
    queue: Queue[FloatArray] = Queue(maxsize=AUDIO_QUEUE_MAX_FRAMES)
    callback_errors: list[RecoError] = []

    def callback(indata: FloatArray, frames: int, time: Any, status: sd.CallbackFlags) -> None:
      del time
      if status:
        if not callback_errors:
          callback_errors.append(RecoError(f"Microphone input error: {status}"))
        return
      mono = normalize_channels(indata)
      if frames != self.frame_samples or mono.size != self.frame_samples:
        if not callback_errors:
          callback_errors.append(
            RecoError(f"Microphone input returned {mono.size} samples for a {self.frame_samples}-sample VAD frame.")
          )
        return
      try:
        queue.put_nowait(mono)
      except Full:
        if not callback_errors:
          callback_errors.append(
            RecoError(f"Microphone input exceeded the bounded {AUDIO_QUEUE_MAX_FRAMES}-frame capture buffer.")
          )

    start_sample = self.start_sample
    try:
      with sd.InputStream(
        samplerate=self.sample_rate,
        blocksize=self.frame_samples,
        channels=1,
        dtype="float32",
        device=self.device,
        callback=callback,
      ):
        while True:
          if callback_errors:
            raise callback_errors[0]
          try:
            item = queue.get(timeout=0.1)
          except Empty:
            continue
          samples = ensure_float32(item)
          yield AudioChunk(samples=samples, sample_rate=self.sample_rate, start_sample=start_sample)
          start_sample += samples.size
    except sd.PortAudioError as exc:
      raise RecoError(f"Microphone input is unavailable: {exc}") from exc


def resolve_microphone_device_name(device: int | str | None = None) -> str | None:
  """Return a human-readable input device name for display metadata."""

  try:
    device_info = sd.query_devices(device=device, kind="input")
  except (ValueError, sd.PortAudioError):
    return str(device) if device is not None else None

  if not isinstance(device_info, dict):
    return str(device) if device is not None else None

  name = device_info.get("name")
  resolved_name = str(name).strip() if name else ""
  return resolved_name or None


def list_microphone_devices() -> tuple[MicrophoneDevice, ...]:
  """Map persistent Core Audio UIDs to the current PortAudio device indices."""

  try:
    port_audio_devices = sd.query_devices()
    host_apis = sd.query_hostapis()
    default_input_index = int(sd.default.device[0])
  except (TypeError, ValueError, sd.PortAudioError) as exc:
    raise RecoError(f"Could not list audio input devices: {exc}") from exc
  core_audio_host = next(
    (host_api for host_api in host_apis if str(host_api.get("name", "")) == "Core Audio"),
    None,
  )
  if core_audio_host is None:
    raise RecoError("The Core Audio PortAudio host is unavailable")
  port_audio_indices = tuple(int(index) for index in core_audio_host.get("devices", ()))
  mapping = _match_core_audio_devices(
    port_audio_indices,
    port_audio_devices,
    list_core_audio_devices(),
  )
  return tuple(
    MicrophoneDevice(
      uid=core_audio_device.uid,
      index=index,
      name=str(port_audio_devices[index]["name"]),
      channels=int(port_audio_devices[index]["max_input_channels"]),
      is_default=index == default_input_index,
    )
    for index, core_audio_device in mapping
    if int(port_audio_devices[index]["max_input_channels"]) > 0
  )


def resolve_microphone_device(uid: str) -> MicrophoneDevice:
  """Resolve one persistent UID without falling back to a different device."""

  device = next((candidate for candidate in list_microphone_devices() if candidate.uid == uid), None)
  if device is None:
    raise RecoError(f"The selected audio input device is unavailable: {uid}")
  return device


def _match_core_audio_devices(
  port_audio_indices: tuple[int, ...],
  port_audio_devices: Any,
  core_audio_devices: tuple[CoreAudioDevice, ...],
) -> tuple[tuple[int, CoreAudioDevice], ...]:
  if len(port_audio_indices) != len(core_audio_devices):
    raise RecoError("Core Audio and PortAudio returned different device counts")
  mapping = tuple(zip(port_audio_indices, core_audio_devices, strict=True))
  for index, core_audio_device in mapping:
    port_audio_name = str(port_audio_devices[index]["name"]).strip()
    if port_audio_name != core_audio_device.name:
      raise RecoError("Core Audio and PortAudio device identities do not match")
  return mapping


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

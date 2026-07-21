from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass
from threading import Event, get_ident
from time import sleep

import numpy as np
import pytest

from reco.audio import SAMPLE_RATE, VAD_FRAME_SAMPLES, AudioChunk, AudioInput, AudioStream, SourceMetadata
from reco.config import TranscriptionConfig
from reco.errors import RecoError
from reco.models import (
  RunStatus,
  SpeechSegment,
  TranscriptionDiagnostics,
  TranscriptionResult,
  TranscriptModelMetadata,
  TranscriptSegment,
  VadDiagnostics,
)
from reco.pipeline import TranscriptionProgress, run_transcription, start_asr_worker


@dataclass
class FakeInput(AudioInput):
  chunks: list[AudioChunk]
  finite: bool = True

  def open(self) -> AudioStream:
    return AudioStream(
      source=SourceMetadata(kind="file", path="audio.wav"),
      chunks=iter(self.chunks),
      finite=self.finite,
    )


class InterruptingInput(AudioInput):
  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="microphone"), chunks=self._chunks(), finite=False)

  def _chunks(self) -> Iterator[AudioChunk]:
    yield chunk(0)
    raise KeyboardInterrupt


class CoordinatedInterruptingInput(AudioInput):
  def __init__(self, transcription_started: Event) -> None:
    self.transcription_started = transcription_started

  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="microphone"), chunks=self._chunks(), finite=False)

  def _chunks(self) -> Iterator[AudioChunk]:
    yield chunk(0)
    assert self.transcription_started.wait(timeout=1)
    yield chunk(1)
    raise KeyboardInterrupt


class QueueDepthInput(AudioInput):
  def __init__(self, transcription_started: Event, release_transcription: Event) -> None:
    self.transcription_started = transcription_started
    self.release_transcription = release_transcription

  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="file"), chunks=self._chunks(), finite=True)

  def _chunks(self) -> Iterator[AudioChunk]:
    yield chunk(0)
    assert self.transcription_started.wait(timeout=1)
    yield chunk(1)
    yield chunk(2)
    self.release_transcription.set()


class ExplodingInput(AudioInput):
  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="file"), chunks=self._chunks(), finite=True)

  def _chunks(self) -> Iterator[AudioChunk]:
    yield chunk(0)
    raise RecoError("input exploded")


class WaitingAfterFirstChunkInput(AudioInput):
  def __init__(self, failure_started: Event) -> None:
    self.failure_started = failure_started
    self.yielded = 0

  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="microphone"), chunks=self._chunks(), finite=False)

  def _chunks(self) -> Iterator[AudioChunk]:
    self.yielded += 1
    yield chunk(0)
    assert self.failure_started.wait(timeout=1)
    sleep(0.01)
    for index in range(1, 1_000):
      self.yielded += 1
      yield chunk(index)


class ClosableChunks:
  def __init__(self) -> None:
    self.closed = False
    self._emitted = False

  def __iter__(self) -> ClosableChunks:
    return self

  def __next__(self) -> AudioChunk:
    if self._emitted:
      raise StopIteration
    self._emitted = True
    return chunk(0)

  def close(self) -> None:
    self.closed = True


class ClosingInput(AudioInput):
  def __init__(self) -> None:
    self.chunks = ClosableChunks()

  def open(self) -> AudioStream:
    return AudioStream(source=SourceMetadata(kind="microphone"), chunks=self.chunks, finite=False)


class FakeVad:
  def __init__(
    self,
    segments: list[SpeechSegment] | None = None,
    flushed_segments: list[SpeechSegment] | None = None,
  ) -> None:
    self.segments = segments or []
    self.flushed_segments = flushed_segments or []
    self.processed = 0
    self.flushed_with: bool | None = None

  def reset(self) -> None:
    self.processed = 0

  def process_frame(self, chunk: AudioChunk) -> list[SpeechSegment]:
    del chunk
    self.processed += 1
    return self.segments if self.processed == 1 else []

  def flush(self, finalize_open_segment: bool) -> list[SpeechSegment]:
    self.flushed_with = finalize_open_segment
    return self.flushed_segments if finalize_open_segment else []


class InterruptingVad(FakeVad):
  def process_frame(self, chunk: AudioChunk) -> list[SpeechSegment]:
    del chunk
    raise KeyboardInterrupt


class SequentialVad(FakeVad):
  def process_frame(self, chunk: AudioChunk) -> list[SpeechSegment]:
    del chunk
    if self.processed >= len(self.segments):
      return []
    speech_segment = self.segments[self.processed]
    self.processed += 1
    return [speech_segment]


class FakeTranscriptionService:
  model_load_ms = 7

  def __init__(self) -> None:
    self.load_thread_id: int | None = None
    self.transcribe_thread_ids: list[int] = []

  def load_model(self) -> None:
    self.load_thread_id = get_ident()

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.transcribe_thread_ids.append(get_ident())
    return TranscriptionResult(
      text=f"text {segment.audio.size}",
      raw_text=f"text {segment.audio.size}",
      language="Japanese",
      diagnostics=TranscriptionDiagnostics(max_tokens=64, generation_tokens=4),
    )


class FailingTranscriptionService(FakeTranscriptionService):
  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    del segment
    raise RecoError("transcription exploded")


class SignalingFailingTranscriptionService(FakeTranscriptionService):
  def __init__(self) -> None:
    super().__init__()
    self.failure_started = Event()

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    del segment
    self.failure_started.set()
    raise RecoError("transcription exploded")


class SlowTranscriptionService(FakeTranscriptionService):
  def __init__(self) -> None:
    super().__init__()
    self.transcription_started = Event()

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.transcription_started.set()
    sleep(0.05)
    return super().transcribe(segment)


class QueueDepthTranscriptionService(FakeTranscriptionService):
  def __init__(self) -> None:
    super().__init__()
    self.transcription_started = Event()
    self.release_transcription = Event()

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.transcription_started.set()
    assert self.release_transcription.wait(timeout=1)
    return super().transcribe(segment)


class BlockingTranscriptionService(FakeTranscriptionService):
  def __init__(self) -> None:
    super().__init__()
    self.transcription_started = Event()
    self.release_transcription = Event()

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.transcription_started.set()
    assert self.release_transcription.wait(timeout=2)
    return super().transcribe(segment)


class PartiallyFailingTranscriptionService(FakeTranscriptionService):
  def __init__(self) -> None:
    super().__init__()
    self.calls = 0

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.calls += 1
    if self.calls == 2:
      raise RecoError("second transcription exploded")
    return super().transcribe(segment)


def chunk(index: int) -> AudioChunk:
  return AudioChunk(
    samples=np.zeros(VAD_FRAME_SAMPLES, dtype=np.float32),
    sample_rate=SAMPLE_RATE,
    start_sample=index * VAD_FRAME_SAMPLES,
  )


def segment(start_sample: int = 0, samples: int = SAMPLE_RATE) -> SpeechSegment:
  return SpeechSegment(
    start_sample=start_sample,
    audio=np.zeros(samples, dtype=np.float32),
    sample_rate=SAMPLE_RATE,
    vad=VadDiagnostics(mean_probability=0.8),
  )


def model_metadata() -> TranscriptModelMetadata:
  return TranscriptModelMetadata(path="fixed-model", language="Japanese")


def test_pipeline_preserves_sample_boundaries_and_runs_model_on_one_worker_thread() -> None:
  speech = segment(start_sample=3_200, samples=8_000)
  service = FakeTranscriptionService()
  progress: list[TranscriptionProgress] = []

  result = run_transcription(
    audio_input=FakeInput(chunks=[chunk(0)]),
    vad_engine=FakeVad([speech]),
    transcription_service=service,
    model_metadata=model_metadata(),
    progress_callback=progress.append,
  )

  assert result.text == "text 8000"
  assert result.model == model_metadata()
  assert len(result.segments) == 1
  completed = result.segments[0]
  assert (completed.start_sample, completed.end_sample, completed.sample_rate) == (3_200, 11_200, SAMPLE_RATE)
  assert completed.vad.mean_probability == 0.8
  assert completed.transcription.generation_tokens == 4
  assert service.load_thread_id is not None
  assert service.load_thread_id != get_ident()
  assert service.transcribe_thread_ids == [service.load_thread_id]
  assert progress[-1].event == "transcript"
  assert progress[-1].latest_text == "text 8000"


def test_stdout_progress_is_published_before_optional_recording_work() -> None:
  events: list[str] = []

  def progress_callback(progress: TranscriptionProgress) -> None:
    if progress.event == "transcript":
      events.append("stdout")

  result = run_transcription(
    audio_input=FakeInput(chunks=[chunk(0)]),
    vad_engine=FakeVad([segment()]),
    transcription_service=FakeTranscriptionService(),
    model_metadata=model_metadata(),
    progress_callback=progress_callback,
    segment_callback=lambda completed: events.append(f"record:{completed.index}"),
  )

  assert result.total_segments == 1
  assert events == ["stdout", "record:0"]


def test_keyboard_interrupt_finalizes_the_open_microphone_segment() -> None:
  vad = FakeVad(flushed_segments=[segment(samples=4_000)])

  result = run_transcription(
    audio_input=InterruptingInput(),
    vad_engine=vad,
    transcription_service=FakeTranscriptionService(),
    model_metadata=model_metadata(),
  )

  assert vad.flushed_with is True
  assert len(result.segments) == 1
  assert result.segments[0].end_sample == 4_000


def test_keyboard_interrupt_closes_capture_before_draining_worker() -> None:
  audio_input = ClosingInput()

  result = run_transcription(
    audio_input=audio_input,
    vad_engine=InterruptingVad(),
    transcription_service=FakeTranscriptionService(),
    model_metadata=model_metadata(),
  )

  assert result.status is RunStatus.INTERRUPTED
  assert audio_input.chunks.closed is True


def test_keyboard_interrupt_flush_waits_for_space_in_a_full_asr_queue() -> None:
  service = SlowTranscriptionService()
  vad = SequentialVad(
    segments=[segment(start_sample=0, samples=100), segment(start_sample=1_000, samples=100)],
    flushed_segments=[segment(start_sample=2_000, samples=100)],
  )

  result = run_transcription(
    audio_input=CoordinatedInterruptingInput(service.transcription_started),
    vad_engine=vad,
    transcription_service=service,
    model_metadata=model_metadata(),
    config=TranscriptionConfig(max_transcription_queue_size=1),
  )

  assert result.status is RunStatus.INTERRUPTED
  assert len(result.segments) == 3


def test_keyboard_interrupt_abandons_a_native_worker_after_the_bounded_grace_period() -> None:
  service = BlockingTranscriptionService()
  worker = start_asr_worker(service)
  config = TranscriptionConfig(
    interrupted_worker_shutdown_timeout_seconds=0.01,
    failed_worker_shutdown_timeout_seconds=0.01,
  )

  try:
    with pytest.raises(RecoError, match="daemon worker was abandoned"):
      run_transcription(
        audio_input=CoordinatedInterruptingInput(service.transcription_started),
        vad_engine=FakeVad([segment()]),
        transcription_service=service,
        model_metadata=model_metadata(),
        asr_worker=worker,
        config=config,
      )
  finally:
    service.release_transcription.set()
    worker.thread.join(timeout=1)

  assert not worker.thread.is_alive()


def test_progress_is_coalesced_instead_of_emitted_for_every_32ms_frame() -> None:
  chunks = [chunk(index) for index in range(2_000)]
  progress: list[TranscriptionProgress] = []

  result = run_transcription(
    audio_input=FakeInput(chunks=chunks),
    vad_engine=FakeVad(),
    transcription_service=FakeTranscriptionService(),
    model_metadata=model_metadata(),
    progress_callback=progress.append,
  )

  chunk_events = [event for event in progress if event.event == "chunk"]
  assert len(chunk_events) < 10
  assert result.timing.media_duration_ms == 64_000
  assert result.timing.decode_time_ms == 0


@pytest.mark.parametrize("finite", [False, True])
def test_pipeline_rejects_partial_frames_that_are_not_a_finite_terminal_frame(finite: bool) -> None:
  partial = AudioChunk(samples=np.zeros(20, dtype=np.float32), sample_rate=SAMPLE_RATE, start_sample=0)
  chunks = [partial]
  if finite:
    chunks.append(
      AudioChunk(
        samples=np.zeros(VAD_FRAME_SAMPLES, dtype=np.float32),
        sample_rate=SAMPLE_RATE,
        start_sample=20,
      )
    )

  with pytest.raises(RecoError, match=r"exactly 512|after its final partial"):
    run_transcription(
      audio_input=FakeInput(chunks=chunks, finite=finite),
      vad_engine=FakeVad(),
      transcription_service=FakeTranscriptionService(),
      model_metadata=model_metadata(),
    )


def test_max_queue_depth_is_tracked_exactly_during_worker_dequeues() -> None:
  service = QueueDepthTranscriptionService()
  result = run_transcription(
    audio_input=QueueDepthInput(service.transcription_started, service.release_transcription),
    vad_engine=SequentialVad(segments=[segment(), segment(20_000), segment(40_000)]),
    transcription_service=service,
    model_metadata=model_metadata(),
    config=TranscriptionConfig(max_transcription_queue_size=2),
  )

  assert result.max_queue_depth == 2


def test_input_failure_stops_an_existing_worker_and_preserves_the_original_error() -> None:
  service = FakeTranscriptionService()
  worker = start_asr_worker(service)

  with pytest.raises(RecoError, match="input exploded"):
    run_transcription(
      audio_input=ExplodingInput(),
      vad_engine=FakeVad(),
      transcription_service=service,
      model_metadata=model_metadata(),
      asr_worker=worker,
    )

  assert not worker.thread.is_alive()


def test_transcription_failure_stops_worker_without_deadlock() -> None:
  service = FailingTranscriptionService()
  worker = start_asr_worker(service)

  with pytest.raises(RecoError, match="transcription exploded"):
    run_transcription(
      audio_input=FakeInput(chunks=[chunk(0)]),
      vad_engine=FakeVad([segment()]),
      transcription_service=service,
      model_metadata=model_metadata(),
      asr_worker=worker,
    )

  assert not worker.thread.is_alive()


def test_worker_failure_is_detected_while_microphone_continues_with_silence() -> None:
  service = SignalingFailingTranscriptionService()
  audio_input = WaitingAfterFirstChunkInput(service.failure_started)
  worker = start_asr_worker(service)

  with pytest.raises(RecoError, match="transcription exploded"):
    run_transcription(
      audio_input=audio_input,
      vad_engine=FakeVad([segment()]),
      transcription_service=service,
      model_metadata=model_metadata(),
      asr_worker=worker,
    )

  assert audio_input.yielded < 1_000
  assert not worker.thread.is_alive()


def test_successful_result_is_published_before_a_later_worker_failure() -> None:
  service = PartiallyFailingTranscriptionService()
  published: list[TranscriptSegment] = []

  with pytest.raises(RecoError, match="second transcription exploded"):
    run_transcription(
      audio_input=FakeInput(chunks=[chunk(0)]),
      vad_engine=FakeVad([segment(start_sample=0), segment(start_sample=20_000)]),
      transcription_service=service,
      model_metadata=model_metadata(),
      segment_callback=published.append,
    )

  assert [completed.index for completed in published] == [0]
  assert published[0].text == f"text {SAMPLE_RATE}"

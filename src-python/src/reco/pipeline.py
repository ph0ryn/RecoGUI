"""Transcription pipeline shared by microphone and file inputs."""

from __future__ import annotations

from collections.abc import Callable
from contextlib import suppress
from dataclasses import dataclass
from datetime import datetime
from queue import Empty, Full, Queue
from threading import Event, Thread
from time import monotonic, sleep
from typing import Protocol, cast

from reco.audio import SAMPLE_RATE, VAD_FRAME_SAMPLES, AudioInput, AudioStream
from reco.config import DEFAULT_TRANSCRIPTION_CONFIG, TranscriptionConfig
from reco.errors import RecoError
from reco.models import (
  RunStatus,
  SpeechSegment,
  TranscriptDocument,
  TranscriptModelMetadata,
  TranscriptSegment,
  TranscriptTiming,
)
from reco.transcription import TranscriptionService
from reco.vad import VadEngine

PROGRESS_INTERVAL_SECONDS = 0.125


class TranscriptionControl(Protocol):
  """Cooperative Stop/Cancel state supplied by the application engine."""

  def stop_requested(self) -> bool:
    """Return whether input capture should stop and pending ASR should drain."""

  def cancel_requested(self) -> bool:
    """Return whether input and pending ASR should be truncated."""


@dataclass(frozen=True)
class TranscriptionProgress:
  """Progress snapshot emitted while a transcription session is running."""

  event: str
  processed_audio_ms: int
  total_segments: int
  recognized_segments: int
  characters: int
  queue_depth: int
  max_queue_depth: int
  latest_text: str | None = None
  latest_start_ms: int | None = None


@dataclass(frozen=True)
class _QueuedSegment:
  index: int
  segment: SpeechSegment
  submitted_at: float


@dataclass(frozen=True)
class _CompletedSegment:
  index: int
  segment: TranscriptSegment


@dataclass
class _PipelineState:
  processed_audio_ms: int = 0
  total_segments: int = 0
  recognized_segments: int = 0
  characters: int = 0
  max_queue_depth: int = 0


class _SegmentQueue(Queue[_QueuedSegment | None]):
  """Queue with exact segment-depth metrics updated under the queue mutex."""

  def __init__(self, maxsize: int) -> None:
    super().__init__(maxsize=maxsize)
    self._segment_depth = 0
    self._max_segment_depth = 0

  def _put(self, item: _QueuedSegment | None) -> None:
    super()._put(item)
    if item is not None:
      self._segment_depth += 1
      self._max_segment_depth = max(self._max_segment_depth, self._segment_depth)

  def _get(self) -> _QueuedSegment | None:
    item = super()._get()
    if item is not None:
      self._segment_depth -= 1
    return item

  def depth(self) -> int:
    """Return the current queued segment count."""

    with self.mutex:
      return self._segment_depth

  def max_depth(self) -> int:
    """Return the exact maximum queued segment count."""

    with self.mutex:
      return self._max_segment_depth


@dataclass
class AsrWorker:
  """Long-lived ASR worker that owns MLX model loading and generation."""

  segment_queue: _SegmentQueue
  result_queue: Queue[_CompletedSegment]
  worker_error: list[BaseException]
  ready: Event
  thread: Thread

  def wait_until_ready(self) -> None:
    """Block until the worker has loaded the model or failed."""

    self.ready.wait()
    _raise_worker_error(self.worker_error)

  def stop(self, *, cancel_pending: bool = False, timeout: float | None = None) -> None:
    """Stop the worker and propagate any pending worker error."""

    if timeout is not None and timeout <= 0:
      raise ValueError("ASR worker shutdown timeout must be positive")
    deadline = None if timeout is None else monotonic() + timeout
    if cancel_pending:
      while True:
        try:
          self.segment_queue.get_nowait()
        except Empty:
          break
    while self.thread.is_alive():
      remaining = None if deadline is None else deadline - monotonic()
      if remaining is not None and remaining <= 0:
        raise _worker_shutdown_timeout(timeout)
      try:
        self.segment_queue.put(None, timeout=0.1 if remaining is None else min(0.1, remaining))
        break
      except Full:
        if cancel_pending:
          with suppress(Empty):
            self.segment_queue.get_nowait()
    remaining = None if deadline is None else max(0.0, deadline - monotonic())
    self.thread.join(remaining)
    if self.thread.is_alive():
      raise _worker_shutdown_timeout(timeout)
    _raise_worker_error(self.worker_error)


def start_asr_worker(
  transcription_service: TranscriptionService,
  *,
  config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG,
) -> AsrWorker:
  """Start the ASR worker and begin loading the model on the worker thread."""

  result_queue: Queue[_CompletedSegment] = Queue()
  segment_queue = _SegmentQueue(maxsize=config.max_transcription_queue_size)
  worker_error: list[BaseException] = []
  ready = Event()
  worker = Thread(
    target=_run_asr_worker,
    args=(segment_queue, result_queue, transcription_service, worker_error, ready),
    daemon=True,
  )
  worker.start()
  return AsrWorker(
    segment_queue=segment_queue,
    result_queue=result_queue,
    worker_error=worker_error,
    ready=ready,
    thread=worker,
  )


def run_transcription(
  audio_input: AudioInput,
  vad_engine: VadEngine,
  transcription_service: TranscriptionService,
  model_metadata: TranscriptModelMetadata,
  *,
  asr_worker: AsrWorker | None = None,
  progress_callback: Callable[[TranscriptionProgress], None] | None = None,
  segment_callback: Callable[[TranscriptSegment], None] | None = None,
  session_started_at: datetime | None = None,
  session_started_monotonic: float | None = None,
  initial_sample: int = 0,
  control: TranscriptionControl | None = None,
  config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG,
) -> TranscriptDocument:
  """Run input, VAD, local ASR worker, and output aggregation."""

  owns_worker = asr_worker is None
  worker = asr_worker or start_asr_worker(transcription_service, config=config)
  state = _PipelineState()
  pending_results: dict[int, TranscriptSegment] = {}
  transcript_segments: list[TranscriptSegment] = []
  next_result_index = 0
  interrupted = False
  cancelled = False
  controlled_stop = False
  started_at = session_started_at or datetime.now().astimezone()
  command_started = session_started_monotonic if session_started_monotonic is not None else monotonic()
  pipeline_started: float | None = None
  stream = None
  pipeline_error: BaseException | None = None
  next_progress_at = 0.0
  if initial_sample < 0:
    raise ValueError("Initial sample must not be negative")
  expected_start_sample = initial_sample
  state.processed_audio_ms = round(initial_sample * 1000 / SAMPLE_RATE)
  partial_frame_seen = False

  try:
    worker.wait_until_ready()
    stream = audio_input.open()
    vad_engine.reset()
    pipeline_started = monotonic()
    next_progress_at = pipeline_started
    for chunk in stream.chunks:
      if control is not None and control.cancel_requested() and not stream.drain_on_stop:
        interrupted = True
        cancelled = True
        controlled_stop = True
        break
      if control is not None and control.stop_requested() and not stream.drain_on_stop:
        interrupted = True
        controlled_stop = True
        break
      if chunk.sample_rate != SAMPLE_RATE:
        raise RecoError(f"Audio input must use {SAMPLE_RATE} Hz samples, got {chunk.sample_rate} Hz.")
      if chunk.start_sample != expected_start_sample:
        raise RecoError(
          f"Audio input must be contiguous: expected sample {expected_start_sample}, got {chunk.start_sample}."
        )
      if chunk.samples.size > VAD_FRAME_SAMPLES:
        raise RecoError(f"Audio input frame cannot exceed {VAD_FRAME_SAMPLES} samples.")
      if partial_frame_seen:
        raise RecoError("Audio input emitted data after its final partial VAD frame.")
      if chunk.samples.size < VAD_FRAME_SAMPLES:
        partial_frame_seen = True
      expected_start_sample = chunk.start_sample + chunk.samples.size
      chunk_end_ms = round((chunk.start_sample + chunk.samples.size) * 1000 / chunk.sample_rate)
      state.processed_audio_ms = max(state.processed_audio_ms, chunk_end_ms)
      current_time = monotonic()
      if current_time >= next_progress_at:
        _emit_progress(progress_callback, event="chunk", state=state, queue_depth=worker.segment_queue.depth())
        next_progress_at = current_time + PROGRESS_INTERVAL_SECONDS

      for speech_segment in vad_engine.process_frame(chunk):
        _enqueue_segment(speech_segment, stream.finite, worker.segment_queue, state, worker.worker_error)
        _emit_progress(progress_callback, event="segment", state=state, queue_depth=worker.segment_queue.depth())

      next_result_index = _drain_results(
        worker.result_queue,
        pending_results,
        transcript_segments,
        next_result_index,
        state,
        progress_callback,
        segment_callback,
        worker.segment_queue.depth(),
      )
      _raise_worker_error(worker.worker_error)
  except KeyboardInterrupt as exc:
    if stream is None or pipeline_started is None:
      pipeline_error = exc
    else:
      interrupted = True
  except BaseException as exc:
    pipeline_error = exc

  if stream is not None and stream.drain_on_stop and control is not None and control.stop_requested():
    interrupted = True
    controlled_stop = True
    cancelled = control.cancel_requested()

  if stream is not None:
    try:
      _close_audio_chunks(stream)
    except BaseException as exc:
      if pipeline_error is None:
        pipeline_error = exc
      else:
        pipeline_error.add_note(f"Could not close audio input cleanly: {exc}")

  if pipeline_error is None and stream is not None and not cancelled:
    try:
      for speech_segment in vad_engine.flush(finalize_open_segment=True):
        _enqueue_segment(speech_segment, True, worker.segment_queue, state, worker.worker_error)
    except BaseException as exc:
      pipeline_error = exc

  shutdown_timeout = None
  if pipeline_error is not None:
    shutdown_timeout = config.failed_worker_shutdown_timeout_seconds
  elif interrupted:
    shutdown_timeout = config.interrupted_worker_shutdown_timeout_seconds
  should_stop_worker = owns_worker or pipeline_error is not None or cancelled or (interrupted and not controlled_stop)
  if should_stop_worker:
    try:
      worker.stop(cancel_pending=pipeline_error is not None or cancelled, timeout=shutdown_timeout)
    except BaseException as exc:
      if pipeline_error is None:
        pipeline_error = exc
      elif exc is not pipeline_error:
        pipeline_error.add_note(f"Could not stop ASR worker cleanly ({type(exc).__name__}): {exc}")
  else:
    try:
      while next_result_index < state.total_segments:
        next_result_index = _drain_results(
          worker.result_queue,
          pending_results,
          transcript_segments,
          next_result_index,
          state,
          progress_callback,
          segment_callback,
          worker.segment_queue.depth(),
        )
        _raise_worker_error(worker.worker_error)
        if next_result_index < state.total_segments:
          sleep(0.01)
    except BaseException as exc:
      pipeline_error = exc
      try:
        worker.stop(cancel_pending=True, timeout=config.failed_worker_shutdown_timeout_seconds)
      except BaseException as cleanup_error:
        if cleanup_error is not exc:
          exc.add_note(f"Could not stop ASR worker cleanly ({type(cleanup_error).__name__}): {cleanup_error}")

  if pipeline_error is not None:
    try:
      next_result_index = _drain_results(
        worker.result_queue,
        pending_results,
        transcript_segments,
        next_result_index,
        state,
        progress_callback,
        segment_callback,
        worker.segment_queue.depth(),
      )
    except BaseException as exc:
      if exc is not pipeline_error:
        pipeline_error.add_note(f"Could not publish completed ASR results ({type(exc).__name__}): {exc}")
    raise pipeline_error.with_traceback(pipeline_error.__traceback__)

  if stream is None or pipeline_started is None:
    raise RecoError("Audio input did not open a stream.")

  next_result_index = _drain_results(
    worker.result_queue,
    pending_results,
    transcript_segments,
    next_result_index,
    state,
    progress_callback,
    segment_callback,
    worker.segment_queue.depth(),
  )
  del next_result_index

  if transcript_segments:
    state.processed_audio_ms = max(state.processed_audio_ms, transcript_segments[-1].end_ms)

  ended_at = datetime.now().astimezone()
  ended_monotonic = monotonic()
  pipeline_wall_time_ms = max(0, round((ended_monotonic - pipeline_started) * 1000))
  command_wall_time_ms = max(0, round((ended_monotonic - command_started) * 1000))
  decode_time_ms = sum(segment.decode_ms for segment in transcript_segments)
  pipeline_rtf = None if state.processed_audio_ms == 0 else pipeline_wall_time_ms / state.processed_audio_ms
  decode_rtf = None if state.processed_audio_ms == 0 else decode_time_ms / state.processed_audio_ms

  return TranscriptDocument(
    source=stream.source,
    model=model_metadata,
    status=RunStatus.INTERRUPTED if interrupted else RunStatus.COMPLETED,
    timing=TranscriptTiming(
      command_started_at=started_at.isoformat(),
      command_ended_at=ended_at.isoformat(),
      command_wall_time_ms=command_wall_time_ms,
      pipeline_wall_time_ms=pipeline_wall_time_ms,
      media_duration_ms=state.processed_audio_ms,
      model_load_ms=transcription_service.model_load_ms,
      decode_time_ms=decode_time_ms,
      pipeline_rtf=pipeline_rtf,
      decode_rtf=decode_rtf,
    ),
    max_queue_depth=state.max_queue_depth,
    segments=tuple(transcript_segments),
  )


def _run_asr_worker(
  segment_queue: _SegmentQueue,
  result_queue: Queue[_CompletedSegment],
  transcription_service: TranscriptionService,
  worker_error: list[BaseException],
  ready: Event,
) -> None:
  failed = False
  try:
    transcription_service.load_model()
  except BaseException as exc:
    worker_error.append(exc)
    ready.set()
    return
  ready.set()
  while True:
    queued = segment_queue.get()
    if queued is None:
      return
    if failed:
      continue
    try:
      queue_wait_ms = round((monotonic() - queued.submitted_at) * 1000)
      started = monotonic()
      result = transcription_service.transcribe(queued.segment)
      decode_ms = round((monotonic() - started) * 1000)
      result_queue.put(
        _CompletedSegment(
          index=queued.index,
          segment=TranscriptSegment(
            index=queued.index,
            start_sample=queued.segment.start_sample,
            end_sample=queued.segment.end_sample,
            sample_rate=queued.segment.sample_rate,
            split_reason=queued.segment.split_reason,
            text=result.text,
            raw_text=result.raw_text,
            language=result.language,
            vad=queued.segment.vad,
            transcription=result.diagnostics,
            queue_wait_ms=queue_wait_ms,
            decode_ms=decode_ms,
          ),
        )
      )
    except BaseException as exc:
      worker_error.append(exc)
      failed = True


def _enqueue_segment(
  speech_segment: SpeechSegment,
  finite_input: bool,
  segment_queue: _SegmentQueue,
  state: _PipelineState,
  worker_error: list[BaseException],
) -> None:
  queued = _QueuedSegment(index=state.total_segments, segment=speech_segment, submitted_at=monotonic())
  if finite_input:
    while True:
      _raise_worker_error(worker_error)
      try:
        segment_queue.put(queued, timeout=0.1)
        break
      except Full:
        continue
  else:
    _raise_worker_error(worker_error)
    try:
      segment_queue.put_nowait(queued)
    except Full as exc:
      raise RecoError("Transcription queue is full; local ASR is slower than realtime input.") from exc

  state.total_segments += 1
  state.max_queue_depth = max(state.max_queue_depth, segment_queue.max_depth())


def _drain_results(
  result_queue: Queue[_CompletedSegment],
  pending_results: dict[int, TranscriptSegment],
  transcript_segments: list[TranscriptSegment],
  next_result_index: int,
  state: _PipelineState,
  progress_callback: Callable[[TranscriptionProgress], None] | None,
  segment_callback: Callable[[TranscriptSegment], None] | None,
  queue_depth: int,
) -> int:
  while True:
    try:
      completed = result_queue.get_nowait()
    except Empty:
      break
    pending_results[completed.index] = completed.segment

  while next_result_index in pending_results:
    segment = pending_results.pop(next_result_index)
    transcript_segments.append(segment)
    if segment.text:
      state.recognized_segments += 1
      state.characters += len(segment.text)
      _emit_progress(
        progress_callback,
        event="transcript",
        state=state,
        queue_depth=queue_depth,
        latest_text=segment.text,
        latest_start_ms=segment.start_ms,
      )
    if segment_callback is not None:
      segment_callback(segment)
    next_result_index += 1

  return next_result_index


def _emit_progress(
  progress_callback: Callable[[TranscriptionProgress], None] | None,
  *,
  event: str,
  state: _PipelineState,
  queue_depth: int,
  latest_text: str | None = None,
  latest_start_ms: int | None = None,
) -> None:
  if progress_callback is None:
    return
  progress_callback(
    TranscriptionProgress(
      event=event,
      processed_audio_ms=state.processed_audio_ms,
      total_segments=state.total_segments,
      recognized_segments=state.recognized_segments,
      characters=state.characters,
      queue_depth=queue_depth,
      max_queue_depth=state.max_queue_depth,
      latest_text=latest_text,
      latest_start_ms=latest_start_ms,
    )
  )


def _raise_worker_error(worker_error: list[BaseException]) -> None:
  if worker_error:
    error = worker_error[0]
    if isinstance(error, RecoError):
      raise error
    raise RecoError(f"ASR worker failed: {error}") from error


def _worker_shutdown_timeout(timeout: float | None) -> RecoError:
  timeout_label = "the configured timeout" if timeout is None else f"{timeout:g} seconds"
  return RecoError(
    f"ASR worker did not stop within {timeout_label}; the daemon worker was abandoned so the CLI can exit."
  )


def _close_audio_chunks(stream: AudioStream) -> None:
  close = getattr(stream.chunks, "close", None)
  if callable(close):
    cast(Callable[[], None], close)()

"""Headless persistent transcription engine shared by GUI and sidecar."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import asdict, replace
from datetime import datetime
from pathlib import Path
from threading import Event, Lock, Thread
from time import monotonic
from typing import cast

import sounddevice as sd

from reco.audio import (
  SAMPLE_RATE,
  LocalAudioFileInput,
  MicrophoneInput,
  audio_file_duration_ms,
  resolve_microphone_device_name,
  validate_audio_file,
)
from reco.config import DEFAULT_CONFIG
from reco.errors import RecoError
from reco.model_manager import MODEL_ID, MODEL_REVISION, ModelManager
from reco.models import TranscriptModelMetadata, TranscriptSegment
from reco.pipeline import AsrWorker, TranscriptionProgress, run_transcription, start_asr_worker
from reco.recording import fingerprint_file_snapshot
from reco.repository import (
  NewQueueItem,
  NewSession,
  RecordingRepository,
  RepositoryError,
  SessionMutationReceipt,
  SessionState,
)
from reco.transcription import LocalAsrTranscriptionService
from reco.vad import OnnxSileroProbabilityModel, SileroVadEngine, ensure_silero_vad_asset

EngineEventCallback = Callable[[str, str | None, Mapping[str, object]], None]


class EngineCommandError(RecoError):
  """Stable command failure returned across the sidecar protocol."""

  def __init__(
    self, code: str, message: str, *, recoverable: bool = True, details: dict[str, object] | None = None
  ) -> None:
    super().__init__(message)
    self.code = code
    self.recoverable = recoverable
    self.details = details


class SessionControl:
  """Thread-safe cooperative controls for one session."""

  def __init__(self) -> None:
    self._stop = Event()
    self._cancel = Event()
    self._stop_reason = "userStop"

  @property
  def stop_reason(self) -> str:
    return self._stop_reason

  def request_stop(self, reason: str = "userStop") -> None:
    self._stop_reason = reason
    self._stop.set()

  def request_cancel(self) -> None:
    self._cancel.set()
    self._stop.set()

  def stop_requested(self) -> bool:
    return self._stop.is_set()

  def cancel_requested(self) -> bool:
    return self._cancel.is_set()


class ModelRuntime:
  """Resident ASR worker that owns model loading and generation."""

  def __init__(self, model_path: Path) -> None:
    self.model_path = model_path
    self._service: LocalAsrTranscriptionService | None = None
    self._worker: AsrWorker | None = None
    self._lock = Lock()

  @property
  def loaded(self) -> bool:
    return self._worker is not None and self._worker.thread.is_alive() and not self._worker.worker_error

  def acquire(self) -> tuple[LocalAsrTranscriptionService, AsrWorker]:
    """Start the fixed model worker once and return its resident resources."""

    with self._lock:
      if self.loaded:
        assert self._service is not None and self._worker is not None
        return self._service, self._worker
      self._service = LocalAsrTranscriptionService(self.model_path, language=DEFAULT_CONFIG.cli.default_language)
      self._worker = start_asr_worker(self._service)
      self._worker.wait_until_ready()
      return self._service, self._worker

  def invalidate(self) -> None:
    """Drop a cancelled or failed worker so the next session reloads cleanly."""

    with self._lock:
      worker = self._worker
      self._worker = None
      self._service = None
    if worker is not None and worker.thread.is_alive():
      worker.stop(cancel_pending=True, timeout=DEFAULT_CONFIG.transcription.failed_worker_shutdown_timeout_seconds)

  def close(self) -> None:
    self.invalidate()


class RecoEngine:
  """Single-session application engine with durable history."""

  def __init__(self, database: Path, models_directory: Path, event_callback: EngineEventCallback | None = None) -> None:
    self.repository = RecordingRepository(database)
    self.repository.recover_abandoned()
    self.model_manager = ModelManager(models_directory)
    self.runtime = ModelRuntime(self.model_manager.model_directory)
    self._event_callback = event_callback
    self._active_session_id: str | None = None
    self._active_control: SessionControl | None = None
    self._active_thread: Thread | None = None
    self._lock = Lock()
    self._shutting_down = False
    self._queue_auto_advance = False
    self._queue_scheduler_active = False
    self._active_queue_origin = False
    self._paused_queue_sessions: set[str] = set()

  def state(self) -> dict[str, object]:
    """Return the complete engine/model/session snapshot."""

    with self._lock:
      active = self._active_session_id
      engine_state = "shuttingDown" if self._shutting_down else ("running" if active else "idle")
    model = self.model_manager.snapshot(loaded=self.runtime.loaded)
    return {
      "engineState": engine_state,
      "modelState": model.state.value,
      "model": {
        "state": model.state.value,
        "modelId": model.model_id,
        "revision": model.revision,
        "bytesOnDisk": model.bytes_on_disk,
      },
      "activeSession": active,
    }

  def queue_state(self) -> dict[str, object]:
    """Return the durable queue plus the intentionally non-durable run toggle."""

    snapshot = cast(dict[str, object], _camel(self.repository.queue_snapshot()))
    with self._lock:
      snapshot["autoAdvanceEnabled"] = self._queue_auto_advance
    return snapshot

  def enqueue_files(self, files: object) -> dict[str, object]:
    """Append resolved files and immediately start when no work is already pending."""

    if not isinstance(files, list) or not files:
      raise EngineCommandError("invalid_queue_files", "files must be a non-empty array")
    values: list[NewQueueItem] = []
    for value in files:
      if not isinstance(value, Mapping):
        raise EngineCommandError("invalid_queue_file", "Each queue file requires a resolved path")
      file_value = cast(Mapping[str, object], value)
      if not file_value.get("path"):
        raise EngineCommandError("invalid_queue_file", "Each queue file requires a resolved path")
      path = Path(str(file_value["path"]))
      try:
        validate_audio_file(path)
        fingerprint = fingerprint_file_snapshot(path)
      except RecoError as exc:
        raise EngineCommandError("invalid_queue_file", str(exc)) from exc
      display_name = str(file_value.get("displayName") or path.name).strip()
      values.append(NewQueueItem(str(path), display_name, fingerprint.value))

    # Model verification may touch the filesystem, so keep it outside the engine lock.
    # This preliminary check is repeated while committing the queue entries to make
    # concurrent enqueue/session/shutdown transitions deterministic.
    with self._lock:
      may_start_immediately = (
        not self._shutting_down and self._active_session_id is None and not self.repository.queue_snapshot()["items"]
      )
    model_available = may_start_immediately and self.model_manager.ensure_verified()

    with self._lock:
      start_immediately = (
        model_available
        and not self._shutting_down
        and self._active_session_id is None
        and not self.repository.queue_snapshot()["items"]
      )
      self.repository.enqueue_files(values)
      if start_immediately:
        self._queue_auto_advance = True

    if start_immediately:
      self._schedule_next_queue_item()
      return self.queue_state()
    return self._publish_queue_changed()

  def reorder_queue(self, item_ids: object, revision: object) -> dict[str, object]:
    if not isinstance(item_ids, list) or not all(isinstance(value, str) for value in item_ids):
      raise EngineCommandError("invalid_queue_order", "itemIds must be an array of strings")
    if not isinstance(revision, int) or isinstance(revision, bool):
      raise EngineCommandError("invalid_queue_revision", "revision must be an integer")
    try:
      self.repository.reorder_queue(cast(list[str], item_ids), revision)
    except RepositoryError as exc:
      code = "queue_revision_conflict" if str(exc).startswith("Stale queue revision") else "invalid_queue_order"
      raise EngineCommandError(code, str(exc), details={"queue": self.queue_state()}) from exc
    return self._publish_queue_changed()

  def remove_queue_item(self, item_id: str) -> dict[str, object]:
    try:
      self.repository.remove_queue_item(item_id)
    except RepositoryError as exc:
      raise EngineCommandError("queue_item_not_found", str(exc), details={"queue": self.queue_state()}) from exc
    return self._publish_queue_changed()

  def clear_queue(self) -> dict[str, object]:
    self.repository.clear_queue()
    return self._publish_queue_changed()

  def start_queue(self) -> dict[str, object]:
    """Enable explicit serial advancement and claim the first usable item."""

    with self._lock:
      if self._shutting_down:
        raise EngineCommandError("engine_shutting_down", "The engine is shutting down", recoverable=False)
      if self._active_session_id is not None:
        raise EngineCommandError("session_active", "Another transcription session is already active")
    if not self.model_manager.ensure_verified():
      raise EngineCommandError("model_missing", "The fixed transcription model is not installed")
    with self._lock:
      if self._shutting_down:
        raise EngineCommandError("engine_shutting_down", "The engine is shutting down", recoverable=False)
      if self._active_session_id is not None:
        raise EngineCommandError("session_active", "Another transcription session is already active")
      self._queue_auto_advance = True
    snapshot = self._publish_queue_changed()
    self._schedule_next_queue_item()
    return snapshot

  def pause_queue(self) -> dict[str, object]:
    """Prevent another queued file from starting; the active session keeps running."""

    with self._lock:
      self._queue_auto_advance = False
    return self._publish_queue_changed()

  def start_session(self, payload: Mapping[str, object], requested_session_id: str | None = None) -> dict[str, object]:
    """Commit and asynchronously start a microphone transcription."""

    with self._lock:
      if self._shutting_down:
        raise EngineCommandError("engine_shutting_down", "The engine is shutting down", recoverable=False)
      if self._active_session_id is not None:
        raise EngineCommandError("session_active", "Another transcription session is already active")
      if self._queue_auto_advance:
        raise EngineCommandError("queue_active", "The file queue is active")
    if not self.model_manager.ensure_verified():
      raise EngineCommandError("model_missing", "The fixed transcription model is not installed")
    source_value = payload.get("source", {"type": "microphone"})
    if not isinstance(source_value, Mapping):
      raise EngineCommandError("invalid_source", "source must be an object")
    source_value = cast(Mapping[str, object], source_value)
    source_kind = str(source_value.get("type", "microphone"))
    if source_kind != "microphone":
      raise EngineCommandError("queue_required", "File sources must be added to the processing queue")
    source_path = Path(str(source_value["path"])) if source_kind == "file" and source_value.get("path") else None
    if source_kind == "file" and source_path is None:
      raise EngineCommandError("invalid_source", "A file source requires path")
    device = source_value.get("deviceId")
    fingerprint = None
    if source_path is not None:
      validate_audio_file(source_path)
      fingerprint = fingerprint_file_snapshot(source_path)
      display_name = source_path.name
    else:
      display_name = resolve_microphone_device_name(device if isinstance(device, int | str) else None) or "microphone"
    title = str(
      payload.get("title") or (source_path.stem if source_path else datetime.now().strftime("Recording %Y-%m-%d %H:%M"))
    )
    session_id = self.repository.create_session(
      NewSession(
        session_id=requested_session_id or "",
        source_kind=source_kind,
        source_display_name=display_name,
        source_fingerprint=fingerprint.value if fingerprint else None,
        source_path=str(source_path) if source_path is not None else None,
        source_device_id=str(device) if device is not None else None,
        model=MODEL_ID,
        model_revision=MODEL_REVISION,
        language=DEFAULT_CONFIG.cli.default_language,
        sample_rate=SAMPLE_RATE,
        title=title,
        config={"vad": asdict(DEFAULT_CONFIG.vad), "transcription": asdict(DEFAULT_CONFIG.transcription)},
      )
    )
    control = SessionControl()
    thread = Thread(
      target=self._run_session,
      args=(session_id, source_path, device, fingerprint, control, 0, 0),
      daemon=True,
      name=f"reco-session-{session_id}",
    )
    with self._lock:
      self._active_session_id = session_id
      self._active_control = control
      self._active_thread = thread
    thread.start()
    return {"sessionId": session_id, "state": SessionState.PREPARING.value, "rowVersion": 1}

  def stop_session(self, session_id: str, *, reason: str = "userStop") -> dict[str, object]:
    """Stop capture, flush VAD, and drain all queued ASR."""

    if reason not in {"userStop", "systemSleep", "appQuit"}:
      raise EngineCommandError("invalid_stop_reason", f"Unsupported stop reason: {reason}")
    control = self._require_active(session_id)
    self._stop_queue_for_active_session()
    receipt = self.repository.set_state(session_id, SessionState.STOPPING)
    control.request_stop(reason)
    self._emit("session.stateChanged", session_id, {**self._state_receipt(receipt), "reason": reason})
    return {"sessionId": session_id, **self._state_receipt(receipt)}

  def cancel_session(self, session_id: str) -> dict[str, object]:
    """Stop capture and discard pending, uncommitted ASR work."""

    control = self._require_active(session_id)
    self._stop_queue_for_active_session()
    receipt = self.repository.set_state(session_id, SessionState.STOPPING)
    control.request_cancel()
    self._emit("session.stateChanged", session_id, {**self._state_receipt(receipt), "cancelled": True})
    return {"sessionId": session_id, **self._state_receipt(receipt)}

  def pause_session(self, session_id: str) -> dict[str, object]:
    """Stop capture and drain queued ASR before making the session resumable."""

    control = self._require_active(session_id)
    queue_origin = False
    with self._lock:
      if getattr(self, "_active_queue_origin", False):
        queue_origin = True
        self._paused_queue_sessions.add(session_id)
        self._queue_auto_advance = False
    if queue_origin:
      self._publish_queue_changed()
    receipt = self.repository.set_state(session_id, SessionState.PAUSING)
    control.request_stop("userPause")
    self._emit("session.stateChanged", session_id, self._state_receipt(receipt))
    return {"sessionId": session_id, **self._state_receipt(receipt)}

  def resume_session(self, session_id: str) -> dict[str, object]:
    """Resume one durable paused session when the ASR slot is idle."""

    with self._lock:
      if self._shutting_down:
        raise EngineCommandError("engine_shutting_down", "The engine is shutting down", recoverable=False)
      if self._active_session_id is not None:
        raise EngineCommandError("session_active", "Another transcription session is already active")
      context = self.repository.get_resume_context(session_id)
      source_kind = str(context["source_kind"])
      queue_origin = source_kind == "file"
      if self._queue_auto_advance and not queue_origin:
        raise EngineCommandError("queue_active", "The file queue is active")
      source_path = Path(str(context["source_path"])) if context.get("source_path") else None
      device_value = context.get("source_device_id")
      device = _restore_device_id(str(device_value)) if device_value is not None else None
      fingerprint = None
      if source_kind == "file":
        if source_path is None:
          raise EngineCommandError("resume_source_missing", "The paused audio file path is unavailable")
        fingerprint = fingerprint_file_snapshot(source_path)
        if fingerprint.value != context.get("source_fingerprint"):
          raise EngineCommandError("resume_source_changed", "The paused audio file has changed")
      resume_sample = int(context["resume_sample"])
      segment_offset = int(context["total_segments"])
      control = SessionControl()
      receipt = self.repository.set_state(session_id, SessionState.PREPARING)
      thread = Thread(
        target=self._run_session,
        args=(session_id, source_path, device, fingerprint, control, resume_sample, segment_offset),
        daemon=True,
        name=f"reco-session-{session_id}",
      )
      self._active_session_id = session_id
      self._active_control = control
      self._active_thread = thread
      self._active_queue_origin = queue_origin
      if queue_origin:
        self._queue_auto_advance = True
      thread.start()
    if queue_origin:
      self._publish_queue_changed()
    self._emit("session.stateChanged", session_id, self._state_receipt(receipt))
    return {"sessionId": session_id, **self._state_receipt(receipt)}

  def shutdown(self, timeout: float = 30.0) -> None:
    """Gracefully stop the active session and resident model worker."""

    with self._lock:
      self._shutting_down = True
      self._queue_auto_advance = False
      session_id = self._active_session_id
      control = self._active_control
      thread = self._active_thread
    if session_id is not None and control is not None:
      with _ignore_repository_error():
        self.repository.set_state(session_id, SessionState.STOPPING)
      control.request_stop("appQuit")
    if thread is not None:
      thread.join(timeout)
      if thread.is_alive():
        with _ignore_repository_error():
          self.repository.set_state(session_id or "", SessionState.ABANDONED, end_reason="forceStop")
    self.runtime.close()

  def _stop_queue_for_active_session(self) -> None:
    changed = False
    with self._lock:
      if getattr(self, "_active_queue_origin", False):
        changed = self._queue_auto_advance
        self._queue_auto_advance = False
    if changed:
      self._publish_queue_changed()

  def _schedule_next_queue_item(self) -> None:
    """Claim at most one valid item; terminal sessions call this again."""

    with self._lock:
      if (
        not self._queue_auto_advance
        or self._queue_scheduler_active
        or self._active_session_id is not None
        or self._shutting_down
      ):
        return
      self._queue_scheduler_active = True
    try:
      for item in self.repository.queue_items_private():
        item_id = str(item["item_id"])
        source_path = Path(str(item["source_path"]))
        try:
          validate_audio_file(source_path)
          fingerprint = fingerprint_file_snapshot(source_path)
          if fingerprint.value != item["source_fingerprint"]:
            raise EngineCommandError("queue_source_changed", "The queued audio file has changed")
        except Exception as exc:
          code = exc.code if isinstance(exc, EngineCommandError) else "queue_source_unavailable"
          self.repository.invalidate_queue_item(item_id, code, str(exc))
          self._publish_queue_changed()
          continue
        control = SessionControl()
        with self._lock:
          if not self._queue_auto_advance or self._active_session_id is not None or self._shutting_down:
            return
          try:
            session_id = self.repository.claim_queue_item(
              item_id,
              NewSession(
                source_kind="file",
                source_display_name=str(item["display_name"]),
                source_fingerprint=fingerprint.value,
                source_path=str(source_path),
                model=MODEL_ID,
                model_revision=MODEL_REVISION,
                language=DEFAULT_CONFIG.cli.default_language,
                sample_rate=SAMPLE_RATE,
                title=source_path.stem,
                config={"vad": asdict(DEFAULT_CONFIG.vad), "transcription": asdict(DEFAULT_CONFIG.transcription)},
              ),
            )
          except RepositoryError:
            continue
          thread = Thread(
            target=self._run_session,
            args=(session_id, source_path, None, fingerprint, control, 0, 0),
            daemon=True,
            name=f"reco-session-{session_id}",
          )
          self._active_session_id = session_id
          self._active_control = control
          self._active_thread = thread
          self._active_queue_origin = True
          self._queue_scheduler_active = False
          thread.start()
        self._publish_queue_changed()
        return
      with self._lock:
        self._queue_auto_advance = False
      self._publish_queue_changed()
    finally:
      with self._lock:
        self._queue_scheduler_active = False

  def list_audio_inputs(self) -> list[dict[str, object]]:
    """Return current input-capable PortAudio devices."""

    try:
      devices = sd.query_devices()
    except sd.PortAudioError as exc:
      raise EngineCommandError("audio_unavailable", f"Could not list audio inputs: {exc}") from exc
    return [
      {"id": index, "name": str(device["name"]), "channels": int(device["max_input_channels"])}
      for index, device in enumerate(devices)
      if int(device["max_input_channels"]) > 0
    ]

  def _run_session(
    self,
    session_id: str,
    source_path: Path | None,
    device: object,
    fingerprint: object,
    control: SessionControl,
    initial_sample: int = 0,
    segment_offset: int = 0,
  ) -> None:
    try:
      running_receipt = self.repository.set_state(session_id, SessionState.RUNNING)
      self._emit("session.stateChanged", session_id, self._state_receipt(running_receipt))
      service, worker = self.runtime.acquire()
      total_audio_ms = None
      if source_path is None:
        audio_input = MicrophoneInput(
          device=device if isinstance(device, int | str) else None,
          start_sample=initial_sample,
        )
      else:
        expected_identity = getattr(fingerprint, "identity", None)
        total_audio_ms = audio_file_duration_ms(source_path)
        audio_input = LocalAudioFileInput(
          source_path,
          expected_identity=expected_identity,
          start_sample=initial_sample,
        )
      document = run_transcription(
        audio_input,
        SileroVadEngine(model=OnnxSileroProbabilityModel(ensure_silero_vad_asset(self.model_manager.vad_asset_path))),
        service,
        TranscriptModelMetadata(
          path=str(self.model_manager.model_directory), language=DEFAULT_CONFIG.cli.default_language
        ),
        asr_worker=worker,
        progress_callback=lambda progress: self._publish_progress(session_id, progress, total_audio_ms),
        segment_callback=lambda segment: self._persist_segment(
          session_id,
          replace(segment, index=segment.index + segment_offset),
        ),
        session_started_monotonic=monotonic(),
        initial_sample=initial_sample,
        control=control,
      )
      if control.cancel_requested():
        state, reason = SessionState.STOPPED, "userCancel"
        self.runtime.invalidate()
      elif control.stop_requested() and control.stop_reason == "userPause":
        resume_sample = round(document.timing.media_duration_ms * SAMPLE_RATE / 1000)
        paused_receipt = self.repository.pause_session(session_id, resume_sample)
        self._emit(
          "session.stateChanged",
          session_id,
          self._state_receipt(paused_receipt),
        )
        return
      elif control.stop_requested():
        state, reason = SessionState.STOPPED, control.stop_reason
      else:
        state, reason = SessionState.COMPLETED, "naturalEnd"
      terminal_receipt = self.repository.set_state(session_id, state, end_reason=reason)
      self._emit(
        "session.completed",
        session_id,
        {**self._state_receipt(terminal_receipt), "endReason": reason},
      )
    except BaseException as exc:
      code = exc.code if isinstance(exc, EngineCommandError) else "transcription_failed"
      failed_receipt = None
      with _ignore_repository_error():
        failed_receipt = self.repository.set_state(
          session_id,
          SessionState.FAILED,
          end_reason=code,
          error_code=code,
          error_message=str(exc),
        )
      self._emit(
        "session.failed",
        session_id,
        {
          **(self._state_receipt(failed_receipt) if failed_receipt is not None else {}),
          "code": code,
          "message": str(exc),
          "recoverable": True,
        },
      )
      with _ignore_repository_error():
        self.runtime.invalidate()
    finally:
      should_advance = False
      with self._lock:
        if self._active_session_id == session_id:
          was_queue_origin = self._active_queue_origin
          self._active_session_id = None
          self._active_control = None
          self._active_thread = None
          self._active_queue_origin = False
          if was_queue_origin:
            if control.stop_reason != "userPause":
              self._paused_queue_sessions.discard(session_id)
            should_advance = self._queue_auto_advance and not self._shutting_down
      self._emit("history.changed", session_id, {"sessionId": session_id})
      if should_advance:
        self._schedule_next_queue_item()

  def _persist_segment(self, session_id: str, segment: TranscriptSegment) -> None:
    receipt = self.repository.append_segment(session_id, segment)
    self._emit(
      "segment.persisted",
      session_id,
      {
        "segment": {
          "segmentIndex": receipt.segment.index,
          "startSample": receipt.segment.start_sample,
          "endSample": receipt.segment.end_sample,
          "text": receipt.segment.text,
        },
        "rowVersion": receipt.row_version,
        "totalSegments": receipt.total_segments,
        "recognizedSegments": receipt.recognized_segments,
        "characters": receipt.characters,
        "mediaDurationMs": receipt.media_duration_ms,
      },
    )

  def _publish_progress(
    self,
    session_id: str,
    progress: TranscriptionProgress,
    total_audio_ms: int | None = None,
  ) -> None:
    self._emit(
      "session.progress",
      session_id,
      {
        "processedAudioMs": progress.processed_audio_ms,
        "totalAudioMs": total_audio_ms,
        "totalSegments": progress.total_segments,
        "recognizedSegments": progress.recognized_segments,
        "queueDepth": progress.queue_depth,
      },
    )

  @staticmethod
  def _state_receipt(receipt: SessionMutationReceipt) -> dict[str, object]:
    return {
      "state": receipt.state.value,
      "rowVersion": receipt.row_version,
      "totalSegments": receipt.total_segments,
      "recognizedSegments": receipt.recognized_segments,
      "characters": receipt.characters,
      "mediaDurationMs": receipt.media_duration_ms,
      "endedAt": receipt.ended_at,
    }

  def _require_active(self, session_id: str) -> SessionControl:
    with self._lock:
      if self._active_session_id != session_id or self._active_control is None:
        raise EngineCommandError("session_not_active", f"Session is not active: {session_id}")
      return self._active_control

  def _emit(self, event: str, session_id: str | None, payload: Mapping[str, object]) -> None:
    if self._event_callback is not None:
      self._event_callback(event, session_id, payload)

  def _publish_queue_changed(self) -> dict[str, object]:
    snapshot = self.queue_state()
    self._emit("queue.changed", None, snapshot)
    return snapshot


def _restore_device_id(value: str) -> int | str:
  return int(value) if value.isdecimal() else value


class _ignore_repository_error:
  def __enter__(self) -> None:
    return None

  def __exit__(self, exc_type: object, exc: object, traceback: object) -> bool:
    return exc is not None


def _camel(value: object) -> object:
  if isinstance(value, dict):
    return {_camel_key(str(key)): _camel(item) for key, item in value.items()}
  if isinstance(value, list | tuple):
    return [_camel(item) for item in value]
  return value


def _camel_key(value: str) -> str:
  head, *tail = value.split("_")
  return head + "".join(part.capitalize() for part in tail)

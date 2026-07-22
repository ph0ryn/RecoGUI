"""ASR-only RASR worker process."""

from __future__ import annotations

import argparse
import gc
import logging
import os
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from importlib.metadata import PackageNotFoundError, version
from math import isfinite
from pathlib import Path
from threading import Event, Thread
from typing import BinaryIO, Protocol, cast

import numpy as np

from reco_worker.config import DEFAULT_TRANSCRIPTION_CONFIG, TranscriptionConfig
from reco_worker.errors import AsrRuntimeError
from reco_worker.model_catalog import ModelCatalog, ModelReference
from reco_worker.models import SpeechSegment, TranscriptionDiagnostics, TranscriptionResult
from reco_worker.protocol import AsrProtocolError, Frame, FrameKind, FrameWriter, read_frame
from reco_worker.transcription import LocalAsrTranscriptionService

CAPABILITIES = (
  "models.list",
  "model.load",
  "segment.transcribe",
  "model.unload",
  "shutdown",
)
_MAX_U32 = (1 << 32) - 1
_MAX_U64 = (1 << 64) - 1
_MAX_SEGMENT_SAMPLES = 960_000
_COMMON_REQUEST_FIELDS = frozenset({"requestId", "operation"})
_OPERATION_FIELDS = {
  "models.list": _COMMON_REQUEST_FIELDS,
  "model.load": _COMMON_REQUEST_FIELDS | {"repoId", "revision"},
  "segment.transcribe": _COMMON_REQUEST_FIELDS
  | {
    "sessionId",
    "runId",
    "jobId",
    "segmentIndex",
    "startSample",
    "endSample",
    "sampleRate",
    "splitReason",
    "language",
    "vad",
    "options",
  },
  "model.unload": _COMMON_REQUEST_FIELDS,
  "shutdown": _COMMON_REQUEST_FIELDS,
}
_VAD_FIELDS = frozenset({"meanProbability", "peakProbability", "speechRatio"})
_OPTION_FIELDS = frozenset(
  {
    "generationTokensPerSecond",
    "minGenerationTokens",
    "maxGenerationTokens",
    "temperature",
    "repetitionPenalty",
  }
)


class AsrService(Protocol):
  """Model operations needed by the worker dispatcher."""

  model_load_ms: int

  def load_model(self) -> object:
    """Load and return one ASR model runtime."""

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    """Transcribe one normalized speech segment."""


class AsrServiceFactory(Protocol):
  """Construct an ASR service without coupling the worker to MLX in tests."""

  def __call__(
    self,
    model_path: str | Path,
    *,
    language: str | None,
    model: object | None = None,
    config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG,
  ) -> AsrService:
    """Create a service for model loading or one segment transcription."""


class WorkerRequestError(ValueError):
  """A valid Request frame whose operation cannot be completed."""

  def __init__(self, code: str, message: str, *, recoverable: bool) -> None:
    super().__init__(message)
    self.code = code
    self.recoverable = recoverable


@dataclass(frozen=True)
class DispatchResult:
  """Successful operation result and lifecycle signal."""

  value: dict[str, object]
  should_shutdown: bool = False


@dataclass(frozen=True)
class LoadedModel:
  """The sole model lease retained by one worker process."""

  reference: ModelReference
  snapshot_path: Path
  runtime: object
  load_ms: int


class WorkerDispatcher:
  """Validate and execute the five RASR v1 worker operations."""

  def __init__(
    self,
    *,
    model_catalog: ModelCatalog | None = None,
    service_factory: AsrServiceFactory = LocalAsrTranscriptionService,
    cache_clearer: Callable[[], None] | None = None,
  ) -> None:
    self._model_catalog = model_catalog or ModelCatalog()
    self._service_factory = service_factory
    self._cache_clearer = cache_clearer or _clear_mlx_cache
    self._loaded: LoadedModel | None = None

  def dispatch(self, operation: str, metadata: Mapping[str, object], binary: bytes) -> DispatchResult:
    """Execute one already-correlated request."""

    if operation not in CAPABILITIES:
      raise AsrProtocolError("unsupportedOperation", f"Unsupported ASR worker operation: {operation}")
    _reject_unknown_fields(metadata, _OPERATION_FIELDS[operation], context=operation)
    if operation != "segment.transcribe" and binary:
      raise AsrProtocolError("unexpectedBinary", f"{operation} requests must not contain binary data")
    if operation == "models.list":
      return DispatchResult(self._list_models(metadata))
    if operation == "model.load":
      return DispatchResult(self._load_model(metadata))
    if operation == "segment.transcribe":
      return DispatchResult(self._transcribe(metadata, binary))
    if operation == "model.unload":
      return DispatchResult(self._unload_model(metadata))
    return DispatchResult({}, should_shutdown=True)

  def close(self) -> None:
    """Release the model lease and its MLX allocator cache."""

    if self._loaded is None:
      return
    loaded = self._loaded
    self._loaded = None
    del loaded
    gc.collect()
    self._cache_clearer()

  def _list_models(self, metadata: Mapping[str, object]) -> dict[str, object]:
    del metadata
    public_models = self._model_catalog.refresh()
    if self._model_catalog.error is not None:
      raise WorkerRequestError(
        "modelScanFailed",
        self._model_catalog.error,
        recoverable=True,
      )
    models: list[dict[str, object]] = []
    for public_model in public_models:
      repository_id = _require_string(public_model, "repoId")
      revision = _require_string(public_model, "revision")
      reference = ModelReference(repository_id, revision)
      resolved = self._model_catalog.resolve(reference)
      if resolved is None:
        raise WorkerRequestError(
          "modelScanFailed",
          f"Cached model disappeared during cache scan: {repository_id}@{revision}",
          recoverable=True,
        )
      models.append(dict(public_model))
    return {"models": models}

  def _load_model(self, metadata: Mapping[str, object]) -> dict[str, object]:
    repository_id = _require_string(metadata, "repoId")
    revision = _require_string(metadata, "revision")

    self._model_catalog.refresh()
    if self._model_catalog.error is not None:
      raise WorkerRequestError(
        "modelScanFailed",
        self._model_catalog.error,
        recoverable=True,
      )
    reference = ModelReference(repository_id, revision)
    resolved = self._model_catalog.resolve(reference)
    if resolved is None:
      raise WorkerRequestError(
        "modelUnavailable",
        f"Model revision is not available in the Hugging Face cache: {repository_id}@{revision}",
        recoverable=True,
      )
    resolved = resolved.resolve()

    if self._loaded is not None:
      if self._loaded.reference == reference and self._loaded.snapshot_path == resolved:
        return self._loaded_model_result(self._loaded, reused=True)
      self.close()

    try:
      service = self._service_factory(resolved, language=None)
      runtime = service.load_model()
    except (OSError, AsrRuntimeError, RuntimeError, ValueError) as exc:
      raise WorkerRequestError("modelLoadFailed", str(exc), recoverable=True) from exc
    loaded = LoadedModel(reference, resolved, runtime, service.model_load_ms)
    self._loaded = loaded
    return self._loaded_model_result(loaded, reused=False)

  def _transcribe(self, metadata: Mapping[str, object], binary: bytes) -> dict[str, object]:
    if self._loaded is None:
      raise WorkerRequestError("modelNotLoaded", "Load a model before transcribing audio", recoverable=True)

    session_id = _require_string(metadata, "sessionId")
    run_id = _require_string(metadata, "runId")
    job_id = _require_string(metadata, "jobId")
    segment_index = _require_integer(metadata, "segmentIndex", minimum=0, maximum=_MAX_U32)
    start_sample = _require_integer(metadata, "startSample", minimum=0, maximum=_MAX_U64)
    end_sample = _require_integer(metadata, "endSample", minimum=1, maximum=_MAX_U64)
    if end_sample <= start_sample:
      raise WorkerRequestError("invalidRequest", "endSample must be greater than startSample", recoverable=False)
    if _require_integer(metadata, "sampleRate", minimum=1, maximum=_MAX_U32) != 16_000:
      raise WorkerRequestError("invalidRequest", "sampleRate must be 16000", recoverable=False)
    split_reason = _require_string(metadata, "splitReason")
    if split_reason not in {"silence", "adaptiveSplit", "endOfInput"}:
      raise WorkerRequestError(
        "invalidRequest",
        "splitReason must be silence, adaptiveSplit, or endOfInput",
        recoverable=False,
      )
    language = _optional_language(metadata)
    _validate_vad(_require_object(metadata, "vad"))
    config = _transcription_config(_require_object(metadata, "options"))

    sample_count = end_sample - start_sample
    if sample_count > _MAX_SEGMENT_SAMPLES:
      raise AsrProtocolError(
        "segmentTooLarge",
        f"segment.transcribe exceeds {_MAX_SEGMENT_SAMPLES} samples",
      )
    expected_bytes = sample_count * np.dtype("<f4").itemsize
    if len(binary) != expected_bytes:
      raise AsrProtocolError(
        "binaryLengthMismatch",
        f"segment.transcribe requires {expected_bytes} binary bytes, received {len(binary)}",
      )
    audio = np.frombuffer(binary, dtype="<f4").astype(np.float32, copy=True)
    if not np.isfinite(audio).all():
      raise WorkerRequestError("invalidRequest", "PCM samples must be finite", recoverable=False)
    segment = SpeechSegment(start_sample=start_sample, audio=audio, sample_rate=16_000)
    try:
      service = self._service_factory(
        self._loaded.snapshot_path,
        language=language,
        model=self._loaded.runtime,
        config=config,
      )
      result = service.transcribe(segment)
    except (OSError, AsrRuntimeError, RuntimeError, ValueError) as exc:
      raise WorkerRequestError("transcriptionFailed", str(exc), recoverable=True) from exc
    return {
      "sessionId": session_id,
      "runId": run_id,
      "jobId": job_id,
      "segmentIndex": segment_index,
      "text": result.text,
      "rawText": result.raw_text,
      "language": result.language,
      "diagnostics": _diagnostics(result.diagnostics),
    }

  def _unload_model(self, metadata: Mapping[str, object]) -> dict[str, object]:
    del metadata
    unloaded = self._loaded is not None
    self.close()
    return {"unloaded": unloaded}

  @staticmethod
  def _loaded_model_result(loaded: LoadedModel, *, reused: bool) -> dict[str, object]:
    return {
      "repoId": loaded.reference.repo_id,
      "revision": loaded.reference.revision,
      "loadMs": loaded.load_ms,
      "reused": reused,
    }


class AsrWorkerServer:
  """Serve RASR v1 on one inherited full-duplex stream."""

  def __init__(
    self,
    *,
    dispatcher: WorkerDispatcher | None = None,
    heartbeat_interval_seconds: float | None = 2.0,
    logger: logging.Logger | None = None,
  ) -> None:
    if heartbeat_interval_seconds is not None and heartbeat_interval_seconds <= 0:
      raise ValueError("Heartbeat interval must be positive")
    self._dispatcher = dispatcher or WorkerDispatcher()
    self._heartbeat_interval_seconds = heartbeat_interval_seconds
    self._logger = logger or logging.getLogger(__name__)

  def serve(self, stream: BinaryIO) -> int:
    """Run until graceful shutdown, clean host disconnect, or protocol failure."""

    writer = FrameWriter(stream)
    stop_heartbeat = Event()
    heartbeat_thread: Thread | None = None
    try:
      writer.write(FrameKind.HELLO, _hello())
      if self._heartbeat_interval_seconds is not None:
        heartbeat_thread = Thread(
          target=self._send_heartbeats,
          args=(writer, stop_heartbeat, self._heartbeat_interval_seconds),
          name="reco-asr-heartbeat",
          daemon=True,
        )
        heartbeat_thread.start()

      seen_request_ids: set[str] = set()
      while True:
        frame = read_frame(stream)
        if frame is None:
          return 0
        if frame.kind is FrameKind.HEARTBEAT:
          _validate_heartbeat(frame)
          continue
        if frame.kind is not FrameKind.REQUEST:
          raise AsrProtocolError("invalidDirection", f"Host must not send {frame.kind.name} frames")

        request_id, operation = _request_identity(frame.metadata)
        if operation != "segment.transcribe" and frame.binary:
          raise AsrProtocolError("unexpectedBinary", f"{operation} requests must not contain binary data")
        if request_id in seen_request_ids:
          raise AsrProtocolError("duplicateRequestId", f"requestId has already been used: {request_id}")
        seen_request_ids.add(request_id)

        try:
          dispatched = self._dispatcher.dispatch(operation, frame.metadata, frame.binary)
        except WorkerRequestError as exc:
          self._write_error(writer, request_id, operation, exc)
          continue
        except AsrProtocolError:
          raise
        except Exception as exc:
          self._logger.exception("Unhandled ASR worker operation failure")
          self._write_error(
            writer,
            request_id,
            operation,
            WorkerRequestError("internalError", str(exc), recoverable=False),
          )
          continue

        writer.write(
          FrameKind.RESPONSE,
          {
            "requestId": request_id,
            "operation": operation,
            "ok": True,
            "result": dispatched.value,
          },
        )
        if dispatched.should_shutdown:
          return 0
    except AsrProtocolError as exc:
      self._logger.error("RASR protocol failure [%s]: %s", exc.code, exc)
      return 2
    finally:
      stop_heartbeat.set()
      if heartbeat_thread is not None:
        heartbeat_thread.join(timeout=1)
      try:
        self._dispatcher.close()
      except Exception:
        self._logger.exception("Could not release the ASR model during worker shutdown")

  def _send_heartbeats(self, writer: FrameWriter, stop: Event, interval_seconds: float) -> None:
    while not stop.wait(interval_seconds):
      try:
        writer.write(FrameKind.HEARTBEAT, {})
      except (AsrProtocolError, OSError):
        self._logger.exception("Could not send an ASR worker heartbeat")
        return

  @staticmethod
  def _write_error(
    writer: FrameWriter,
    request_id: str,
    operation: str,
    error: WorkerRequestError,
  ) -> None:
    writer.write(
      FrameKind.RESPONSE,
      {
        "requestId": request_id,
        "operation": operation,
        "ok": False,
        "error": {
          "code": error.code,
          "message": str(error),
          "recoverable": error.recoverable,
        },
      },
    )


def main(argv: Sequence[str] | None = None) -> int:
  """Run the worker over the inherited RASR file descriptor."""

  parser = argparse.ArgumentParser(prog="reco-asr-worker", description=__doc__)
  parser.add_argument("--ipc-fd", type=_file_descriptor, default=3, help="inherited full-duplex RASR descriptor")
  args = parser.parse_args(argv)
  logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="%(levelname)s %(name)s: %(message)s")
  try:
    with os.fdopen(args.ipc_fd, "r+b", buffering=0) as stream:
      return AsrWorkerServer().serve(cast(BinaryIO, stream))
  except OSError as exc:
    logging.getLogger(__name__).error("Could not open RASR descriptor %d: %s", args.ipc_fd, exc)
    return 2


def _hello() -> dict[str, object]:
  try:
    worker_version = version("reco-worker")
  except PackageNotFoundError:
    worker_version = "0.2.0"
  return {
    "workerVersion": worker_version,
    "capabilities": list(CAPABILITIES),
  }


def _request_identity(metadata: Mapping[str, object]) -> tuple[str, str]:
  try:
    request_id = _require_string(metadata, "requestId")
    operation = _require_string(metadata, "operation")
  except WorkerRequestError as exc:
    raise AsrProtocolError("invalidRequestEnvelope", str(exc)) from exc
  return request_id, operation


def _validate_heartbeat(frame: Frame) -> None:
  if frame.binary:
    raise AsrProtocolError("unexpectedBinary", "Heartbeat frames must not contain binary data")
  _reject_unknown_fields(frame.metadata, frozenset(), context="Heartbeat")


def _require_string(value: Mapping[str, object], name: str) -> str:
  result = value.get(name)
  if not isinstance(result, str) or not result.strip():
    raise WorkerRequestError("invalidRequest", f"{name} must be a non-empty string", recoverable=False)
  return result


def _require_integer(
  value: Mapping[str, object],
  name: str,
  *,
  minimum: int,
  maximum: int,
) -> int:
  result = value.get(name)
  if not isinstance(result, int) or isinstance(result, bool) or not minimum <= result <= maximum:
    raise WorkerRequestError(
      "invalidRequest",
      f"{name} must be an integer between {minimum} and {maximum}",
      recoverable=False,
    )
  return result


def _require_object(value: Mapping[str, object], name: str) -> Mapping[str, object]:
  result = value.get(name)
  if not isinstance(result, dict) or not all(isinstance(key, str) for key in result):
    raise WorkerRequestError("invalidRequest", f"{name} must be a JSON object", recoverable=False)
  return cast(dict[str, object], result)


def _reject_unknown_fields(value: Mapping[str, object], allowed: frozenset[str], *, context: str) -> None:
  unknown = sorted(set(value) - allowed)
  if unknown:
    names = ", ".join(unknown)
    raise AsrProtocolError("unknownField", f"{context} contains unknown fields: {names}")


def _optional_language(metadata: Mapping[str, object]) -> str | None:
  value = metadata.get("language")
  if value is None:
    return None
  if not isinstance(value, str) or not value.strip():
    raise WorkerRequestError("invalidRequest", "language must be null or a non-empty string", recoverable=False)
  return value


def _transcription_config(value: Mapping[str, object]) -> TranscriptionConfig:
  _reject_unknown_fields(value, _OPTION_FIELDS, context="segment.transcribe options")
  try:
    return TranscriptionConfig(
      generation_tokens_per_sec=_number(value, "generationTokensPerSecond"),
      min_generation_tokens=_integer(value, "minGenerationTokens"),
      max_generation_tokens=_integer(value, "maxGenerationTokens"),
      temperature=_number(value, "temperature"),
      repetition_penalty=_optional_number(value, "repetitionPenalty"),
    )
  except ValueError as exc:
    raise WorkerRequestError("invalidRequest", str(exc), recoverable=False) from exc


def _integer(value: Mapping[str, object], name: str) -> int:
  result = value.get(name)
  if not isinstance(result, int) or isinstance(result, bool):
    raise WorkerRequestError("invalidRequest", f"{name} must be an integer", recoverable=False)
  return result


def _number(value: Mapping[str, object], name: str) -> float:
  result = value.get(name)
  if not isinstance(result, int | float) or isinstance(result, bool):
    raise WorkerRequestError("invalidRequest", f"{name} must be a number", recoverable=False)
  return float(result)


def _optional_number(value: Mapping[str, object], name: str) -> float | None:
  if name not in value:
    raise WorkerRequestError("invalidRequest", f"{name} is required", recoverable=False)
  result = value[name]
  if result is None:
    return None
  if not isinstance(result, int | float) or isinstance(result, bool):
    raise WorkerRequestError("invalidRequest", f"{name} must be null or a number", recoverable=False)
  return float(result)


def _validate_vad(value: Mapping[str, object]) -> None:
  _reject_unknown_fields(value, _VAD_FIELDS, context="segment.transcribe vad")
  probabilities = [_number(value, name) for name in ("meanProbability", "peakProbability", "speechRatio")]
  if not all(isfinite(probability) and 0 <= probability <= 1 for probability in probabilities):
    raise WorkerRequestError("invalidRequest", "VAD diagnostics must be finite probabilities", recoverable=False)


def _diagnostics(value: TranscriptionDiagnostics) -> dict[str, object]:
  return {
    "maxTokens": value.max_tokens,
    "generationTokens": value.generation_tokens,
    "promptTokens": value.prompt_tokens,
    "totalTokens": value.total_tokens,
    "modelTotalTimeMs": value.model_total_time_ms,
    "retryCount": value.retry_count,
    "tokenLimitReached": value.token_limit_reached,
    "warning": value.warning,
  }


def _file_descriptor(value: str) -> int:
  try:
    result = int(value)
  except ValueError as exc:
    raise argparse.ArgumentTypeError("file descriptor must be an integer") from exc
  if result < 0:
    raise argparse.ArgumentTypeError("file descriptor must not be negative")
  return result


def _clear_mlx_cache() -> None:
  import mlx.core as mx

  mx.clear_cache()


if __name__ == "__main__":
  raise SystemExit(main())

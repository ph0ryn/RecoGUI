"""Stdout-first CLI for local streaming transcription and durable recording."""

from __future__ import annotations

import argparse
from collections.abc import Callable
from contextlib import nullcontext
from dataclasses import asdict
from datetime import datetime
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path
from time import monotonic

from reco.audio import (
  SAMPLE_RATE,
  LocalAudioFileInput,
  MicrophoneInput,
  resolve_microphone_device_name,
  validate_audio_file,
)
from reco.config import DEFAULT_CLI_CONFIG, DEFAULT_CONFIG
from reco.errors import RecoError
from reco.models import RunStatus, TranscriptModelMetadata, TranscriptSegment
from reco.pipeline import run_transcription, start_asr_worker
from reco.recording import (
  RecordingSegment,
  RecordingSession,
  RecordingSource,
  RecordingSummary,
  SessionRecorder,
  fingerprint_file_snapshot,
)
from reco.transcription import LocalAsrTranscriptionService
from reco.ui import RecoRichUi, SessionDisplayInfo, print_error, print_status
from reco.vad import SileroVadEngine

DEFAULT_RECORDING_DATABASE = Path.home() / "Library" / "Application Support" / "com.ph0ryn.recogui" / "reco-cli.sqlite3"
try:
  RECO_VERSION = version("reco")
except PackageNotFoundError:
  RECO_VERSION = "unknown"


def build_parser() -> argparse.ArgumentParser:
  """Build the Reco CLI contract parser."""

  parser = argparse.ArgumentParser(
    prog="reco",
    description="Transcribe microphone input or a local audio file.",
    allow_abbrev=False,
  )
  parser.add_argument("audio_file", nargs="?", type=Path, help="Optional local audio file to transcribe.")
  parser.add_argument(
    "--model",
    default=DEFAULT_CLI_CONFIG.default_model,
    help="MLX ASR model path or Hugging Face repo ID.",
  )
  parser.add_argument(
    "--model-revision",
    metavar="REVISION",
    help="Hugging Face model revision. The default model uses Reco's pinned commit when omitted.",
  )
  parser.add_argument(
    "--language",
    default=DEFAULT_CLI_CONFIG.default_language,
    help="Transcription language passed to the local ASR model.",
  )
  parser.add_argument(
    "--record",
    action="store_true",
    help="Deprecated compatibility flag; every run is durably recorded.",
  )
  parser.add_argument(
    "--record-database",
    type=Path,
    metavar="DATABASE",
    help="SQLite recording database path; implies --record.",
  )
  return parser


def main(argv: list[str] | None = None) -> int:
  """Run the Reco CLI for microphone input or local file transcription."""

  parser = build_parser()
  args = parser.parse_args(argv)

  try:
    model = str(args.model).strip() or DEFAULT_CLI_CONFIG.default_model
    if model.startswith("~"):
      model = str(Path(model).expanduser())
    model_revision = _resolve_model_revision(model, args.model_revision)
    language = str(args.language).strip() or DEFAULT_CLI_CONFIG.default_language
    session_started_monotonic = monotonic()
    session_started_at = datetime.now().astimezone()
    if args.audio_file is not None:
      validate_audio_file(args.audio_file)
    microphone_device_name = resolve_microphone_device_name() if args.audio_file is None else None
    model_metadata = TranscriptModelMetadata(
      path=str(model),
      language=language,
      revision=model_revision,
    )
    session_info = SessionDisplayInfo(
      source="microphone" if args.audio_file is None else "file",
      source_path=str(args.audio_file) if args.audio_file is not None else microphone_device_name,
      model_path=str(model),
      language=language,
      started_at=session_started_at,
    )
    recording_database = args.record_database or DEFAULT_RECORDING_DATABASE
    file_fingerprint = (
      fingerprint_file_snapshot(args.audio_file)
      if args.audio_file is not None and recording_database is not None
      else None
    )
    audio_input = (
      MicrophoneInput()
      if args.audio_file is None
      else LocalAudioFileInput(
        args.audio_file,
        expected_identity=file_fingerprint.identity if file_fingerprint is not None else None,
      )
    )
    recorder = _build_recorder(
      recording_database,
      audio_file=args.audio_file,
      microphone_device_name=microphone_device_name,
      model=model,
      model_revision=model_revision,
      language=language,
      started_at=session_started_at,
      source_fingerprint=file_fingerprint.value if file_fingerprint is not None else None,
    )
    recorder_context = recorder if recorder is not None else nullcontext(None)

    with recorder_context as active_recorder:
      vad_engine = SileroVadEngine()
      print_status(f"Loading ASR model: {model}")
      transcription_service = LocalAsrTranscriptionService(
        model_path=model,
        language=language,
        revision=model_revision,
      )
      asr_worker = start_asr_worker(transcription_service)
      try:
        asr_worker.wait_until_ready()
        print_status("ASR model loaded")
        with RecoRichUi(session_info) as ui:
          result = run_transcription(
            audio_input=audio_input,
            vad_engine=vad_engine,
            transcription_service=transcription_service,
            model_metadata=model_metadata,
            asr_worker=asr_worker,
            progress_callback=ui.update,
            segment_callback=_recording_callback(active_recorder),
            session_started_at=session_started_at,
            session_started_monotonic=session_started_monotonic,
          )
          if active_recorder is not None:
            summary = RecordingSummary.from_document(result)
            if result.status is RunStatus.INTERRUPTED:
              active_recorder.interrupt(summary=summary)
            else:
              active_recorder.complete(summary=summary)
          ui.complete(result, _recording_label(active_recorder))
      except BaseException as exc:
        if asr_worker.thread.is_alive():
          try:
            asr_worker.stop(
              cancel_pending=True,
              timeout=DEFAULT_CONFIG.transcription.failed_worker_shutdown_timeout_seconds,
            )
          except BaseException as cleanup_error:
            exc.add_note(f"Could not stop ASR worker cleanly: {cleanup_error}")
        raise
    return 130 if result.status is RunStatus.INTERRUPTED else 0
  except KeyboardInterrupt as exc:
    for note in getattr(exc, "__notes__", ()):
      print_error(f"reco: note: {note}")
    return 130
  except RecoError as exc:
    print_error(f"reco: {exc}")
    for note in getattr(exc, "__notes__", ()):
      print_error(f"reco: note: {note}")
    return 1


def _build_recorder(
  database_path: Path | None,
  *,
  audio_file: Path | None,
  microphone_device_name: str | None,
  model: str,
  model_revision: str | None,
  language: str,
  started_at: datetime,
  source_fingerprint: str | None,
) -> SessionRecorder | None:
  if database_path is None:
    return None
  source_display_name = audio_file.name if audio_file is not None else microphone_device_name or "microphone"
  source = RecordingSource(
    kind="microphone" if audio_file is None else "file",
    display_name=source_display_name,
    fingerprint=source_fingerprint,
  )
  session = RecordingSession(
    source=source,
    model=model,
    model_revision=model_revision,
    reco_version=RECO_VERSION,
    language=language,
    sample_rate=SAMPLE_RATE,
    config={
      "transcription": asdict(DEFAULT_CONFIG.transcription),
      "vad": asdict(DEFAULT_CONFIG.vad),
    },
    started_at=started_at,
  )
  return SessionRecorder(database_path, session)


def _resolve_model_revision(model: str, requested_revision: str | None) -> str | None:
  is_local_path = Path(model).exists()
  if requested_revision is not None:
    revision = requested_revision.strip()
    if not revision:
      raise RecoError("Model revision must not be empty.")
    if is_local_path:
      raise RecoError("Model revisions apply only to Hugging Face repositories, not local model paths.")
    return revision
  if model == DEFAULT_CLI_CONFIG.default_model:
    if is_local_path:
      raise RecoError(f"Local path '{model}' shadows Reco's default Hugging Face model ID.")
    return DEFAULT_CLI_CONFIG.default_model_revision
  return None


def _recording_callback(recorder: SessionRecorder | None) -> Callable[[TranscriptSegment], None] | None:
  if recorder is None:
    return None

  def record(segment: TranscriptSegment) -> None:
    recorder.record_segment(RecordingSegment.from_transcript(segment))

  return record


def _recording_label(recorder: SessionRecorder | None) -> str | None:
  if recorder is None:
    return None
  return f"{recorder.database_path}  run {recorder.run_id}"


if __name__ == "__main__":
  raise SystemExit(main())

from __future__ import annotations

from datetime import UTC, datetime
from io import StringIO
from typing import Any, cast

from rich.console import Console

from reco.audio import SourceMetadata
from reco.models import (
  RunStatus,
  SplitReason,
  TranscriptDocument,
  TranscriptionDiagnostics,
  TranscriptModelMetadata,
  TranscriptSegment,
  TranscriptTiming,
  VadDiagnostics,
)
from reco.pipeline import TranscriptionProgress
from reco.ui import RecoRichUi, SessionDisplayInfo


def make_document(status: RunStatus = RunStatus.COMPLETED) -> TranscriptDocument:
  segment = TranscriptSegment(
    index=0,
    start_sample=16_000,
    end_sample=32_000,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text="[bold]literal[/bold] 日本語",
    raw_text="[bold]literal[/bold] 日本語",
    vad=VadDiagnostics(mean_probability=0.8, peak_probability=0.95, speech_ratio=0.7),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=8),
    decode_ms=800,
  )
  return TranscriptDocument(
    source=SourceMetadata(kind="file", path="lecture.wav"),
    model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
    status=status,
    timing=TranscriptTiming(
      command_started_at="2026-07-18T00:00:00+00:00",
      command_ended_at="2026-07-18T00:00:01+00:00",
      command_wall_time_ms=1000,
      pipeline_wall_time_ms=900,
      media_duration_ms=2000,
      model_load_ms=100,
      decode_time_ms=800,
      pipeline_rtf=0.45,
      decode_rtf=0.4,
    ),
    max_queue_depth=1,
    segments=(segment,),
  )


def make_ui(buffer: StringIO) -> RecoRichUi:
  return RecoRichUi(
    SessionDisplayInfo(
      source="file",
      source_path="lecture.wav",
      model_path="fixed-model",
      language="Japanese",
      started_at=datetime(2026, 7, 18, 12, 34, 56, tzinfo=UTC),
    ),
    console=Console(file=buffer, force_terminal=False, color_system=None, width=100),
  )


def test_stdout_style_keeps_startup_metadata_timestamped_text_and_completion() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)
  document = make_document()

  ui.console.print(ui._startup_panel())
  ui.update(
    TranscriptionProgress(
      event="transcript",
      processed_audio_ms=2000,
      total_segments=1,
      recognized_segments=1,
      characters=document.characters,
      queue_depth=0,
      max_queue_depth=1,
      latest_text=document.segments[0].text,
      latest_start_ms=document.segments[0].start_ms,
    )
  )
  ui.complete(document)

  rendered = buffer.getvalue()
  assert "Reco" in rendered
  assert "file: lecture.wav" in rendered
  assert "fixed-model" in rendered
  assert "Japanese" in rendered
  assert "[00:00:01.000] [bold]literal[/bold] 日本語" in rendered
  assert "Completed" in rendered


def test_startup_metadata_treats_rich_markup_as_literal_user_data() -> None:
  buffer = StringIO()
  ui = RecoRichUi(
    SessionDisplayInfo(
      source="file",
      source_path="[red]lecture.wav[/red]",
      model_path="[bold]fixed-model[/bold]",
      language="[green]Japanese[/green]",
      started_at=datetime(2026, 7, 18, 12, 34, 56, tzinfo=UTC),
    ),
    console=Console(file=buffer, force_terminal=False, color_system=None, width=100),
  )

  ui.console.print(ui._startup_panel())

  rendered = buffer.getvalue()
  assert "[red]lecture.wav[/red]" in rendered
  assert "[bold]fixed-model[/bold]" in rendered
  assert "[green]Japanese[/green]" in rendered


def test_transcript_line_escapes_terminal_controls_without_losing_evidence() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)

  ui.update(
    TranscriptionProgress(
      event="transcript",
      processed_audio_ms=1_000,
      total_segments=1,
      recognized_segments=1,
      characters=8,
      queue_depth=0,
      max_queue_depth=1,
      latest_text="literal\\n first\nsecond\x1b[2J\u202e",
      latest_start_ms=1_000,
    )
  )

  assert buffer.getvalue() == r"[00:00:01.000] literal\\n first\nsecond\x1b[2J\u202e" + "\n"


def test_recording_adds_a_completion_note_without_changing_transcript_lines() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)

  ui.complete(make_document(), "history.sqlite3  run example")

  assert "Completed" in buffer.getvalue()
  assert "Recorded" in buffer.getvalue()
  assert "history.sqlite3  run example" in buffer.getvalue()


def test_progress_updates_do_not_force_live_refresh_for_every_audio_frame() -> None:
  class FakeLive:
    refresh_count = 0

    def refresh(self) -> None:
      self.refresh_count += 1

  ui = make_ui(StringIO())
  live = FakeLive()
  ui._live = cast(Any, live)

  for index in range(1_000):
    ui.update(
      TranscriptionProgress(
        event="chunk",
        processed_audio_ms=index * 32,
        total_segments=0,
        recognized_segments=0,
        characters=0,
        queue_depth=0,
        max_queue_depth=0,
      )
    )

  assert live.refresh_count == 0


def test_interrupted_run_uses_a_distinct_terminal_status() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)

  ui.complete(make_document(RunStatus.INTERRUPTED))

  assert "Stopped" in buffer.getvalue()
  assert "Completed" not in buffer.getvalue()


def test_empty_recorded_run_reports_both_terminal_states() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)
  document = make_document()
  empty_document = TranscriptDocument(
    source=document.source,
    model=document.model,
    status=document.status,
    timing=document.timing,
    max_queue_depth=0,
    segments=(),
  )

  ui.complete(empty_document, "history.sqlite3  run example")

  assert "No speech detected" in buffer.getvalue()
  assert "Recorded" in buffer.getvalue()


def test_detected_speech_with_empty_asr_text_has_an_explicit_state() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)
  document = make_document()
  empty_segment = TranscriptSegment(
    index=0,
    start_sample=16_000,
    end_sample=32_000,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text="",
    raw_text="   ",
    vad=VadDiagnostics(mean_probability=0.8, peak_probability=0.95, speech_ratio=0.7),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=1, warning="empty_text"),
  )
  empty_document = TranscriptDocument(
    source=document.source,
    model=document.model,
    status=document.status,
    timing=document.timing,
    max_queue_depth=1,
    segments=(empty_segment,),
  )

  ui.complete(empty_document)

  assert "No transcript recognized" in buffer.getvalue()


def test_completion_updates_all_metrics_before_the_final_live_render() -> None:
  buffer = StringIO()
  ui = make_ui(buffer)
  document = make_document()
  final_metrics: list[str] = []

  class CapturingLive:
    console = ui.console

    def stop(self) -> None:
      final_metrics.append(ui._metrics_line().plain)

  ui._live = cast(Any, CapturingLive())

  ui.complete(document)

  assert len(final_metrics) == 1
  assert "Audio 2.0s" in final_metrics[0]
  assert "Segments 1" in final_metrics[0]
  assert "Recognized 1" in final_metrics[0]
  assert f"Characters {document.characters}" in final_metrics[0]
  assert "Queue 0/1" in final_metrics[0]

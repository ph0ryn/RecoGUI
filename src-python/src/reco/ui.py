"""Rich terminal UI for Reco sessions."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from time import monotonic
from types import TracebackType
from unicodedata import category

from rich import box
from rich.console import Console, ConsoleOptions, Group, RenderResult
from rich.live import Live
from rich.panel import Panel
from rich.spinner import Spinner
from rich.table import Table
from rich.text import Text

from reco.audio import SAMPLE_RATE
from reco.models import RunStatus, TranscriptDocument
from reco.pipeline import TranscriptionProgress


@dataclass(frozen=True)
class SessionDisplayInfo:
  """Static session details shown before transcription starts."""

  source: str
  source_path: str | None
  model_path: str
  language: str
  started_at: datetime


class RecoRichUi:
  """Render startup, live progress, and completion status with Rich."""

  def __init__(self, session: SessionDisplayInfo, console: Console | None = None) -> None:
    self.session = session
    self.console = console or Console()
    self.started_at = monotonic()
    self.processed_audio_ms = 0
    self.total_segments = 0
    self.recognized_segments = 0
    self.characters = 0
    self.queue_depth = 0
    self.max_queue_depth = 0
    self.listening_spinner = Spinner("dots", text=Text(" Listening", style="bold cyan"))
    self._live: Live | None = None

  def __enter__(self) -> RecoRichUi:
    self.console.print(self._startup_panel())
    self._live = Live(
      self,
      console=self.console,
      refresh_per_second=8,
      redirect_stdout=False,
      redirect_stderr=False,
      vertical_overflow="visible",
    )
    self._live.start(refresh=True)
    return self

  def __exit__(
    self,
    exc_type: type[BaseException] | None,
    exc: BaseException | None,
    traceback: TracebackType | None,
  ) -> None:
    del exc_type, exc, traceback
    self._stop_live()

  def __rich_console__(self, console: Console, options: ConsoleOptions) -> RenderResult:
    del console, options
    yield self._live_display()

  def update(self, progress: TranscriptionProgress) -> None:
    """Apply a pipeline progress snapshot to the live display."""

    self.processed_audio_ms = progress.processed_audio_ms
    self.total_segments = progress.total_segments
    self.recognized_segments = progress.recognized_segments
    self.characters = progress.characters
    self.queue_depth = progress.queue_depth
    self.max_queue_depth = progress.max_queue_depth
    if progress.latest_text:
      self._print_transcript_line(progress)

  def complete(self, document: TranscriptDocument, recording: str | None = None) -> None:
    """Render final run status without coupling stdout to persistence."""

    self.processed_audio_ms = document.timing.media_duration_ms
    self.total_segments = document.total_segments
    self.recognized_segments = document.recognized_segments
    self.characters = document.characters
    self.queue_depth = 0
    self.max_queue_depth = document.max_queue_depth
    self._stop_live()
    terminal_message = None
    if not document.segments:
      terminal_message = "No speech detected"
    elif not document.recognized_segments:
      terminal_message = "No transcript recognized"
    if terminal_message is not None:
      lines = [Text(""), Text(terminal_message, style="bold yellow")]
      if recording is not None:
        lines.extend((Text("Recorded", style="bold green"), Text(_escape_terminal_controls(recording), style="cyan")))
      self.console.print(Group(*lines))
      return
    status = (
      Text("Stopped", style="bold yellow")
      if document.status is RunStatus.INTERRUPTED
      else Text("Completed", style="bold green")
    )
    lines: list[Text] = [Text(""), status]
    if recording is not None:
      lines.extend((Text("Recorded", style="bold green"), Text(_escape_terminal_controls(recording), style="cyan")))
    self.console.print(Group(*lines))

  def _startup_panel(self) -> Panel:
    table = Table.grid(padding=(0, 2))
    table.add_column(style="bold cyan", no_wrap=True)
    table.add_column(style="white")
    table.add_row(Text("Source"), Text(_escape_terminal_controls(self._source_label())))
    table.add_row(Text("Model"), Text(_escape_terminal_controls(self.session.model_path)))
    table.add_row(Text("Language"), Text(_escape_terminal_controls(self.session.language)))
    table.add_row(Text("Sample rate"), Text(f"{SAMPLE_RATE:,} Hz"))
    table.add_row(Text("Started"), Text(self.session.started_at.strftime("%Y-%m-%d %H:%M:%S")))
    return Panel(table, title="Reco", subtitle="Speech-to-text", border_style="cyan", box=box.ROUNDED)

  def _live_display(self) -> Group:
    return Group(
      self.listening_spinner,
      Text(""),
      self._metrics_line(),
    )

  def _metrics_line(self) -> Text:
    metrics = {
      "Elapsed": _format_seconds(monotonic() - self.started_at),
      "Audio": _format_milliseconds(self.processed_audio_ms),
      "Segments": str(self.total_segments),
      "Recognized": str(self.recognized_segments),
      "Characters": str(self.characters),
      "Queue": f"{self.queue_depth}/{self.max_queue_depth}",
    }
    line = Text()
    for index, (label, value) in enumerate(metrics.items()):
      if index > 0:
        line.append("   ")
      line.append(label, style="bold bright_black")
      line.append(" ")
      line.append(value, style="bold white")
    return line

  def _source_label(self) -> str:
    if self.session.source_path is None:
      return self.session.source
    return f"{self.session.source}: {self.session.source_path}"

  def _print_transcript_line(self, progress: TranscriptionProgress) -> None:
    line = _format_transcript_line(progress.latest_start_ms, progress.latest_text or "")
    console = self._live.console if self._live is not None else self.console
    console.print(line, style="white", highlight=False, markup=False)

  def _stop_live(self) -> None:
    if self._live is not None:
      self._live.stop()
      self._live = None


def print_error(message: str) -> None:
  """Render a CLI error with the same Rich visual language."""

  Console(stderr=True).print(Text(_escape_terminal_controls(message), style="bold red"))


def print_status(message: str) -> None:
  """Render a short startup status line before live transcription begins."""

  Console().print(Text(_escape_terminal_controls(message), style="bold cyan"))


def _format_milliseconds(milliseconds: int) -> str:
  seconds = max(0, milliseconds) / 1000
  return _format_seconds(seconds)


def _format_transcript_line(start_ms: int | None, text: str) -> str:
  timestamp = _format_timestamp(start_ms or 0)
  return f"[{timestamp}] {_escape_terminal_controls(text)}"


def _escape_terminal_controls(value: str) -> str:
  escaped: list[str] = []
  named_controls = {"\t": r"\t", "\n": r"\n", "\r": r"\r"}
  for character in value:
    if character == "\\":
      escaped.append(r"\\")
      continue
    if character in named_controls:
      escaped.append(named_controls[character])
      continue
    if category(character) in {"Cc", "Cf", "Zl", "Zp"}:
      codepoint = ord(character)
      escaped.append(f"\\x{codepoint:02x}" if codepoint <= 0xFF else f"\\u{codepoint:04x}")
      continue
    escaped.append(character)
  return "".join(escaped)


def _format_timestamp(milliseconds: int) -> str:
  clamped_milliseconds = max(0, milliseconds)
  hours, remainder = divmod(clamped_milliseconds, 3_600_000)
  minutes, remainder = divmod(remainder, 60_000)
  seconds, millis = divmod(remainder, 1_000)
  return f"{hours:02}:{minutes:02}:{seconds:02}.{millis:03}"


def _format_seconds(seconds: float) -> str:
  if seconds < 60:
    return f"{seconds:.1f}s"
  minutes, remaining_seconds = divmod(int(seconds), 60)
  hours, minutes = divmod(minutes, 60)
  if hours:
    return f"{hours:d}h {minutes:02d}m {remaining_seconds:02d}s"
  return f"{minutes:d}m {remaining_seconds:02d}s"

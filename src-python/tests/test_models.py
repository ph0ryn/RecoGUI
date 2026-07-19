from __future__ import annotations

from typing import cast

import numpy as np
import pytest

from reco.audio import SourceMetadata
from reco.models import (
  RunStatus,
  SpeechSegment,
  SplitReason,
  TranscriptDocument,
  TranscriptionDiagnostics,
  TranscriptModelMetadata,
  TranscriptSegment,
  TranscriptTiming,
  VadDiagnostics,
)


def timing() -> TranscriptTiming:
  return TranscriptTiming(
    command_started_at="2026-07-18T00:00:00+00:00",
    command_ended_at="2026-07-18T00:00:01+00:00",
    command_wall_time_ms=1000,
    pipeline_wall_time_ms=900,
    media_duration_ms=2000,
    model_load_ms=100,
    decode_time_ms=500,
    pipeline_rtf=0.45,
    decode_rtf=0.25,
  )


def transcript_segment(index: int, start_sample: int, end_sample: int, text: str) -> TranscriptSegment:
  return TranscriptSegment(
    index=index,
    start_sample=start_sample,
    end_sample=end_sample,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text=text,
    raw_text=text,
    vad=VadDiagnostics(),
    transcription=TranscriptionDiagnostics(max_tokens=64),
  )


def test_speech_segment_cannot_represent_empty_audio_or_negative_timeline() -> None:
  with pytest.raises(ValueError, match="non-empty"):
    SpeechSegment(start_sample=0, audio=np.array([], dtype=np.float32), sample_rate=16_000)
  with pytest.raises(ValueError, match="negative"):
    SpeechSegment(start_sample=-1, audio=np.ones(10, dtype=np.float32), sample_rate=16_000)


def test_transcript_segment_cannot_represent_zero_or_negative_duration() -> None:
  with pytest.raises(ValueError, match="positive sample range"):
    transcript_segment(0, 100, 100, "invalid")


def test_transcript_model_metadata_requires_a_model_and_language() -> None:
  with pytest.raises(ValueError, match="path"):
    TranscriptModelMetadata(path="", language="Japanese")
  with pytest.raises(ValueError, match="language"):
    TranscriptModelMetadata(path="fixed-model", language="")


def test_document_derives_text_and_counts_from_its_canonical_segments() -> None:
  segments = [
    transcript_segment(0, 0, 1_000, "first"),
    transcript_segment(1, 2_000, 3_000, ""),
    transcript_segment(2, 4_000, 5_000, "third"),
  ]
  document = TranscriptDocument(
    source=SourceMetadata(kind="file", path="audio.wav"),
    model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
    status=RunStatus.COMPLETED,
    timing=timing(),
    max_queue_depth=2,
    segments=tuple(segments),
  )

  assert document.text == "first\nthird"
  assert document.total_segments == 3
  assert document.recognized_segments == 2
  assert document.characters == 10


def test_document_defensively_freezes_its_segment_collection() -> None:
  source_segments = [transcript_segment(0, 0, 1_000, "stable")]
  document = TranscriptDocument(
    source=SourceMetadata(kind="file", path="audio.wav"),
    model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
    status=RunStatus.COMPLETED,
    timing=timing(),
    max_queue_depth=1,
    segments=cast(tuple[TranscriptSegment, ...], source_segments),
  )

  source_segments.append(transcript_segment(1, 2_000, 3_000, "mutation"))

  assert isinstance(document.segments, tuple)
  assert document.text == "stable"


def test_document_rejects_noncontiguous_indices_and_overlapping_ranges() -> None:
  common = {
    "source": SourceMetadata(kind="file", path="audio.wav"),
    "model": TranscriptModelMetadata(path="fixed-model", language="Japanese"),
    "status": RunStatus.COMPLETED,
    "timing": timing(),
    "max_queue_depth": 1,
  }

  with pytest.raises(ValueError, match="indices"):
    TranscriptDocument(
      **common,
      segments=(transcript_segment(1, 0, 1_000, "bad index"),),
    )
  with pytest.raises(ValueError, match="non-overlapping"):
    TranscriptDocument(
      **common,
      segments=(
        transcript_segment(0, 0, 2_000, "first"),
        transcript_segment(1, 1_000, 3_000, "second"),
      ),
    )


@pytest.mark.parametrize("status", [RunStatus.RUNNING, RunStatus.FAILED])
def test_document_rejects_nonterminal_or_failed_pipeline_status(status: RunStatus) -> None:
  with pytest.raises(ValueError, match="completed or interrupted"):
    TranscriptDocument(
      source=SourceMetadata(kind="file", path="audio.wav"),
      model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
      status=status,
      timing=timing(),
      max_queue_depth=0,
      segments=(),
    )


def test_document_rejects_mixed_segment_sample_rates() -> None:
  first = transcript_segment(0, 0, 1_000, "first")
  second = TranscriptSegment(
    index=1,
    start_sample=1_000,
    end_sample=2_000,
    sample_rate=8_000,
    split_reason=SplitReason.SILENCE,
    text="second",
    raw_text="second",
    vad=VadDiagnostics(),
    transcription=TranscriptionDiagnostics(max_tokens=64),
  )

  with pytest.raises(ValueError, match="one sample rate"):
    TranscriptDocument(
      source=SourceMetadata(kind="file", path="audio.wav"),
      model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
      status=RunStatus.COMPLETED,
      timing=timing(),
      max_queue_depth=1,
      segments=(first, second),
    )

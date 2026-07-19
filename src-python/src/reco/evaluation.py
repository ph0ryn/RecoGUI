"""Model-independent character error rate evaluation for transcripts."""

from __future__ import annotations

import argparse
import sys
import unicodedata
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from rapidfuzz.distance import Levenshtein

NORMALIZATION_VERSION = "nfkc-casefold-strip-cpz-v1"
RAW_NORMALIZATION_VERSION = "raw-v1"


class EmptyReferenceError(ValueError):
  """Raised when a reference has no comparable characters."""


@dataclass(frozen=True)
class LevenshteinOperations:
  """Deterministic operation counts for one optimal character alignment."""

  insertions: int = 0
  deletions: int = 0
  substitutions: int = 0

  def __post_init__(self) -> None:
    if min(self.insertions, self.deletions, self.substitutions) < 0:
      raise ValueError("Levenshtein operation counts must not be negative")

  @property
  def distance(self) -> int:
    return self.insertions + self.deletions + self.substitutions


@dataclass(frozen=True)
class TranscriptEvaluation:
  """Exact Levenshtein character error rate inputs and result."""

  normalized_reference: str
  normalized_hypothesis: str
  operations: LevenshteinOperations
  normalization_version: str

  def __post_init__(self) -> None:
    if not self.normalization_version.strip():
      raise ValueError("Normalization version must not be empty")

  @property
  def edit_distance(self) -> int:
    return self.operations.distance

  @property
  def insertions(self) -> int:
    return self.operations.insertions

  @property
  def deletions(self) -> int:
    return self.operations.deletions

  @property
  def substitutions(self) -> int:
    return self.operations.substitutions

  @property
  def reference_characters(self) -> int:
    return len(self.normalized_reference)

  @property
  def hypothesis_characters(self) -> int:
    return len(self.normalized_hypothesis)

  @property
  def character_error_rate(self) -> float:
    return self.edit_distance / self.reference_characters


def normalize_transcript(text: str) -> str:
  """Normalize transcript text for Japanese-friendly CER comparisons.

  Compatibility normalization makes full-width Latin characters, digits, and
  half-width katakana comparable. Case, whitespace, punctuation, and control
  characters are ignored because they are not consistently emitted by speech
  recognition systems. Kana scripts and the long-vowel mark remain distinct.
  """

  casefolded = unicodedata.normalize("NFKC", text).casefold()
  normalized = unicodedata.normalize("NFKC", casefolded)
  return "".join(character for character in normalized if unicodedata.category(character)[0] not in {"C", "P", "Z"})


def levenshtein_distance(reference: str, hypothesis: str) -> int:
  """Return the exact character-level Levenshtein edit distance."""

  return Levenshtein.distance(reference, hypothesis)


def levenshtein_operations(reference: str, hypothesis: str) -> LevenshteinOperations:
  """Return deterministic insertion, deletion, and substitution counts.

  RapidFuzz computes one deterministic optimal alignment using its bit-parallel
  Levenshtein implementation.
  """

  insertions = 0
  deletions = 0
  substitutions = 0
  for operation in Levenshtein.editops(reference, hypothesis):
    match operation.tag:
      case "insert":
        insertions += 1
      case "delete":
        deletions += 1
      case "replace":
        substitutions += 1
      case unexpected:
        raise RuntimeError(f"Unsupported Levenshtein edit operation: {unexpected}")
  return LevenshteinOperations(
    insertions=insertions,
    deletions=deletions,
    substitutions=substitutions,
  )


def evaluate_transcripts(reference: str, hypothesis: str, *, normalize: bool = True) -> TranscriptEvaluation:
  """Evaluate a hypothesis against a non-empty reference transcript."""

  normalized_reference = normalize_transcript(reference) if normalize else reference
  normalized_hypothesis = normalize_transcript(hypothesis) if normalize else hypothesis
  if not normalized_reference:
    mode = " after normalization" if normalize else ""
    raise EmptyReferenceError(f"Reference transcript is empty{mode}; CER is undefined")

  return TranscriptEvaluation(
    normalized_reference=normalized_reference,
    normalized_hypothesis=normalized_hypothesis,
    operations=levenshtein_operations(normalized_reference, normalized_hypothesis),
    normalization_version=NORMALIZATION_VERSION if normalize else RAW_NORMALIZATION_VERSION,
  )


def character_error_rate(reference: str, hypothesis: str, *, normalize: bool = True) -> float:
  """Return exact character error rate for a hypothesis and reference."""

  return evaluate_transcripts(reference, hypothesis, normalize=normalize).character_error_rate


def _build_argument_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
    prog="reco-evaluate",
    description="Compare UTF-8 transcripts using exact character error rate.",
  )
  parser.add_argument("reference", type=Path, help="Human-verified reference transcript")
  parser.add_argument("hypothesis", type=Path, help="ASR hypothesis transcript")
  parser.add_argument(
    "--raw",
    action="store_true",
    help="Compare raw Unicode text, including case, whitespace, and punctuation",
  )
  return parser


def main(argv: Sequence[str] | None = None) -> int:
  """Run the transcript evaluation CLI."""

  arguments = _build_argument_parser().parse_args(argv)
  try:
    reference = arguments.reference.read_text(encoding="utf-8")
    hypothesis = arguments.hypothesis.read_text(encoding="utf-8")
    evaluation = evaluate_transcripts(reference, hypothesis, normalize=not arguments.raw)
  except (OSError, UnicodeError, EmptyReferenceError) as error:
    print(f"reco-evaluate: error: {error}", file=sys.stderr)
    return 2

  print(
    f"CER: {evaluation.character_error_rate:.4%} "
    f"(edits={evaluation.edit_distance}, substitutions={evaluation.substitutions}, "
    f"deletions={evaluation.deletions}, insertions={evaluation.insertions}, "
    f"reference={evaluation.reference_characters}, hypothesis={evaluation.hypothesis_characters}, "
    f"normalization={evaluation.normalization_version})"
  )
  return 0


if __name__ == "__main__":
  raise SystemExit(main())

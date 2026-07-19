from __future__ import annotations

from itertools import product
from pathlib import Path

import pytest

from reco.evaluation import (
  NORMALIZATION_VERSION,
  RAW_NORMALIZATION_VERSION,
  EmptyReferenceError,
  LevenshteinOperations,
  character_error_rate,
  evaluate_transcripts,
  levenshtein_distance,
  levenshtein_operations,
  main,
  normalize_transcript,
)


def reference_levenshtein_distance(reference: str, hypothesis: str) -> int:
  """Small independent dynamic-programming oracle for wrapper tests."""

  previous = list(range(len(hypothesis) + 1))
  for reference_index, reference_character in enumerate(reference, start=1):
    current = [reference_index]
    for hypothesis_index, hypothesis_character in enumerate(hypothesis, start=1):
      current.append(
        min(
          previous[hypothesis_index] + 1,
          current[hypothesis_index - 1] + 1,
          previous[hypothesis_index - 1] + (reference_character != hypothesis_character),
        )
      )
    previous = current
  return previous[-1]


def test_normalize_transcript_handles_japanese_compatibility_forms() -> None:
  assert normalize_transcript(" \uff21\uff29、ｶﾞ音声。\n") == "aiガ音声"


def test_normalize_transcript_preserves_semantic_japanese_characters() -> None:
  assert normalize_transcript("カー + かあ") == "カー+かあ"


@pytest.mark.parametrize(
  ("reference", "hypothesis", "expected"),
  [
    ("", "", 0),
    ("音声", "", 2),
    ("", "音声", 2),
    ("企業と経済", "企業の経済", 1),
    ("kitten", "sitting", 3),
  ],
)
def test_levenshtein_distance_is_exact(reference: str, hypothesis: str, expected: int) -> None:
  assert levenshtein_distance(reference, hypothesis) == expected
  assert levenshtein_distance(hypothesis, reference) == expected


def test_levenshtein_engine_matches_independent_oracle_exhaustively() -> None:
  strings = [""]
  strings.extend("".join(characters) for size in range(1, 4) for characters in product("ab", repeat=size))

  for reference in strings:
    for hypothesis in strings:
      expected = reference_levenshtein_distance(reference, hypothesis)
      assert levenshtein_distance(reference, hypothesis) == expected
      assert levenshtein_operations(reference, hypothesis).distance == expected


def test_ambiguous_optimal_alignment_has_a_stable_operation_breakdown() -> None:
  assert levenshtein_operations("ab", "ba") == LevenshteinOperations(insertions=1, deletions=1)


def test_evaluate_transcripts_reports_normalized_inputs_and_counts() -> None:
  result = evaluate_transcripts("\uff21\uff29音声。", "ai音性")

  assert result.normalized_reference == "ai音声"
  assert result.normalized_hypothesis == "ai音性"
  assert result.edit_distance == 1
  assert result.substitutions == 1
  assert result.deletions == 0
  assert result.insertions == 0
  assert result.normalization_version == NORMALIZATION_VERSION
  assert result.reference_characters == 4
  assert result.hypothesis_characters == 4
  assert result.character_error_rate == 0.25


def test_character_error_rate_can_exceed_one_for_insertions() -> None:
  assert character_error_rate("音", "音声認識") == 3.0


def test_levenshtein_operation_breakdown_distinguishes_each_edit_type() -> None:
  assert levenshtein_operations("音声", "音性").substitutions == 1
  assert levenshtein_operations("音声", "音").deletions == 1
  assert levenshtein_operations("音", "音声").insertions == 1


def test_empty_normalized_reference_has_explicit_error() -> None:
  with pytest.raises(EmptyReferenceError, match="empty after normalization; CER is undefined"):
    evaluate_transcripts(" 。\n", "音声")


def test_raw_evaluation_keeps_spacing_and_punctuation() -> None:
  result = evaluate_transcripts("音声。", "音声", normalize=False)

  assert result.normalized_reference == "音声。"
  assert result.edit_distance == 1
  assert result.deletions == 1
  assert result.normalization_version == RAW_NORMALIZATION_VERSION
  assert result.character_error_rate == pytest.approx(1 / 3)


def test_cli_compares_utf8_files(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  reference = tmp_path / "reference.txt"
  hypothesis = tmp_path / "hypothesis.txt"
  reference.write_text("企業と経済。", encoding="utf-8")
  hypothesis.write_text("企業の経済", encoding="utf-8")

  exit_code = main([str(reference), str(hypothesis)])

  captured = capsys.readouterr()
  assert exit_code == 0
  assert captured.out == (
    "CER: 20.0000% (edits=1, substitutions=1, deletions=0, insertions=0, "
    "reference=5, hypothesis=5, normalization=nfkc-casefold-strip-cpz-v1)\n"
  )
  assert captured.err == ""


def test_cli_raw_mode_includes_punctuation(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  reference = tmp_path / "reference.txt"
  hypothesis = tmp_path / "hypothesis.txt"
  reference.write_text("音声。", encoding="utf-8")
  hypothesis.write_text("音声", encoding="utf-8")

  exit_code = main(["--raw", str(reference), str(hypothesis)])

  captured = capsys.readouterr()
  assert exit_code == 0
  assert captured.out == (
    "CER: 33.3333% (edits=1, substitutions=0, deletions=1, insertions=0, "
    "reference=3, hypothesis=2, normalization=raw-v1)\n"
  )


def test_cli_rejects_empty_reference(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  reference = tmp_path / "reference.txt"
  hypothesis = tmp_path / "hypothesis.txt"
  reference.write_text("。、", encoding="utf-8")
  hypothesis.write_text("音声", encoding="utf-8")

  exit_code = main([str(reference), str(hypothesis)])

  captured = capsys.readouterr()
  assert exit_code == 2
  assert captured.out == ""
  assert "Reference transcript is empty after normalization; CER is undefined" in captured.err


def test_cli_reports_missing_file_without_traceback(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  missing = tmp_path / "missing.txt"
  hypothesis = tmp_path / "hypothesis.txt"
  hypothesis.write_text("音声", encoding="utf-8")

  exit_code = main([str(missing), str(hypothesis)])

  captured = capsys.readouterr()
  assert exit_code == 2
  assert captured.out == ""
  assert "reco-evaluate: error:" in captured.err
  assert str(missing) in captured.err

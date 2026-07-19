from __future__ import annotations

from typing import cast

import pytest

from reco.config import DEFAULT_CONFIG, CliConfig, TranscriptionConfig, VadConfig


def test_default_model_is_the_fixed_japanese_mlx_model() -> None:
  assert DEFAULT_CONFIG.cli.default_model == "ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit"
  assert DEFAULT_CONFIG.cli.default_model_revision == "7c70d18cb650655d32eafb952a74a49c6a3caad0"
  assert DEFAULT_CONFIG.cli.default_language == "Japanese"


def test_default_vad_profile_uses_hysteresis_padding_and_adaptive_split() -> None:
  vad = DEFAULT_CONFIG.vad

  assert vad.end_threshold < vad.start_threshold
  assert vad.min_speech_duration_ms > 0
  assert vad.min_silence_duration_ms == 800
  assert vad.speech_pad_ms > 0
  assert vad.target_segment_duration_ms < vad.max_segment_duration_ms


def test_default_transcription_profile_has_bounded_memory_and_generation() -> None:
  transcription = DEFAULT_CONFIG.transcription

  assert transcription.max_transcription_queue_size == 2
  assert transcription.interrupted_worker_shutdown_timeout_seconds == 30
  assert transcription.failed_worker_shutdown_timeout_seconds == 2
  assert transcription.min_generation_tokens < transcription.max_generation_tokens
  assert transcription.repetition_penalty is None


def test_invalid_cli_config_is_rejected() -> None:
  with pytest.raises(ValueError, match="model"):
    CliConfig(default_model="")
  with pytest.raises(ValueError, match="revision"):
    CliConfig(default_model_revision=" ")
  with pytest.raises(ValueError, match="language"):
    CliConfig(default_language=" ")


def test_invalid_vad_config_is_rejected() -> None:
  with pytest.raises(ValueError, match="thresholds"):
    VadConfig(start_threshold=0.4, end_threshold=0.5)
  with pytest.raises(ValueError, match="finite"):
    VadConfig(start_threshold=float("nan"))
  with pytest.raises(ValueError, match="durations"):
    VadConfig(min_silence_duration_ms=0)
  with pytest.raises(ValueError, match="padding"):
    VadConfig(speech_pad_ms=-1)
  with pytest.raises(ValueError, match="whole milliseconds"):
    VadConfig(min_speech_duration_ms=cast(int, 0.5))
  with pytest.raises(ValueError, match="must not exceed"):
    VadConfig(speech_pad_ms=801, min_silence_duration_ms=800)
  with pytest.raises(ValueError, match="target duration"):
    VadConfig(target_segment_duration_ms=60_001, max_segment_duration_ms=60_000)
  with pytest.raises(ValueError, match="minimum speech"):
    VadConfig(min_speech_duration_ms=1_001, target_segment_duration_ms=1_000)


def test_invalid_transcription_config_is_rejected() -> None:
  with pytest.raises(ValueError, match="per second"):
    TranscriptionConfig(generation_tokens_per_sec=0)
  with pytest.raises(ValueError, match="token bounds"):
    TranscriptionConfig(min_generation_tokens=65, max_generation_tokens=64)
  with pytest.raises(ValueError, match="whole numbers"):
    TranscriptionConfig(min_generation_tokens=cast(int, 64.5))
  with pytest.raises(ValueError, match="Temperature"):
    TranscriptionConfig(temperature=-0.1)
  with pytest.raises(ValueError, match="Repetition penalty"):
    TranscriptionConfig(repetition_penalty=0)
  with pytest.raises(ValueError, match="queue size"):
    TranscriptionConfig(max_transcription_queue_size=0)
  with pytest.raises(ValueError, match="per second"):
    TranscriptionConfig(generation_tokens_per_sec=float("nan"))
  with pytest.raises(ValueError, match="Temperature"):
    TranscriptionConfig(temperature=float("nan"))
  with pytest.raises(ValueError, match="Interrupted worker shutdown"):
    TranscriptionConfig(interrupted_worker_shutdown_timeout_seconds=0)
  with pytest.raises(ValueError, match="Failed worker shutdown"):
    TranscriptionConfig(failed_worker_shutdown_timeout_seconds=0)

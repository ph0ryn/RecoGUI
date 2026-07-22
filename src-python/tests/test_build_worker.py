from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from zipfile import ZipFile

PROJECT_ROOT = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = PROJECT_ROOT / "build_worker.py"


def build_worker(output: Path) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
    [sys.executable, str(BUILD_SCRIPT), str(output)],
    check=False,
    capture_output=True,
    text=True,
  )


def test_worker_archive_contains_only_asr_runtime_modules(tmp_path: Path) -> None:
  output = tmp_path / "reco-asr-worker.pyz"

  result = build_worker(output)

  assert result.returncode == 0, result.stderr
  with ZipFile(output) as archive:
    names = set(archive.namelist())
    assert archive.testzip() is None
  assert "__main__.py" in names
  assert "reco_worker/worker.py" in names
  assert "reco_worker/protocol.py" in names
  assert "reco/model_manager.py" in names
  assert "reco/transcription.py" in names
  assert not names & {
    "reco/audio.py",
    "reco/engine.py",
    "reco/host_pcm.py",
    "reco/pipeline.py",
    "reco/protocol.py",
    "reco/recording.py",
    "reco/repository.py",
    "reco/sidecar.py",
    "reco/vad.py",
  }
  assert not any(name.endswith((".md", ".onnx", ".pyc", ".pyo")) for name in names)
  assert not any("__pycache__" in name or name.startswith("tests/") for name in names)


def test_worker_archive_is_executable_under_the_final_command_name(tmp_path: Path) -> None:
  output = tmp_path / "reco-asr-worker.pyz"
  build_result = build_worker(output)
  assert build_result.returncode == 0, build_result.stderr

  result = subprocess.run(
    [sys.executable, str(output), "--help"],
    check=False,
    capture_output=True,
    text=True,
  )

  assert result.returncode == 0, result.stderr
  assert "reco-asr-worker" in result.stdout


def test_worker_archive_build_is_reproducible(tmp_path: Path) -> None:
  first = tmp_path / "first.pyz"
  second = tmp_path / "second.pyz"

  assert build_worker(first).returncode == 0
  assert build_worker(second).returncode == 0

  assert first.read_bytes() == second.read_bytes()


def test_importing_worker_does_not_reach_retired_python_responsibilities() -> None:
  probe = subprocess.run(
    [
      sys.executable,
      "-c",
      (
        "import json, sys; import reco_worker.worker; "
        "print(json.dumps(sorted(name for name in sys.modules if name.startswith('reco.'))))"
      ),
    ],
    cwd=PROJECT_ROOT,
    check=False,
    capture_output=True,
    text=True,
  )

  assert probe.returncode == 0, probe.stderr
  imported = set(json.loads(probe.stdout))
  assert not imported & {
    "reco.audio",
    "reco.engine",
    "reco.host_pcm",
    "reco.pipeline",
    "reco.protocol",
    "reco.recording",
    "reco.repository",
    "reco.sidecar",
    "reco.vad",
  }

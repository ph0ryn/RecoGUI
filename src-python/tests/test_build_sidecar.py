from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from zipfile import ZipFile

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SOURCE_DIRECTORY = PROJECT_ROOT / "src"
BUILD_SCRIPT = PROJECT_ROOT / "build_sidecar.py"


def build_sidecar(output: Path) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
    [sys.executable, str(BUILD_SCRIPT), str(output)],
    check=False,
    capture_output=True,
    text=True,
  )


def test_build_sidecar_contains_only_runtime_files(tmp_path: Path) -> None:
  output = tmp_path / "reco-engine.pyz"
  result = build_sidecar(output)

  assert result.returncode == 0, result.stderr

  with ZipFile(output) as archive:
    names = set(archive.namelist())
    assert archive.testzip() is None

  expected_python = {
    path.relative_to(SOURCE_DIRECTORY).as_posix()
    for path in SOURCE_DIRECTORY.rglob("*.py")
    if "__pycache__" not in path.parts
    and (path == SOURCE_DIRECTORY / "__main__.py" or path.relative_to(SOURCE_DIRECTORY).parts[:1] == ("reco",))
  }
  assert expected_python <= names
  assert "__main__.py" in names
  assert not any(name.endswith((".md", ".onnx")) for name in names)
  assert not any("__pycache__" in name or name.endswith((".pyc", ".pyo")) for name in names)
  assert not any(name.startswith("tests/") for name in names)
  assert not any(name.startswith("reco_worker/") and name.endswith(".py") for name in names)


def test_built_sidecar_is_executable(tmp_path: Path) -> None:
  output = tmp_path / "reco-engine.pyz"
  build_result = build_sidecar(output)

  assert build_result.returncode == 0, build_result.stderr

  result = subprocess.run(
    [sys.executable, str(output), "--help"],
    check=False,
    capture_output=True,
    text=True,
  )

  assert result.returncode == 0, result.stderr
  assert "reco-engine" in result.stdout


def test_built_sidecar_propagates_engine_exit_codes(tmp_path: Path) -> None:
  output = tmp_path / "reco-engine.pyz"
  build_result = build_sidecar(output)

  assert build_result.returncode == 0, build_result.stderr

  result = subprocess.run(
    [
      sys.executable,
      str(output),
      "serve",
      "--protocol-version",
      "999",
      "--database",
      str(tmp_path / "reco.sqlite3"),
      "--vad-model",
      str(PROJECT_ROOT / "assets" / "silero_vad.onnx"),
      "--logs-directory",
      str(tmp_path / "logs"),
      "--audio-fd",
      "0",
    ],
    check=False,
    capture_output=True,
    text=True,
  )

  assert result.returncode == 2
  assert "Unsupported protocol version" in result.stderr

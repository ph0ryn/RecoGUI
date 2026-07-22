"""Build the isolated ASR worker archive consumed by the Rust supervisor."""

from __future__ import annotations

import argparse
import os
from collections.abc import Iterator
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

PROJECT_ROOT = Path(__file__).resolve().parent
SOURCE_DIRECTORY = PROJECT_ROOT / "src"
DEFAULT_OUTPUT = PROJECT_ROOT / "dist" / "reco-asr-worker.pyz"

_WORKER_MAIN = b"from reco_worker.worker import main\n\nraise SystemExit(main())\n"
_SHARED_ASR_MODULES = (
  Path("reco/__init__.py"),
  Path("reco/config.py"),
  Path("reco/errors.py"),
  Path("reco/model_manager.py"),
  Path("reco/models.py"),
  Path("reco/transcription.py"),
)


def build_worker(output: Path = DEFAULT_OUTPUT) -> Path:
  """Create a deterministic code-only ASR worker archive atomically."""

  output.parent.mkdir(parents=True, exist_ok=True)
  temporary = output.with_name(f".{output.name}.{os.getpid()}.tmp")
  temporary.unlink(missing_ok=True)
  try:
    with ZipFile(temporary, "w", compression=ZIP_DEFLATED, compresslevel=9) as archive:
      _write(archive, Path("__main__.py"), _WORKER_MAIN)
      for relative_path in _runtime_files():
        _write(archive, relative_path, (SOURCE_DIRECTORY / relative_path).read_bytes())
    temporary.replace(output)
  except BaseException:
    temporary.unlink(missing_ok=True)
    raise
  return output


def _runtime_files() -> Iterator[Path]:
  yield from _SHARED_ASR_MODULES
  yield from sorted(
    path.relative_to(SOURCE_DIRECTORY)
    for path in (SOURCE_DIRECTORY / "reco_worker").rglob("*.py")
    if "__pycache__" not in path.parts
  )


def _write(archive: ZipFile, relative_path: Path, content: bytes) -> None:
  entry = ZipInfo(relative_path.as_posix(), date_time=(1980, 1, 1, 0, 0, 0))
  entry.compress_type = ZIP_DEFLATED
  entry.external_attr = 0o100644 << 16
  archive.writestr(entry, content, compress_type=ZIP_DEFLATED, compresslevel=9)


def main() -> int:
  """Build the archive at the optional output path."""

  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("output", nargs="?", type=Path, default=DEFAULT_OUTPUT)
  args = parser.parse_args()
  print(build_worker(args.output))
  return 0


if __name__ == "__main__":
  raise SystemExit(main())

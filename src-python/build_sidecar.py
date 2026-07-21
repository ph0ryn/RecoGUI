"""Build the compressed Python sidecar archive consumed by Tauri."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
from zipapp import create_archive

PROJECT_ROOT = Path(__file__).resolve().parent
SOURCE_DIRECTORY = PROJECT_ROOT / "src"
DEFAULT_OUTPUT = PROJECT_ROOT / "dist" / "reco-engine.pyz"


def _include_runtime_file(relative_path: Path) -> bool:
  source_path = SOURCE_DIRECTORY / relative_path
  if source_path.is_dir():
    return "__pycache__" not in relative_path.parts
  if relative_path == Path("__main__.py"):
    return True
  return (
    relative_path.parts[:1] == ("reco",) and "__pycache__" not in relative_path.parts and relative_path.suffix == ".py"
  )


def build_sidecar(output: Path = DEFAULT_OUTPUT) -> Path:
  """Create the compressed sidecar archive atomically."""

  output.parent.mkdir(parents=True, exist_ok=True)
  temporary = output.with_name(f".{output.name}.{os.getpid()}.tmp")
  temporary.unlink(missing_ok=True)
  try:
    create_archive(
      SOURCE_DIRECTORY,
      temporary,
      compressed=True,
      filter=_include_runtime_file,
    )
    temporary.replace(output)
  except BaseException:
    temporary.unlink(missing_ok=True)
    raise
  return output


def main() -> int:
  """Build the archive at the optional output path."""

  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("output", nargs="?", type=Path, default=DEFAULT_OUTPUT)
  args = parser.parse_args()
  print(build_sidecar(args.output))
  return 0


if __name__ == "__main__":
  raise SystemExit(main())

from __future__ import annotations

from pathlib import Path
from threading import Event, Thread
from time import sleep

import pytest

from reco.errors import RecoError
from reco.model_manager import ModelManager, ModelState


def fake_download(**options: str) -> str:
  destination = Path(options["local_dir"])
  destination.mkdir(parents=True, exist_ok=True)
  (destination / "config.json").write_text("{}", encoding="utf-8")
  (destination / "weights.safetensors").write_bytes(b"weights")
  return str(destination)


def fake_vad_download(destination: Path) -> Path:
  destination.write_bytes(b"vad")
  return destination


def test_download_is_verified_and_atomically_published(tmp_path: Path) -> None:
  manager = ModelManager(tmp_path, download_func=fake_download, vad_download_func=fake_vad_download)

  result = manager.download()

  assert result == manager.model_directory
  assert manager.verify()
  assert manager.snapshot().state is ModelState.READY
  assert not manager.temporary_directory.exists()


def test_modified_model_file_is_invalid(tmp_path: Path) -> None:
  manager = ModelManager(tmp_path, download_func=fake_download, vad_download_func=fake_vad_download)
  manager.download()
  (manager.model_directory / "weights.safetensors").write_bytes(b"modified")

  assert not manager.verify()
  assert manager.snapshot().state is ModelState.INVALID


def test_model_cannot_be_deleted_while_engine_is_active(tmp_path: Path) -> None:
  manager = ModelManager(tmp_path, download_func=fake_download, vad_download_func=fake_vad_download)
  manager.download()

  with pytest.raises(RecoError, match="idle"):
    manager.delete(engine_idle=False)

  assert manager.delete(engine_idle=True)
  assert manager.snapshot().state is ModelState.MISSING


def test_cancel_keeps_resumable_staging_without_promoting_model(tmp_path: Path) -> None:
  started = Event()
  attempts = 0

  def resumable_download(**options: object) -> str:
    nonlocal attempts
    attempts += 1
    destination = Path(str(options["local_dir"]))
    destination.mkdir(parents=True, exist_ok=True)
    partial = destination / "weights.safetensors.part"
    partial.write_bytes(partial.read_bytes() + b"chunk" if partial.exists() else b"chunk")
    cancel = options["cancel_event"]
    assert isinstance(cancel, Event)
    if attempts == 1:
      started.set()
      while not cancel.wait(0.01):
        pass
      raise RecoError("Model download was cancelled")
    (destination / "weights.safetensors").write_bytes(partial.read_bytes() + b"resumed")
    partial.unlink()
    return str(destination)

  manager = ModelManager(
    tmp_path,
    download_func=resumable_download,
    vad_download_func=fake_vad_download,
  )
  failures: list[BaseException] = []

  def run_download() -> None:
    try:
      manager.download()
    except BaseException as exc:
      failures.append(exc)

  thread = Thread(target=run_download)
  thread.start()
  assert started.wait(timeout=1)
  manager.cancel_download()
  thread.join(timeout=1)

  assert failures and "cancelled" in str(failures[0])
  assert manager.temporary_directory.is_dir()
  assert not manager.model_directory.exists()
  sleep(0.01)
  assert manager.download() == manager.model_directory
  assert attempts == 2

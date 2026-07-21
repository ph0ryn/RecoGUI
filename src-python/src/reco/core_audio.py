"""Minimal Core Audio device identity access for macOS."""

from __future__ import annotations

import ctypes
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from reco.errors import RecoError

_CORE_AUDIO_PATH = Path("/System/Library/Frameworks/CoreAudio.framework/CoreAudio")
_CORE_FOUNDATION_PATH = Path("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
_SYSTEM_AUDIO_OBJECT = 1
_MAIN_PROPERTY_ELEMENT = 0
_UTF8_ENCODING = 0x08000100


@dataclass(frozen=True)
class CoreAudioDevice:
  """Persistent Core Audio identity in the system device order."""

  object_id: int
  uid: str
  name: str


class _PropertyAddress(ctypes.Structure):
  _fields_ = [
    ("selector", ctypes.c_uint32),
    ("scope", ctypes.c_uint32),
    ("element", ctypes.c_uint32),
  ]


def list_core_audio_devices() -> tuple[CoreAudioDevice, ...]:
  """Return visible Core Audio devices with persistent boot-stable UIDs."""

  core_audio, core_foundation = _load_frameworks()
  address = _PropertyAddress(_fourcc("dev#"), _fourcc("glob"), _MAIN_PROPERTY_ELEMENT)
  size = ctypes.c_uint32()
  _check_status(
    core_audio.AudioObjectGetPropertyDataSize(
      _SYSTEM_AUDIO_OBJECT,
      ctypes.byref(address),
      0,
      None,
      ctypes.byref(size),
    ),
    "Could not read the Core Audio device list size",
  )
  if size.value % ctypes.sizeof(ctypes.c_uint32) != 0:
    raise RecoError("Core Audio returned an invalid device list")
  device_ids = (ctypes.c_uint32 * (size.value // ctypes.sizeof(ctypes.c_uint32)))()
  _check_status(
    core_audio.AudioObjectGetPropertyData(
      _SYSTEM_AUDIO_OBJECT,
      ctypes.byref(address),
      0,
      None,
      ctypes.byref(size),
      device_ids,
    ),
    "Could not read the Core Audio device list",
  )
  return tuple(
    CoreAudioDevice(
      object_id=int(device_id),
      uid=_read_string_property(core_audio, core_foundation, int(device_id), "uid "),
      name=_read_string_property(core_audio, core_foundation, int(device_id), "lnam"),
    )
    for device_id in device_ids
  )


def _load_frameworks() -> tuple[Any, Any]:
  try:
    core_audio = ctypes.CDLL(str(_CORE_AUDIO_PATH))
    core_foundation = ctypes.CDLL(str(_CORE_FOUNDATION_PATH))
  except OSError as exc:
    raise RecoError("Core Audio is unavailable") from exc

  core_audio.AudioObjectGetPropertyDataSize.argtypes = [
    ctypes.c_uint32,
    ctypes.POINTER(_PropertyAddress),
    ctypes.c_uint32,
    ctypes.c_void_p,
    ctypes.POINTER(ctypes.c_uint32),
  ]
  core_audio.AudioObjectGetPropertyDataSize.restype = ctypes.c_int32
  core_audio.AudioObjectGetPropertyData.argtypes = [
    ctypes.c_uint32,
    ctypes.POINTER(_PropertyAddress),
    ctypes.c_uint32,
    ctypes.c_void_p,
    ctypes.POINTER(ctypes.c_uint32),
    ctypes.c_void_p,
  ]
  core_audio.AudioObjectGetPropertyData.restype = ctypes.c_int32
  core_foundation.CFStringGetCString.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_long,
    ctypes.c_uint32,
  ]
  core_foundation.CFStringGetCString.restype = ctypes.c_bool
  core_foundation.CFRelease.argtypes = [ctypes.c_void_p]
  core_foundation.CFRelease.restype = None
  return core_audio, core_foundation


def _read_string_property(
  core_audio: Any,
  core_foundation: Any,
  object_id: int,
  selector: str,
) -> str:
  address = _PropertyAddress(_fourcc(selector), _fourcc("glob"), _MAIN_PROPERTY_ELEMENT)
  value = ctypes.c_void_p()
  size = ctypes.c_uint32(ctypes.sizeof(value))
  _check_status(
    core_audio.AudioObjectGetPropertyData(
      object_id,
      ctypes.byref(address),
      0,
      None,
      ctypes.byref(size),
      ctypes.byref(value),
    ),
    f"Could not read Core Audio property {selector!r}",
  )
  if value.value is None:
    raise RecoError(f"Core Audio property {selector!r} is empty")
  try:
    buffer = ctypes.create_string_buffer(4_096)
    if not core_foundation.CFStringGetCString(value, buffer, len(buffer), _UTF8_ENCODING):
      raise RecoError(f"Could not decode Core Audio property {selector!r}")
    result = buffer.value.decode("utf-8").strip()
    if not result:
      raise RecoError(f"Core Audio property {selector!r} is empty")
    return result
  finally:
    core_foundation.CFRelease(value)


def _fourcc(value: str) -> int:
  if len(value) != 4:
    raise ValueError("Core Audio property codes must contain four ASCII characters")
  return int.from_bytes(value.encode("ascii"), "big")


def _check_status(status: int, message: str) -> None:
  if status != 0:
    raise RecoError(f"{message} (OSStatus {status})")

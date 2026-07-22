from __future__ import annotations

import json
import socket
from collections.abc import Callable
from pathlib import Path
from threading import Thread
from types import SimpleNamespace
from typing import BinaryIO, cast

import numpy as np
import pytest

from reco.config import DEFAULT_TRANSCRIPTION_CONFIG, TranscriptionConfig
from reco.model_manager import ModelManager
from reco.models import SpeechSegment, TranscriptionDiagnostics, TranscriptionResult
from reco_worker.protocol import AsrProtocolError, FrameKind, FrameWriter, read_frame
from reco_worker.worker import AsrWorkerServer, WorkerDispatcher, WorkerRequestError

FIXTURE_PATH = Path(__file__).resolve().parents[2] / "fixtures" / "rasr-v1" / "frames.json"


class FakeService:
  def __init__(
    self,
    factory: FakeServiceFactory,
    *,
    language: str | None,
    model: object | None,
    config: TranscriptionConfig,
  ) -> None:
    self.factory = factory
    self.language = language
    self.model = model
    self.config = config
    self.model_load_ms = 37

  def load_model(self) -> object:
    self.factory.load_count += 1
    return self.factory.runtime

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    self.factory.transcriptions.append((segment, self.language, self.model, self.config))
    return TranscriptionResult(
      text="recognized",
      raw_text=" recognized ",
      language=self.language or "Japanese",
      diagnostics=TranscriptionDiagnostics(
        max_tokens=64,
        generation_tokens=8,
        prompt_tokens=4,
        total_tokens=12,
        model_total_time_ms=125,
        retry_count=1,
        token_limit_reached=False,
        warning=None,
      ),
    )


class FakeServiceFactory:
  def __init__(self) -> None:
    self.runtime = object()
    self.load_count = 0
    self.paths: list[Path] = []
    self.transcriptions: list[tuple[SpeechSegment, str | None, object | None, TranscriptionConfig]] = []

  def __call__(
    self,
    model_path: str | Path,
    *,
    language: str | None,
    model: object | None = None,
    config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG,
  ) -> FakeService:
    self.paths.append(Path(model_path))
    return FakeService(self, language=language, model=model, config=config)


def cache_info(snapshot: Path, revision_id: str = "commit") -> SimpleNamespace:
  revision = SimpleNamespace(
    commit_hash=revision_id,
    snapshot_path=snapshot,
    size_on_disk=2_500_000_000,
    last_modified=1_700_000_000,
    refs=frozenset({"main"}),
  )
  repo = SimpleNamespace(repo_id="owner/model", repo_type="model", revisions=(revision,))
  return SimpleNamespace(repos=(repo,))


def dispatcher_with_model(tmp_path: Path) -> tuple[WorkerDispatcher, FakeServiceFactory, Path]:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  (snapshot / "config.json").write_text('{"support_languages":["Japanese"]}')
  factory = FakeServiceFactory()
  dispatcher = WorkerDispatcher(
    model_manager=ModelManager(cache_scanner=lambda: cache_info(snapshot)),
    service_factory=factory,
    cache_clearer=lambda: None,
  )
  dispatcher.dispatch(
    "model.load",
    {
      "requestId": "load-request",
      "operation": "model.load",
      "repoId": "owner/model",
      "revision": "commit",
    },
    b"",
  )
  return dispatcher, factory, snapshot.resolve()


def segment_request(*, sample_count: int = 3) -> dict[str, object]:
  return {
    "requestId": "segment-request",
    "operation": "segment.transcribe",
    "sessionId": "opaque session",
    "runId": "opaque run",
    "jobId": "opaque job",
    "segmentIndex": 7,
    "startSample": 160,
    "endSample": 160 + sample_count,
    "sampleRate": 16_000,
    "splitReason": "adaptiveSplit",
    "language": "Japanese",
    "vad": {
      "meanProbability": 0.7,
      "peakProbability": 0.9,
      "speechRatio": 0.8,
    },
    "options": {
      "generationTokensPerSecond": 24.0,
      "minGenerationTokens": 48,
      "maxGenerationTokens": 1_024,
      "temperature": 0.1,
      "repetitionPenalty": 1.2,
    },
  }


def test_models_list_exposes_public_cache_fields_without_snapshot_paths(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  (snapshot / "config.json").write_text('{"support_languages":["Japanese"]}')
  dispatcher = WorkerDispatcher(
    model_manager=ModelManager(cache_scanner=lambda: cache_info(snapshot)),
    cache_clearer=lambda: None,
  )

  result = dispatcher.dispatch(
    "models.list",
    {"requestId": "list-request", "operation": "models.list"},
    b"",
  )

  assert result.value == {
    "models": [
      {
        "repoId": "owner/model",
        "revision": "commit",
        "size": "2.5GB",
        "lastModified": "2023-11-14T22:13:20+00:00",
        "refs": ["main"],
        "supportedLanguages": ["Japanese"],
      }
    ]
  }
  assert "snapshot" not in str(result.value).lower()


def test_model_load_resolves_the_cache_path_internally_and_reuses_one_lease(tmp_path: Path) -> None:
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  factory = FakeServiceFactory()
  dispatcher = WorkerDispatcher(
    model_manager=ModelManager(cache_scanner=lambda: cache_info(snapshot)),
    service_factory=factory,
    cache_clearer=lambda: None,
  )
  request = {
    "requestId": "load-one",
    "operation": "model.load",
    "repoId": "owner/model",
    "revision": "commit",
  }

  first = dispatcher.dispatch("model.load", request, b"")
  second = dispatcher.dispatch("model.load", {**request, "requestId": "load-two"}, b"")

  assert first.value == {
    "repoId": "owner/model",
    "revision": "commit",
    "loadMs": 37,
    "reused": False,
  }
  assert second.value == {**first.value, "reused": True}
  assert factory.paths == [snapshot.resolve()]
  assert factory.load_count == 1


def test_segment_transcription_echoes_opaque_identity_and_uses_implicit_mono_f32le(tmp_path: Path) -> None:
  dispatcher, factory, snapshot = dispatcher_with_model(tmp_path)
  samples = np.array([0.25, -0.5, 0.75], dtype="<f4")

  result = dispatcher.dispatch("segment.transcribe", segment_request(), samples.tobytes())

  assert result.value == {
    "sessionId": "opaque session",
    "runId": "opaque run",
    "jobId": "opaque job",
    "segmentIndex": 7,
    "text": "recognized",
    "rawText": " recognized ",
    "language": "Japanese",
    "diagnostics": {
      "maxTokens": 64,
      "generationTokens": 8,
      "promptTokens": 4,
      "totalTokens": 12,
      "modelTotalTimeMs": 125,
      "retryCount": 1,
      "tokenLimitReached": False,
      "warning": None,
    },
  }
  segment, language, model, config = factory.transcriptions[0]
  assert segment.start_sample == 160
  assert segment.sample_rate == 16_000
  np.testing.assert_array_equal(segment.audio, samples)
  assert language == "Japanese"
  assert model is factory.runtime
  assert config == TranscriptionConfig(
    generation_tokens_per_sec=24.0,
    min_generation_tokens=48,
    max_generation_tokens=1_024,
    temperature=0.1,
    repetition_penalty=1.2,
  )
  assert factory.paths[-1] == snapshot


@pytest.mark.parametrize(
  "mutation",
  [
    lambda request: request.update({"snapshotPath": "/private/path"}),
    lambda request: cast_mapping(request["vad"]).update({"unknown": 1}),
    lambda request: cast_mapping(request["options"]).update({"unknown": 1}),
  ],
)
def test_unknown_fields_at_every_request_level_are_protocol_fatal(
  tmp_path: Path,
  mutation: Callable[[dict[str, object]], None],
) -> None:
  dispatcher, _, _ = dispatcher_with_model(tmp_path)
  request = segment_request()
  mutation(request)

  with pytest.raises(AsrProtocolError) as raised:
    dispatcher.dispatch("segment.transcribe", request, np.zeros(3, dtype="<f4").tobytes())

  assert raised.value.code == "unknownField"


def test_segment_enforces_semantic_and_binary_limits(tmp_path: Path) -> None:
  dispatcher, _, _ = dispatcher_with_model(tmp_path)

  with pytest.raises(AsrProtocolError) as mismatch:
    dispatcher.dispatch("segment.transcribe", segment_request(), b"\x00" * 8)
  assert mismatch.value.code == "binaryLengthMismatch"

  with pytest.raises(AsrProtocolError) as oversized:
    dispatcher.dispatch("segment.transcribe", segment_request(sample_count=960_001), b"")
  assert oversized.value.code == "segmentTooLarge"

  invalid_rate = segment_request()
  invalid_rate["sampleRate"] = 48_000
  with pytest.raises(WorkerRequestError) as invalid:
    dispatcher.dispatch("segment.transcribe", invalid_rate, np.zeros(3, dtype="<f4").tobytes())
  assert invalid.value.code == "invalidRequest"


def test_model_unload_and_shutdown_have_minimal_results(tmp_path: Path) -> None:
  dispatcher, _, _ = dispatcher_with_model(tmp_path)

  unloaded = dispatcher.dispatch(
    "model.unload",
    {"requestId": "unload", "operation": "model.unload"},
    b"",
  )
  shutdown = dispatcher.dispatch(
    "shutdown",
    {"requestId": "shutdown", "operation": "shutdown"},
    b"",
  )

  assert unloaded.value == {"unloaded": True}
  assert shutdown.value == {}
  assert shutdown.should_shutdown is True


def test_server_sends_exact_hello_accepts_heartbeat_and_responds_before_shutdown() -> None:
  dispatcher = WorkerDispatcher(model_manager=ModelManager(cache_scanner=lambda: SimpleNamespace(repos=())))
  server = AsrWorkerServer(dispatcher=dispatcher, heartbeat_interval_seconds=None)
  server_socket, client_socket = socket.socketpair()
  outcome: list[int] = []

  def run_server() -> None:
    with server_socket, server_socket.makefile("rwb", buffering=0) as stream:
      outcome.append(server.serve(cast(BinaryIO, stream)))

  thread = Thread(target=run_server)
  thread.start()
  with client_socket, client_socket.makefile("rwb", buffering=0) as stream:
    typed_stream = cast(BinaryIO, stream)
    hello = read_frame(typed_stream)
    assert hello is not None
    assert hello.kind is FrameKind.HELLO
    fixture = json.loads(FIXTURE_PATH.read_text())
    assert hello.metadata == fixture["valid"][0]["metadata"]
    FrameWriter(typed_stream).write(FrameKind.HEARTBEAT, {})
    FrameWriter(typed_stream).write(
      FrameKind.REQUEST,
      {"requestId": "stop", "operation": "shutdown"},
    )
    response = read_frame(typed_stream)
    assert response is not None
    assert response.kind is FrameKind.RESPONSE
    assert response.metadata == {
      "requestId": "stop",
      "operation": "shutdown",
      "ok": True,
      "result": {},
    }

  thread.join(timeout=2)
  assert not thread.is_alive()
  assert outcome == [0]


def test_duplicate_request_id_is_connection_fatal() -> None:
  dispatcher = WorkerDispatcher(
    model_manager=ModelManager(cache_scanner=lambda: SimpleNamespace(repos=())),
    cache_clearer=lambda: None,
  )
  server = AsrWorkerServer(dispatcher=dispatcher, heartbeat_interval_seconds=None)
  server_socket, client_socket = socket.socketpair()
  outcome: list[int] = []

  def run_server() -> None:
    with server_socket, server_socket.makefile("rwb", buffering=0) as stream:
      outcome.append(server.serve(cast(BinaryIO, stream)))

  thread = Thread(target=run_server)
  thread.start()
  with client_socket, client_socket.makefile("rwb", buffering=0) as stream:
    typed_stream = cast(BinaryIO, stream)
    assert read_frame(typed_stream) is not None
    request = {"requestId": "duplicate", "operation": "models.list"}
    writer = FrameWriter(typed_stream)
    writer.write(FrameKind.REQUEST, request)
    assert read_frame(typed_stream) is not None
    writer.write(FrameKind.REQUEST, request)
    assert read_frame(typed_stream) is None

  thread.join(timeout=2)
  assert not thread.is_alive()
  assert outcome == [2]


def test_valid_request_errors_echo_operation_and_keep_the_connection_alive() -> None:
  dispatcher = WorkerDispatcher(model_manager=ModelManager(cache_scanner=lambda: SimpleNamespace(repos=())))
  server = AsrWorkerServer(dispatcher=dispatcher, heartbeat_interval_seconds=None)
  server_socket, client_socket = socket.socketpair()
  outcome: list[int] = []

  def run_server() -> None:
    with server_socket, server_socket.makefile("rwb", buffering=0) as stream:
      outcome.append(server.serve(cast(BinaryIO, stream)))

  thread = Thread(target=run_server)
  thread.start()
  with client_socket, client_socket.makefile("rwb", buffering=0) as stream:
    typed_stream = cast(BinaryIO, stream)
    assert read_frame(typed_stream) is not None
    writer = FrameWriter(typed_stream)
    writer.write(
      FrameKind.REQUEST,
      {"requestId": "invalid-load", "operation": "model.load", "repoId": "owner/model"},
    )
    error = read_frame(typed_stream)
    assert error is not None
    assert error.metadata == {
      "requestId": "invalid-load",
      "operation": "model.load",
      "ok": False,
      "error": {
        "code": "invalidRequest",
        "message": "revision must be a non-empty string",
        "recoverable": False,
      },
    }
    writer.write(FrameKind.REQUEST, {"requestId": "stop", "operation": "shutdown"})
    assert read_frame(typed_stream) is not None

  thread.join(timeout=2)
  assert not thread.is_alive()
  assert outcome == [0]


def cast_mapping(value: object) -> dict[str, object]:
  assert isinstance(value, dict)
  return cast(dict[str, object], value)


def test_shared_rasr_v1_request_fixtures_are_accepted_in_sequence(tmp_path: Path) -> None:
  fixture = json.loads(FIXTURE_PATH.read_text())
  snapshot = tmp_path / "snapshot"
  snapshot.mkdir()
  factory = FakeServiceFactory()
  dispatcher = WorkerDispatcher(
    model_manager=ModelManager(
      cache_scanner=lambda: cache_info(snapshot, "0123456789abcdef"),
    ),
    service_factory=factory,
    cache_clearer=lambda: None,
  )

  requests = [case for case in fixture["valid"] if case["kind"] == FrameKind.REQUEST]
  results = [
    dispatcher.dispatch(
      case["metadata"]["operation"],
      case["metadata"],
      bytes.fromhex(case["binaryHex"]),
    )
    for case in requests
  ]

  assert [case["name"] for case in requests] == [
    "models-list-request",
    "model-load-request",
    "segment-transcribe-request",
  ]
  assert results[2].value["sessionId"] == "session-opaque"
  assert results[2].value["runId"] == "run-opaque"
  assert results[2].value["jobId"] == "job-opaque"
  assert results[2].value["segmentIndex"] == 7


def test_shared_rasr_v1_invalid_metadata_is_protocol_fatal(tmp_path: Path) -> None:
  fixture = json.loads(FIXTURE_PATH.read_text())
  dispatcher, _, _ = dispatcher_with_model(tmp_path)

  codes: list[str] = []
  for case in fixture["invalidMetadata"]:
    with pytest.raises(AsrProtocolError) as raised:
      dispatcher.dispatch(
        case["metadata"]["operation"],
        case["metadata"],
        bytes.fromhex(case["binaryHex"]),
      )
    codes.append(raised.value.code)

  assert codes == ["unsupportedOperation", "unknownField", "binaryLengthMismatch"]

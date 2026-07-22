// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";

import { recoBridge } from "./bridge";

import type { AppEvent, SessionDetail } from "./generated/bindings";

const tauriMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listener: undefined as ((event: { payload: AppEvent }) => void) | undefined,
  unlisten: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauriMocks.invoke }));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((name: string, listener: (event: { payload: AppEvent }) => void) => {
    expect(name).toBe("app://event");
    tauriMocks.listener = listener;

    return Promise.resolve(tauriMocks.unlisten);
  }),
}));

function sessionDetail(
  rowVersion = "1",
  segmentIndex = 0,
  nextSegmentOffset: number | null = null,
): SessionDetail {
  return {
    characterCount: segmentIndex + 1,
    detectedLanguages: ["Japanese"],
    durationMs: (segmentIndex + 1) * 1_000,
    endedAt: null,
    error: null,
    id: "session-1",
    inputKind: "file",
    inputName: "audio.wav",
    model: { repoId: "owner/model", revision: "revision" },
    nextSegmentOffset,
    recognizedSegmentCount: segmentIndex + 1,
    requestedLanguage: null,
    resumeMode: "none",
    rowVersion,
    segmentCount: segmentIndex + 1,
    segments: [
      {
        endMs: (segmentIndex + 1) * 1_000,
        index: segmentIndex,
        language: "Japanese",
        splitReason: "silence",
        startMs: segmentIndex * 1_000,
        text: `segment-${segmentIndex}`,
      },
    ],
    snippet: `segment-${segmentIndex}`,
    startedAt: "2026-07-21T00:00:00.000Z",
    status: "running",
    title: "Audio",
  };
}

const queue = {
  autoAdvanceEnabled: false,
  items: [
    {
      addedAt: "2026-07-21T00:00:00Z",
      displayName: "audio.wav",
      error: null,
      id: "queue-1",
      status: "pending" as const,
      updatedAt: "2026-07-21T00:00:00Z",
    },
  ],
  revision: "4",
};

beforeEach(() => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
  tauriMocks.invoke.mockReset();
  tauriMocks.listener = undefined;
  tauriMocks.unlisten.mockReset();
});

describe("recoBridge", () => {
  it("lists microphone inputs through the typed native command", async () => {
    tauriMocks.invoke.mockResolvedValue([
      { channels: 1, id: "builtin-uid", isDefault: true, name: "MacBookのマイク" },
    ]);

    await expect(recoBridge.listAudioInputs()).resolves.toEqual([
      { channels: 1, id: "builtin-uid", isDefault: true, name: "MacBookのマイク" },
    ]);

    expect(tauriMocks.invoke).toHaveBeenCalledWith("audio_list_inputs");
  });

  it("starts microphone and system audio with distinct live sources", async () => {
    tauriMocks.invoke.mockResolvedValue({ ...sessionDetail(), inputKind: "microphone" });

    await recoBridge.startSession({
      deviceId: "builtin-uid",
      inputKind: "microphone",
      language: "Japanese",
    });

    await recoBridge.startSession({ inputKind: "systemAudio", language: null });

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(1, "session_start", {
      input: {
        language: "Japanese",
        source: { deviceId: "builtin-uid", type: "microphone" },
        title: null,
      },
    });

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(2, "session_start", {
      input: { language: null, source: { type: "systemAudio" }, title: null },
    });
  });

  it("rejects file sessions before invoking the native core", async () => {
    await expect(recoBridge.startSession({ inputKind: "file", language: null })).rejects.toThrow(
      "Files must be added through the queue",
    );

    expect(tauriMocks.invoke).not.toHaveBeenCalled();
  });

  it("lists and selects cached models through dedicated commands", async () => {
    tauriMocks.invoke.mockResolvedValueOnce({
      models: [
        {
          lastModified: "2026-07-01T00:00:00Z",
          refs: ["main"],
          repoId: "owner/model",
          revision: "revision",
          size: "2500000000",
          supportedLanguages: ["Japanese"],
        },
      ],
      state: { error: null, selected: null, status: "unselected" },
    });

    await expect(recoBridge.listModels()).resolves.toMatchObject({
      models: [{ repoId: "owner/model", revision: "revision" }],
    });

    expect(tauriMocks.invoke).toHaveBeenCalledWith("model_list");

    tauriMocks.invoke.mockResolvedValueOnce({
      error: null,
      selected: { repoId: "owner/model", revision: "revision" },
      status: "ready",
    });

    await recoBridge.selectModel({ repoId: "owner/model", revision: "revision" });

    expect(tauriMocks.invoke).toHaveBeenLastCalledWith("model_select", {
      input: { repoId: "owner/model", revision: "revision" },
    });
  });

  it("coalesces concurrent model catalog requests", async () => {
    let resolveModelList: (value: {
      models: never[];
      state: { error: null; selected: null; status: "unselected" };
    }) => void = () => undefined;
    const response = {
      models: [],
      state: { error: null, selected: null, status: "unselected" as const },
    };

    tauriMocks.invoke.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveModelList = resolve;
        }),
    );

    const first = recoBridge.listModels();
    const second = recoBridge.listModels();

    expect(tauriMocks.invoke).toHaveBeenCalledTimes(1);
    resolveModelList(response);

    await expect(Promise.all([first, second])).resolves.toEqual([
      {
        models: [],
        state: {
          errorCode: undefined,
          errorMessage: undefined,
          selected: null,
          status: "unselected",
        },
      },
      {
        models: [],
        state: {
          errorCode: undefined,
          errorMessage: undefined,
          selected: null,
          status: "unselected",
        },
      },
    ]);
  });

  it("turns typed native model errors into JavaScript errors", async () => {
    tauriMocks.invoke.mockRejectedValueOnce({
      code: "workerUnavailable",
      message: "Python worker could not start.",
      recoverable: true,
    });

    await expect(recoBridge.listModels()).rejects.toThrow("Python worker could not start.");
  });

  it("opens the native file dialog and maps the canonical queue snapshot", async () => {
    tauriMocks.invoke.mockResolvedValue({ canceled: false, queue });

    await expect(recoBridge.addFiles(null)).resolves.toEqual({
      canceled: false,
      queue: expect.objectContaining({ revision: "4" }),
    });

    expect(tauriMocks.invoke).toHaveBeenCalledWith("queue_add_files", {
      input: { language: null },
    });
  });

  it("keeps an accepted export running until the finished event arrives", async () => {
    tauriMocks.invoke.mockResolvedValue({ canceled: false, operationId: "export-1" });

    await expect(recoBridge.exportSessions(["session-1"], "txt", "Audio")).resolves.toEqual({
      canceled: false,
      completed: false,
      operationId: "export-1",
    });

    expect(tauriMocks.invoke).toHaveBeenCalledWith("export_start", {
      input: { format: "txt", sessionIds: ["session-1"] },
    });
  });

  it("sends a decimal revision with the complete queue order", async () => {
    tauriMocks.invoke.mockResolvedValue({ ...queue, items: [], revision: "8" });

    await recoBridge.reorderQueue("7", ["queue-2", "queue-1"]);

    expect(tauriMocks.invoke).toHaveBeenCalledWith("queue_reorder", {
      input: { expectedRevision: "7", itemIds: ["queue-2", "queue-1"] },
    });
  });

  it("loads all detail pages against one row version", async () => {
    tauriMocks.invoke
      .mockResolvedValueOnce(sessionDetail("2", 0, 500))
      .mockResolvedValueOnce(sessionDetail("2", 1, null));

    const session = await recoBridge.getSession("session-1");

    expect(session.rowVersion).toBe("2");
    expect(session.segments.map(({ sequence }) => sequence)).toEqual([0, 1]);

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(2, "history_get", {
      input: {
        expectedRowVersion: "2",
        segmentLimit: 500,
        segmentOffset: 500,
        sessionId: "session-1",
      },
    });
  });

  it("maps the unified typed event without losing decimal sequence values", async () => {
    const handler = vi.fn();

    await recoBridge.onApplicationEvent(handler);

    tauriMocks.listener?.({
      payload: {
        characterCount: 4,
        durationMs: 1_000,
        recognizedSegmentCount: 1,
        rowVersion: "3",
        segment: {
          endMs: 1_000,
          index: 0,
          language: "Japanese",
          splitReason: "silence",
          startMs: 0,
          text: "test",
        },
        segmentCount: 1,
        sequence: "9007199254740993",
        sessionId: "session-1",
        type: "segment.committed",
      },
    });

    expect(handler).toHaveBeenCalledWith({
      receipt: expect.objectContaining({
        rowVersion: "3",
        segment: expect.objectContaining({ id: "session-1:0", sequence: 0 }),
      }),
      sequence: "9007199254740993",
      type: "segment.committed",
    });
  });

  it("maps snapshot history, active detail, queue, and event sequence atomically", async () => {
    tauriMocks.invoke.mockResolvedValue({
      activeSession: sessionDetail("7", 1),
      history: { items: [sessionDetail("7", 1)], nextCursor: "cursor" },
      model: {
        error: null,
        selected: { repoId: "owner/model", revision: "revision" },
        status: "ready",
      },
      queue,
      sequence: "42",
    });

    await expect(recoBridge.getSnapshot()).resolves.toMatchObject({
      activeSessionId: "session-1",
      nextCursor: "cursor",
      queue: { revision: "4" },
      sequence: "42",
      sessions: [
        expect.objectContaining({ id: "session-1", rowVersion: "7", segmentsLoaded: true }),
      ],
    });
  });
});

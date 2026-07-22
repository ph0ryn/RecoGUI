// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";

import { recoBridge } from "./bridge";

import type { EngineEvent } from "./types";

const tauriMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listener: undefined as ((event: { payload: EngineEvent }) => void) | undefined,
  unlisten: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauriMocks.invoke }));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_name: string, listener: (event: { payload: EngineEvent }) => void) => {
    tauriMocks.listener = listener;

    return Promise.resolve(tauriMocks.unlisten);
  }),
}));

function rawSession(rowVersion: number, segmentIndex: number, nextSegmentOffset: number | null) {
  return {
    characters: segmentIndex + 1,
    detectedLanguages: ["Japanese"],
    language: "Japanese",
    mediaDurationMs: (segmentIndex + 1) * 1_000,
    model: "model",
    nextSegmentOffset,
    rowVersion,
    segments: [
      {
        endSample: (segmentIndex + 1) * 16_000,
        language: "Japanese",
        segmentIndex,
        startSample: segmentIndex * 16_000,
        text: `segment-${segmentIndex}`,
      },
    ],
    sessionId: "session-1",
    sourceDisplayName: "audio.wav",
    sourceKind: "file",
    startedAt: "2026-07-21T00:00:00.000Z",
    state: "running",
    title: "Audio",
    totalSegments: segmentIndex + 1,
  };
}

beforeEach(() => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
  tauriMocks.invoke.mockReset();
  tauriMocks.listener = undefined;
  tauriMocks.unlisten.mockReset();
});

describe("recoBridge", () => {
  it("lists microphone inputs through the native Tauri command", async () => {
    tauriMocks.invoke.mockResolvedValue({
      inputs: [
        { channels: 1, id: "builtin-uid", isDefault: true, name: "MacBookのマイク" },
        { channels: 2, id: 42, isDefault: false, name: "USBマイク" },
      ],
    });

    await expect(recoBridge.listAudioInputs()).resolves.toEqual([
      { channels: 1, id: "builtin-uid", isDefault: true, name: "MacBookのマイク" },
      { channels: 2, id: "42", isDefault: false, name: "USBマイク" },
    ]);

    expect(tauriMocks.invoke).toHaveBeenCalledWith("audio_list_inputs");
  });

  it("starts microphone and desktop audio with distinct engine sources", async () => {
    tauriMocks.invoke.mockResolvedValue({ rowVersion: 1, sessionId: "session-live" });

    await recoBridge.startSession({
      deviceId: "builtin-uid",
      inputKind: "microphone",
      language: "Japanese",
    });

    await recoBridge.startSession({ inputKind: "systemAudio", language: null });

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(1, "engine_request", {
      command: "session.start",
      payload: {
        language: "Japanese",
        source: { deviceId: "builtin-uid", type: "microphone" },
        title: undefined,
      },
    });

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(2, "engine_request", {
      command: "session.start",
      payload: { language: null, source: { type: "systemAudio" }, title: undefined },
    });
  });

  it("rejects an unknown history source instead of mapping it to microphone", async () => {
    tauriMocks.invoke.mockResolvedValue({
      items: [{ ...rawSession(1, 0, null), sourceKind: "futureAudioSource" }],
      nextCursor: null,
    });

    await expect(recoBridge.listHistory()).rejects.toThrow(
      "Unsupported input kind: futureAudioSource",
    );
  });

  it("lists cached models through the fixed model command", async () => {
    tauriMocks.invoke.mockResolvedValue({
      models: [
        {
          lastModified: "2 months ago",
          refs: ["main"],
          repoId: "owner/model",
          revision: "revision",
          size: "2.5G",
        },
      ],
      state: { selected: null, status: "unselected" },
    });

    await expect(recoBridge.listModels()).resolves.toMatchObject({
      models: [{ repoId: "owner/model", revision: "revision" }],
    });

    expect(tauriMocks.invoke).toHaveBeenCalledWith("engine_request", {
      command: "model.list",
      payload: {},
    });
  });

  it("selects a cached model by repository and revision only", async () => {
    tauriMocks.invoke.mockResolvedValue({
      selected: { repoId: "owner/model", revision: "revision" },
      status: "ready",
    });

    await recoBridge.selectModel({ repoId: "owner/model", revision: "revision" });

    expect(tauriMocks.invoke).toHaveBeenCalledWith("engine_request", {
      command: "model.select",
      payload: { repoId: "owner/model", revision: "revision" },
    });
  });

  it("enqueues selected file tokens and maps the canonical queue snapshot", async () => {
    tauriMocks.invoke.mockResolvedValue({
      autoAdvanceEnabled: false,
      items: [
        {
          addedAt: "2026-07-21T00:00:00Z",
          displayName: "audio.wav",
          errorCode: null,
          errorMessage: null,
          itemId: "queue-1",
          state: "pending",
          updatedAt: "2026-07-21T00:00:00Z",
        },
      ],
      revision: 4,
    });

    const snapshot = await recoBridge.enqueueFiles(
      [{ displayName: "audio.wav", sourceToken: "token-1" }],
      null,
    );

    expect(tauriMocks.invoke).toHaveBeenCalledWith("engine_request", {
      command: "queue.enqueueFiles",
      payload: {
        files: [{ displayName: "audio.wav", sourceToken: "token-1" }],
        language: null,
      },
    });

    expect(snapshot).toEqual({
      autoAdvanceEnabled: false,
      items: [
        expect.objectContaining({ displayName: "audio.wav", id: "queue-1", status: "pending" }),
      ],
      revision: 4,
    });
  });

  it("sends a revision with the complete queue order", async () => {
    tauriMocks.invoke.mockResolvedValue({
      autoAdvanceEnabled: false,
      items: [],
      revision: 8,
    });

    await recoBridge.reorderQueue(7, ["queue-2", "queue-1"]);

    expect(tauriMocks.invoke).toHaveBeenCalledWith("engine_request", {
      command: "queue.reorder",
      payload: { itemIds: ["queue-2", "queue-1"], revision: 7 },
    });
  });

  it("restarts paginated detail loading when the database revision changes", async () => {
    tauriMocks.invoke
      .mockResolvedValueOnce(rawSession(1, 0, 500))
      .mockResolvedValueOnce(rawSession(2, 1, null))
      .mockResolvedValueOnce(rawSession(2, 0, 500))
      .mockResolvedValueOnce(rawSession(2, 1, null));

    const session = await recoBridge.getSession("session-1");

    expect(session.rowVersion).toBe(2);
    expect(session.segments.map(({ sequence }) => sequence)).toEqual([0, 1]);

    expect(tauriMocks.invoke.mock.calls.map(([, input]) => input.payload.segmentOffset)).toEqual([
      0, 500, 0, 500,
    ]);
  });

  it("preserves the envelope sequence and receipt aggregates while mapping a segment", async () => {
    const handler = vi.fn();

    await recoBridge.onEngineEvent(handler);

    tauriMocks.listener?.({
      payload: {
        event: "segment.persisted",
        payload: {
          characters: 4,
          mediaDurationMs: 1_000,
          recognizedSegments: 1,
          rowVersion: 3,
          segment: {
            endSample: 16_000,
            segmentIndex: 0,
            startSample: 0,
            text: "test",
          },
          totalSegments: 1,
        },
        sequence: 9,
        sessionId: "session-1",
      } as unknown as EngineEvent,
    });

    expect(handler).toHaveBeenCalledWith({
      event: "segment.persisted",
      payload: expect.objectContaining({
        characters: 4,
        rowVersion: 3,
        segment: expect.objectContaining({ id: "session-1:0", sequence: 0 }),
        totalSegments: 1,
      }),
      sequence: 9,
      sessionId: "session-1",
    });
  });

  it("keeps a growing active session as a summary when detail retries stay inconsistent", async () => {
    const history = { items: [rawSession(7, 6, null)], nextCursor: null };

    tauriMocks.invoke
      .mockResolvedValueOnce({ activeSession: "session-1" })
      .mockResolvedValueOnce(history)
      .mockResolvedValueOnce({
        selected: { repoId: "owner/model", revision: "revision" },
        status: "ready",
      })
      .mockResolvedValueOnce(rawSession(1, 0, 500))
      .mockResolvedValueOnce(rawSession(2, 1, null))
      .mockResolvedValueOnce(rawSession(3, 0, 500))
      .mockResolvedValueOnce(rawSession(4, 1, null))
      .mockResolvedValueOnce(rawSession(5, 0, 500))
      .mockResolvedValueOnce(rawSession(6, 1, null));

    const snapshot = await recoBridge.getSnapshot();

    expect(snapshot.activeSessionId).toBe("session-1");

    expect(snapshot.sessions).toEqual([
      expect.objectContaining({ id: "session-1", rowVersion: 7 }),
    ]);

    expect(snapshot.sessions[0]).not.toHaveProperty("segments");
  });
});

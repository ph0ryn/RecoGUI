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
    language: "Japanese",
    mediaDurationMs: (segmentIndex + 1) * 1_000,
    model: "model",
    nextSegmentOffset,
    rowVersion,
    segments: [
      {
        endSample: (segmentIndex + 1) * 16_000,
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
      .mockResolvedValueOnce({ activeSession: "session-1", modelState: "ready" })
      .mockResolvedValueOnce(history)
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

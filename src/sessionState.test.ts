import { describe, expect, it } from "vitest";

import { initialSessionState, sessionStateReducer } from "./sessionState";

import type {
  PersistedSegmentReceipt,
  SessionDetail,
  SessionSummary,
  TranscriptSegment,
} from "./types";

function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    characterCount: 3,
    durationMs: 1_000,
    id: "session-1",
    inputKind: "file",
    inputName: "audio.wav",
    language: "ja",
    model: "model",
    rowVersion: 1,
    segmentCount: 1,
    startedAt: "2026-07-21T00:00:00.000Z",
    status: "running",
    title: "Audio",
    ...overrides,
  };
}

function segment(sequence: number, text = `segment-${sequence}`): TranscriptSegment {
  return {
    endMs: sequence * 1_000,
    id: `session-1:${sequence}`,
    sequence,
    startMs: (sequence - 1) * 1_000,
    text,
  };
}

function detail(overrides: Partial<SessionDetail> = {}): SessionDetail {
  return {
    ...summary(),
    segments: [segment(1)],
    segmentsLoaded: true,
    ...overrides,
  };
}

function receipt(overrides: Partial<PersistedSegmentReceipt> = {}): PersistedSegmentReceipt {
  return {
    characters: 9,
    mediaDurationMs: 2_000,
    recognizedSegments: 2,
    rowVersion: 2,
    segment: segment(2),
    sessionId: "session-1",
    totalSegments: 2,
    ...overrides,
  };
}

describe("sessionStateReducer", () => {
  it("upserts duplicate and out-of-order segments by sequence", () => {
    let state = sessionStateReducer(initialSessionState, {
      sessions: [detail({ segments: [] })],
      type: "bootstrap",
    });

    state = sessionStateReducer(state, {
      receipt: receipt({ rowVersion: 3, segment: segment(3), totalSegments: 3 }),
      type: "segmentPersisted",
    });

    state = sessionStateReducer(state, {
      receipt: receipt({ rowVersion: 2, segment: segment(2), totalSegments: 2 }),
      type: "segmentPersisted",
    });

    state = sessionStateReducer(state, {
      receipt: receipt({ rowVersion: 3, segment: segment(2, "corrected"), totalSegments: 3 }),
      type: "segmentPersisted",
    });

    expect(state.sessionsById["session-1"]!.segments).toEqual([
      segment(2, "corrected"),
      segment(3),
    ]);

    expect(state.sessionsById["session-1"]!.segmentCount).toBe(3);
  });

  it("does not let an older detail response overwrite a newer live revision", () => {
    let state = sessionStateReducer(initialSessionState, {
      sessions: [detail({ segments: [] })],
      type: "bootstrap",
    });

    state = sessionStateReducer(state, {
      receipt: receipt({ rowVersion: 3, segment: segment(3), totalSegments: 3 }),
      type: "segmentPersisted",
    });

    state = sessionStateReducer(state, {
      session: detail({ rowVersion: 2, segments: [segment(1), segment(2)] }),
      type: "detailLoaded",
    });

    expect(state.sessionsById["session-1"]!.rowVersion).toBe(3);

    expect(state.sessionsById["session-1"]!.segments.map(({ sequence }) => sequence)).toEqual([
      1, 2, 3,
    ]);
  });

  it("clears a stale active session from a canonical bootstrap", () => {
    const active = sessionStateReducer(initialSessionState, {
      activeSessionId: "session-1",
      sessions: [summary()],
      type: "bootstrap",
    });

    const reconciled = sessionStateReducer(active, {
      sessions: [summary({ status: "completed" })],
      type: "bootstrap",
    });

    expect(reconciled.activeSessionId).toBeUndefined();
  });

  it("infers the active session from a running snapshot when its id is omitted", () => {
    const state = sessionStateReducer(initialSessionState, {
      sessions: [summary({ inputKind: "file", status: "running" })],
      type: "bootstrap",
    });

    expect(state.activeSessionId).toBe("session-1");
  });

  it("preserves loaded detail when a summary page is merged", () => {
    const state = sessionStateReducer(
      sessionStateReducer(initialSessionState, {
        sessions: [detail()],
        type: "bootstrap",
      }),
      {
        sessions: [summary({ rowVersion: 2, title: "Renamed" })],
        type: "historyPageLoaded",
      },
    );

    expect(state.sessionsById["session-1"]).toMatchObject({
      rowVersion: 2,
      segments: [segment(1)],
      segmentsLoaded: true,
      title: "Renamed",
    });
  });

  it("replaces optimistic detail during canonical reconciliation", () => {
    let state = sessionStateReducer(initialSessionState, {
      sessions: [detail({ rowVersion: 3, segments: [segment(1), segment(2), segment(3)] })],
      type: "bootstrap",
    });

    state = sessionStateReducer(state, {
      session: detail({ rowVersion: 3, segmentCount: 2, segments: [segment(1), segment(2)] }),
      type: "canonicalReconciled",
    });

    expect(state.sessionsById["session-1"]!.segments.map(({ sequence }) => sequence)).toEqual([
      1, 2,
    ]);
  });

  it("keeps an active session through stopping and clears it at a terminal state", () => {
    let state = sessionStateReducer(initialSessionState, {
      activeSessionId: "session-1",
      sessions: [summary()],
      type: "bootstrap",
    });

    state = sessionStateReducer(state, {
      rowVersion: 2,
      sessionId: "session-1",
      status: "stopping",
      type: "statusChanged",
    });

    expect(state.activeSessionId).toBe("session-1");

    state = sessionStateReducer(state, {
      rowVersion: 3,
      sessionId: "session-1",
      status: "stopped",
      type: "statusChanged",
    });

    expect(state.activeSessionId).toBeUndefined();
  });

  it("releases the active slot when paused and claims it again when resumed", () => {
    let state = sessionStateReducer(initialSessionState, {
      activeSessionId: "session-1",
      sessions: [summary()],
      type: "bootstrap",
    });

    state = sessionStateReducer(state, {
      rowVersion: 2,
      sessionId: "session-1",
      status: "paused",
      type: "statusChanged",
    });

    expect(state.activeSessionId).toBeUndefined();

    state = sessionStateReducer(state, {
      rowVersion: 3,
      sessionId: "session-1",
      status: "preparing",
      type: "statusChanged",
    });

    expect(state.activeSessionId).toBe("session-1");
  });
});

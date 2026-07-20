/* oxlint-disable no-ternary, curly */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { mockSnapshot } from "./mockData";

import type {
  AudioInput,
  EngineEvent,
  EngineSnapshot,
  ExportFormat,
  ExportResult,
  HistoryPage,
  InputKind,
  ModelState,
  SessionDetail,
  SessionSummary,
  SessionStatus,
  TranscriptSegment,
} from "./types";

interface StartSessionInput {
  deviceId?: string;
  inputKind: InputKind;
  inputToken?: string;
  inputName?: string;
}

interface HistoryQuery {
  cursor?: string;
  inputKind?: InputKind;
  limit?: number;
  query?: string;
  status?: SessionStatus;
}

interface EngineRequest<T> {
  command: string;
  payload?: T;
}

interface RawEngineState {
  activeSession: string | null;
  modelState: string;
}

interface RawHistoryPage {
  items: RawSession[];
  nextCursor: string | null;
}

interface RawSession {
  sessionId: string;
  state: string;
  title: string;
  sourceKind: string;
  sourceDisplayName: string;
  model: string;
  language: string;
  rowVersion: number;
  startedAt: string;
  endedAt?: string | null;
  mediaDurationMs?: number;
  totalSegments?: number;
  characters?: number;
  snippet?: string | null;
  errorCode?: string | null;
  errorMessage?: string | null;
  segments?: RawSegment[];
  nextSegmentOffset?: number | null;
}

interface RawSegment {
  segmentIndex: number;
  startSample: number;
  endSample: number;
  text: string;
}

class SessionSnapshotChangedError extends Error {
  constructor() {
    super("Session changed while its transcript was being loaded");
    this.name = "SessionSnapshotChangedError";
  }
}

function hasTauriRuntime(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

async function request<Result, Payload = unknown>(
  command: string,
  payload?: Payload,
): Promise<Result> {
  return invoke<Result>("engine_request", {
    ...({ command, payload: payload ?? ({} as Payload) } satisfies EngineRequest<Payload>),
  });
}

function modelStatus(value: string): ModelState["status"] {
  if (["missing", "downloading", "verifying", "loading"].includes(value)) {
    return value as ModelState["status"];
  }

  if (["ready", "loaded"].includes(value)) {
    return "ready";
  }

  return "failed";
}

function mapSegment(sessionId: string, segment: RawSegment): TranscriptSegment {
  const sequence = segment.segmentIndex;

  return {
    endMs: Math.round(segment.endSample / 16),
    id: `${sessionId}:${sequence}`,
    sequence,
    startMs: Math.round(segment.startSample / 16),
    text: segment.text,
  };
}

function mapSessionSummary(value: RawSession): SessionSummary {
  return {
    characterCount: value.characters ?? 0,
    durationMs: value.mediaDurationMs ?? 0,
    endedAt: value.endedAt ?? undefined,
    errorCode: value.errorCode ?? undefined,
    errorMessage: value.errorMessage ?? undefined,
    id: value.sessionId,
    inputKind: value.sourceKind === "file" ? "file" : "microphone",
    inputName: value.sourceDisplayName,
    language: value.language,
    model: value.model,
    rowVersion: value.rowVersion,
    segmentCount: value.totalSegments ?? 0,
    snippet: value.snippet ?? undefined,
    startedAt: value.startedAt,
    status: value.state as SessionStatus,
    title: value.title,
  };
}

function mapSessionDetail(value: RawSession): SessionDetail {
  return {
    ...mapSessionSummary(value),
    segments: (value.segments ?? []).map((segment) => mapSegment(value.sessionId, segment)),
    segmentsLoaded: true,
  };
}

export const recoBridge = {
  async cancelExport(operationId: string): Promise<void> {
    if (hasTauriRuntime()) await request("history.cancelExport", { operationId });
  },

  async cancelSession(sessionId: string): Promise<void> {
    if (hasTauriRuntime()) await request("session.cancel", { sessionId });
  },

  async deleteModel(): Promise<void> {
    if (hasTauriRuntime()) await request("model.delete");
  },

  async deleteSessions(sessionIds: string[]): Promise<void> {
    if (hasTauriRuntime()) {
      await request(
        sessionIds.length === 1 ? "history.delete" : "history.deleteMany",
        sessionIds.length === 1 ? { sessionId: sessionIds[0] } : { sessionIds },
      );
    }
  },

  async downloadModel(): Promise<void> {
    if (hasTauriRuntime()) await request("model.download");
  },

  async exportSessions(sessionIds: string[], format: ExportFormat): Promise<ExportResult> {
    if (!hasTauriRuntime()) {
      return { canceled: false, completed: true, operationId: crypto.randomUUID() };
    }

    let extension: string = format;

    if (format === "markdown") {
      extension = "md";
    }

    if (sessionIds.length > 1) {
      extension = "zip";
    }

    const destination = await invoke<{ displayName: string; token: string } | null>(
      "select_export_destination",
      {
        extension,
        suggestedName: sessionIds.length > 1 ? "Reco-exports" : "Reco-transcript",
      },
    );

    if (!destination) {
      return { canceled: true };
    }

    const result = await request<Omit<ExportResult, "canceled">>(
      sessionIds.length === 1 ? "history.export" : "history.exportMany",
      {
        destinationToken: destination.token,
        format,
        overwrite: true,
        sessionIds,
      },
    );

    return { ...result, canceled: false };
  },

  async getSession(sessionId: string): Promise<SessionDetail> {
    if (!hasTauriRuntime()) {
      const session = mockSnapshot.sessions.find(({ id }) => id === sessionId);

      if (!session) throw new Error(`Unknown session: ${sessionId}`);
      if (!("segments" in session)) throw new Error(`Session detail is unavailable: ${sessionId}`);

      return structuredClone(session);
    }

    for (let attempt = 0; attempt < 3; attempt += 1) {
      const first = await request<
        RawSession,
        { segmentLimit: number; segmentOffset: number; sessionId: string }
      >("history.get", { segmentLimit: 500, segmentOffset: 0, sessionId });
      const segments = [...(first.segments ?? [])];
      let nextOffset = first.nextSegmentOffset;
      let consistent = true;

      while (nextOffset !== null && nextOffset !== undefined) {
        const page = await request<
          RawSession,
          { segmentLimit: number; segmentOffset: number; sessionId: string }
        >("history.get", { segmentLimit: 500, segmentOffset: nextOffset, sessionId });

        if (page.rowVersion !== first.rowVersion) {
          consistent = false;
          break;
        }

        segments.push(...(page.segments ?? []));
        nextOffset = page.nextSegmentOffset;
      }

      if (consistent) return mapSessionDetail({ ...first, segments });
    }

    throw new SessionSnapshotChangedError();
  },

  async getSnapshot(): Promise<EngineSnapshot> {
    if (!hasTauriRuntime()) return structuredClone(mockSnapshot);

    const [engine, history] = await Promise.all([
      request<RawEngineState>("engine.getState"),
      request<RawHistoryPage, { limit: number }>("history.list", { limit: 100 }),
    ]);
    const sessions: (SessionSummary | SessionDetail)[] = history.items.map(mapSessionSummary);

    if (engine.activeSession) {
      try {
        const active = await recoBridge.getSession(engine.activeSession);
        const index = sessions.findIndex(({ id }) => id === active.id);

        if (index >= 0) {
          sessions[index] = active;
        } else {
          sessions.unshift(active);
        }
      } catch (error) {
        if (!(error instanceof SessionSnapshotChangedError)) throw error;
      }
    }

    return {
      activeSessionId: engine.activeSession ?? undefined,
      model: { status: modelStatus(engine.modelState) },
      nextCursor: history.nextCursor ?? undefined,
      sessions,
    };
  },

  async listAudioInputs(): Promise<AudioInput[]> {
    if (!hasTauriRuntime()) {
      return [
        { channels: 1, id: "default", name: "MacBookのマイク" },
        { channels: 2, id: "external", name: "外部マイク" },
      ];
    }

    const result = await request<{
      inputs: { channels: number; id: number | string; name: string }[];
    }>("audio.listInputs");

    return result.inputs.map((input) => ({ ...input, id: String(input.id) }));
  },

  async listHistory(query: HistoryQuery = {}): Promise<HistoryPage> {
    if (!hasTauriRuntime()) {
      const items = mockSnapshot.sessions.filter(
        (session) =>
          (!query.status || session.status === query.status) &&
          (!query.inputKind || session.inputKind === query.inputKind),
      );

      return { items: structuredClone(items) };
    }

    const result = await request<RawHistoryPage>("history.list", {
      cursor: query.cursor,
      limit: query.limit ?? 50,
      sourceKind: query.inputKind,
      states: query.status ? [query.status] : [],
    });

    return {
      items: result.items.map(mapSessionSummary),
      nextCursor: result.nextCursor ?? undefined,
    };
  },

  async onCloseForceRequired(
    handler: (payload: { error: string; sessionId: string | null }) => void,
  ): Promise<UnlistenFn> {
    if (!hasTauriRuntime()) return () => undefined;

    return listen<{ error: string; sessionId: string | null }>(
      "host://close-force-required",
      ({ payload }) => handler(payload),
    );
  },

  async onCloseRequested(handler: (payload: { sessionId: string }) => void): Promise<UnlistenFn> {
    if (!hasTauriRuntime()) return () => undefined;

    return listen<{ sessionId: string }>("host://close-requested", ({ payload }) =>
      handler(payload),
    );
  },

  async onEngineEvent(handler: (event: EngineEvent) => void): Promise<UnlistenFn> {
    if (!hasTauriRuntime()) return () => undefined;

    return listen<EngineEvent>("engine://event", ({ payload }) => {
      if (payload.event !== "segment.persisted") {
        handler(payload);

        return;
      }

      const eventPayload = payload.payload as { segment?: RawSegment };

      if (!payload.sessionId || !eventPayload.segment) {
        handler(payload);

        return;
      }

      handler({
        ...payload,
        payload: {
          ...eventPayload,
          segment: mapSegment(payload.sessionId, eventPayload.segment),
        },
      });
    });
  },

  async pickAudioFile(): Promise<{ inputName: string; inputToken: string } | null> {
    if (!hasTauriRuntime()) return { inputName: "選択した音声.wav", inputToken: "mock-file-token" };

    const selected = await invoke<{ displayName: string; token: string } | null>(
      "select_audio_file",
    );

    return selected ? { inputName: selected.displayName, inputToken: selected.token } : null;
  },

  async resolveClose(resolution: "cancel" | "stopAndQuit" | "forceQuit"): Promise<void> {
    if (hasTauriRuntime()) {
      await invoke("host_resolve_close_request", { input: { resolution } });
    }
  },

  async searchHistory(query: HistoryQuery): Promise<HistoryPage> {
    if (!query.query?.trim()) return this.listHistory(query);

    if (!hasTauriRuntime()) {
      const normalizedQuery = query.query.trim().toLocaleLowerCase("ja-JP");
      const items = mockSnapshot.sessions.filter((session) => {
        const texts = "segments" in session ? session.segments.map(({ text }) => text) : [];
        const textMatches = [session.title, ...texts].some((value) =>
          value.toLocaleLowerCase("ja-JP").includes(normalizedQuery),
        );

        return (
          textMatches &&
          (!query.status || session.status === query.status) &&
          (!query.inputKind || session.inputKind === query.inputKind)
        );
      });

      return { items: structuredClone(items) };
    }

    const result = await request<RawHistoryPage>("history.search", {
      cursor: query.cursor,
      limit: query.limit ?? 50,
      query: query.query,
      source: query.inputKind,
      status: query.status,
    });

    return {
      items: result.items.map(mapSessionSummary),
      nextCursor: result.nextCursor ?? undefined,
    };
  },

  async startSession(input: StartSessionInput): Promise<{ rowVersion: number; sessionId: string }> {
    if (!hasTauriRuntime()) return { rowVersion: 1, sessionId: crypto.randomUUID() };

    const source =
      input.inputKind === "microphone"
        ? { deviceId: input.deviceId, type: "microphone" }
        : { sourceToken: input.inputToken, type: "file" };

    return request("session.start", { source, title: input.inputName });
  },

  async stopSession(sessionId: string): Promise<void> {
    if (hasTauriRuntime()) await request("session.stop", { sessionId });
  },

  async verifyModel(): Promise<void> {
    if (hasTauriRuntime()) await request("model.verify");
  },
};

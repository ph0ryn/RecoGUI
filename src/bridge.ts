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
  SessionStatus,
  TranscriptSegment,
  TranscriptionSession,
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
  segmentIndex?: number;
  sequence?: number;
  startSample: number;
  endSample: number;
  text: string;
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
  const sequence = segment.segmentIndex ?? segment.sequence ?? 0;

  return {
    endMs: Math.round(segment.endSample / 16),
    id: `${sessionId}:${sequence}`,
    sequence,
    startMs: Math.round(segment.startSample / 16),
    text: segment.text,
  };
}

function mapSession(value: RawSession, detailsLoaded = false): TranscriptionSession {
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
    segmentCount: value.totalSegments ?? 0,
    segments: (value.segments ?? []).map((segment) => mapSegment(value.sessionId, segment)),
    segmentsLoaded: detailsLoaded,
    snippet: value.snippet ?? undefined,
    startedAt: value.startedAt,
    status: value.state as SessionStatus,
    title: value.title,
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

  async getSession(sessionId: string): Promise<TranscriptionSession> {
    if (!hasTauriRuntime()) {
      const session = mockSnapshot.sessions.find(({ id }) => id === sessionId);

      if (!session) throw new Error(`Unknown session: ${sessionId}`);

      return structuredClone(session);
    }

    const result = await request<
      RawSession,
      { segmentLimit: number; segmentOffset: number; sessionId: string }
    >("history.get", { segmentLimit: 500, segmentOffset: 0, sessionId });

    while (result.nextSegmentOffset !== null && result.nextSegmentOffset !== undefined) {
      const page = await request<
        RawSession,
        { segmentLimit: number; segmentOffset: number; sessionId: string }
      >("history.get", {
        segmentLimit: 500,
        segmentOffset: result.nextSegmentOffset,
        sessionId,
      });

      result.segments = [...(result.segments ?? []), ...(page.segments ?? [])];
      result.nextSegmentOffset = page.nextSegmentOffset;
    }

    return mapSession(result, true);
  },

  async getSnapshot(): Promise<EngineSnapshot> {
    if (!hasTauriRuntime()) return structuredClone(mockSnapshot);

    const [engine, history] = await Promise.all([
      request<RawEngineState>("engine.getState"),
      request<RawHistoryPage, { limit: number }>("history.list", { limit: 100 }),
    ]);
    const sessions = history.items.map((session) => mapSession(session));

    if (engine.activeSession) {
      const active = await recoBridge.getSession(engine.activeSession);
      const index = sessions.findIndex(({ id }) => id === active.id);

      if (index >= 0) {
        sessions[index] = active;
      } else {
        sessions.unshift(active);
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
      items: result.items.map((session) => mapSession(session)),
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
        const textMatches = [session.title, ...session.segments.map(({ text }) => text)].some(
          (value) => value.toLocaleLowerCase("ja-JP").includes(normalizedQuery),
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
      items: result.items.map((session) => mapSession(session)),
      nextCursor: result.nextCursor ?? undefined,
    };
  },

  async startSession(input: StartSessionInput): Promise<{ sessionId: string }> {
    if (!hasTauriRuntime()) return { sessionId: crypto.randomUUID() };

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

/* oxlint-disable no-ternary, curly, @stylistic/padding-line-between-statements */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";

import { mockSnapshot } from "./mockData";

import type {
  AudioInput,
  EngineEvent,
  EngineSnapshot,
  ExportFormat,
  ExportResult,
  HistoryPage,
  InputKind,
  ModelList,
  ModelReference,
  ModelState,
  QueueSnapshot,
  SelectedAudioFile,
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

interface RawQueueItem {
  itemId: string;
  displayName: string;
  state: "pending" | "invalid";
  addedAt: string;
  updatedAt: string;
  errorCode?: string | null;
  errorMessage?: string | null;
}

interface RawQueueSnapshot {
  revision: number;
  autoAdvanceEnabled: boolean;
  items: RawQueueItem[];
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

function mapQueueSnapshot(value: RawQueueSnapshot): QueueSnapshot {
  return {
    autoAdvanceEnabled: value.autoAdvanceEnabled,
    items: value.items.map((item) => ({
      addedAt: item.addedAt,
      displayName: item.displayName,
      errorCode: item.errorCode ?? undefined,
      errorMessage: item.errorMessage ?? undefined,
      id: item.itemId,
      status: item.state,
      updatedAt: item.updatedAt,
    })),
    revision: value.revision,
  };
}

const mockQueueSnapshot: QueueSnapshot = {
  autoAdvanceEnabled: false,
  items: [],
  revision: 0,
};

export const recoBridge = {
  async cancelExport(operationId: string): Promise<void> {
    if (hasTauriRuntime()) await request("history.cancelExport", { operationId });
  },

  async clearQueue(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.items = [];
      mockQueueSnapshot.revision += 1;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.clear"));
  },

  async copySessions(sessionIds: string[], format: ExportFormat): Promise<void> {
    const content = await this.renderSessions(sessionIds, format);

    if (hasTauriRuntime()) {
      await writeText(content);
      return;
    }
    await navigator.clipboard.writeText(content);
  },

  async deleteSessions(sessionIds: string[]): Promise<void> {
    if (hasTauriRuntime()) {
      await request(
        sessionIds.length === 1 ? "history.delete" : "history.deleteMany",
        sessionIds.length === 1 ? { sessionId: sessionIds[0] } : { sessionIds },
      );
    }
  },

  async enqueueFiles(files: SelectedAudioFile[]): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.items.push(
        ...files.map((file) => ({
          addedAt: new Date().toISOString(),
          displayName: file.displayName,
          id: crypto.randomUUID(),
          status: "pending" as const,
          updatedAt: new Date().toISOString(),
        })),
      );
      mockQueueSnapshot.revision += 1;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.enqueueFiles", { files }));
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

  async getModelState(): Promise<ModelState> {
    if (!hasTauriRuntime()) return structuredClone(mockSnapshot.model);
    return request<ModelState>("model.getState");
  },

  async getQueueState(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) return structuredClone(mockQueueSnapshot);
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.getState"));
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

    const [engine, history, model] = await Promise.all([
      request<RawEngineState>("engine.getState"),
      request<RawHistoryPage, { limit: number }>("history.list", { limit: 100 }),
      request<ModelState>("model.getState"),
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
      model,
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

  async listModels(): Promise<ModelList> {
    if (!hasTauriRuntime()) {
      const selected = mockSnapshot.model.selected;

      return {
        models: selected
          ? [
              {
                ...selected,
                lastModified: "2 months ago",
                refs: ["main"],
                size: "2.5G",
              },
            ]
          : [],
        state: structuredClone(mockSnapshot.model),
      };
    }
    return request<ModelList>("model.list");
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
      if (payload.event === "queue.changed") {
        handler({
          ...payload,
          payload: mapQueueSnapshot(payload.payload as RawQueueSnapshot),
        });

        return;
      }
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

  async pauseQueue(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.autoAdvanceEnabled = false;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.pause"));
  },

  async pauseSession(sessionId: string): Promise<void> {
    if (hasTauriRuntime()) await request("session.pause", { sessionId });
  },

  async pickAudioFiles(): Promise<SelectedAudioFile[] | null> {
    if (!hasTauriRuntime()) {
      return [{ displayName: "選択した音声.wav", sourceToken: "mock-file-token" }];
    }

    const selected = await invoke<{ displayName: string; token: string }[] | null>(
      "select_audio_files",
    );

    return selected?.map(({ displayName, token }) => ({ displayName, sourceToken: token })) ?? null;
  },

  async removeQueueItem(itemId: string): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.items = mockQueueSnapshot.items.filter(({ id }) => id !== itemId);
      mockQueueSnapshot.revision += 1;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.remove", { itemId }));
  },

  async renameSession(
    sessionId: string,
    title: string,
  ): Promise<{ rowVersion: number; sessionId: string; title: string }> {
    if (!hasTauriRuntime()) {
      return { rowVersion: 1, sessionId, title: title.trim() };
    }

    return request("history.rename", { sessionId, title });
  },

  async renderSessions(sessionIds: string[], format: ExportFormat): Promise<string> {
    if (!hasTauriRuntime()) {
      return mockSnapshot.sessions
        .filter(({ id }) => sessionIds.includes(id))
        .flatMap((session) =>
          "segments" in session ? session.segments.map(({ text }) => text) : [],
        )
        .join("\n");
    }

    const result = await request<{ content: string }>("history.render", {
      format,
      sessionIds,
    });

    return result.content;
  },

  async reorderQueue(revision: number, itemIds: string[]): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      if (revision !== mockQueueSnapshot.revision) throw new Error("Stale queue revision");
      const itemsById = new Map(mockQueueSnapshot.items.map((item) => [item.id, item]));

      mockQueueSnapshot.items = itemIds.flatMap((id) => {
        const item = itemsById.get(id);

        return item ? [item] : [];
      });
      mockQueueSnapshot.revision += 1;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(
      await request<RawQueueSnapshot>("queue.reorder", { itemIds, revision }),
    );
  },

  async resolveClose(resolution: "cancel" | "stopAndQuit" | "forceQuit"): Promise<void> {
    if (hasTauriRuntime()) {
      await invoke("host_resolve_close_request", { input: { resolution } });
    }
  },

  async resumeSession(sessionId: string): Promise<void> {
    if (hasTauriRuntime()) await request("session.resume", { sessionId });
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

  async selectModel(model: ModelReference): Promise<ModelState> {
    if (!hasTauriRuntime()) return { selected: model, status: "ready" };
    return request<ModelState, ModelReference>("model.select", model);
  },

  async startQueue(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.autoAdvanceEnabled = mockQueueSnapshot.items.length > 0;
      return structuredClone(mockQueueSnapshot);
    }
    return mapQueueSnapshot(await request<RawQueueSnapshot>("queue.start"));
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
};

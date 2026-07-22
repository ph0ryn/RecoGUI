import { writeText } from "@tauri-apps/plugin-clipboard-manager";

import {
  commands,
  events,
  type AppEvent,
  type AppSnapshot,
  type LiveSource,
  type ModelState as ContractModelState,
  type QueueSnapshot as ContractQueueSnapshot,
  type SessionDetail as ContractSessionDetail,
  type SessionSummary as ContractSessionSummary,
  type TranscriptSegment as ContractTranscriptSegment,
} from "./generated/bindings";
import { mockSnapshot } from "./mockData";

import type {
  ApplicationEvent,
  AudioInput,
  EngineSnapshot,
  ExportFormat,
  ExportResult,
  HistoryPage,
  InputKind,
  ModelList,
  ModelReference,
  ModelState,
  QueueSnapshot,
  SessionDetail,
  SessionStatus,
  SessionSummary,
  TranscriptSegment,
} from "./types";

interface StartSessionInput {
  deviceId?: string;
  inputKind: InputKind;
  inputName?: string;
  language: string | null;
}

interface HistoryQuery {
  cursor?: string;
  inputKind?: InputKind;
  limit?: number;
  query?: string;
  status?: SessionStatus;
}

function hasTauriRuntime(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

function mapSegment(sessionId: string, value: ContractTranscriptSegment): TranscriptSegment {
  return {
    endMs: value.endMs ?? 0,
    id: `${sessionId}:${value.index}`,
    language: value.language,
    sequence: value.index,
    startMs: value.startMs ?? 0,
    text: value.text,
  };
}

function language(value: ContractSessionSummary): string {
  if (value.requestedLanguage) {
    return value.requestedLanguage;
  }

  if (value.detectedLanguages.length === 0) {
    return "自動";
  }

  return `自動 (${value.detectedLanguages.join(", ")})`;
}

function mapSummary(value: ContractSessionSummary): SessionSummary {
  return {
    characterCount: value.characterCount,
    durationMs: value.durationMs ?? 0,
    endedAt: value.endedAt ?? undefined,
    errorCode: value.error?.code,
    errorMessage: value.error?.message,
    id: value.id,
    inputKind: value.inputKind,
    inputName: value.inputName,
    language: language(value),
    model: value.model.repoId,
    rowVersion: value.rowVersion,
    segmentCount: value.segmentCount,
    snippet: value.snippet ?? undefined,
    startedAt: value.startedAt,
    status: value.status,
    title: value.title,
  };
}

function mapDetail(value: ContractSessionDetail): SessionDetail {
  return {
    ...mapSummary(value),
    segments: value.segments.map((segment) => mapSegment(value.id, segment)),
    segmentsLoaded: true,
  };
}

function mapModel(value: ContractModelState): ModelState {
  return {
    errorCode: value.error?.code,
    errorMessage: value.error?.message,
    selected: value.selected,
    status: value.status,
  };
}

function mapQueue(value: ContractQueueSnapshot): QueueSnapshot {
  return {
    autoAdvanceEnabled: value.autoAdvanceEnabled,
    items: value.items.map((item) => ({
      addedAt: item.addedAt,
      displayName: item.displayName,
      errorCode: item.error?.code,
      errorMessage: item.error?.message,
      id: item.id,
      status: item.status,
      updatedAt: item.updatedAt,
    })),
    revision: value.revision,
  };
}

function mapSnapshot(value: AppSnapshot): EngineSnapshot {
  const history = value.history.items.map(mapSummary);
  let active: SessionDetail | undefined = undefined;

  if (value.activeSession !== null) {
    active = mapDetail(value.activeSession);
  }

  const sessions: (SessionSummary | SessionDetail)[] = [...history];

  if (active) {
    const index = sessions.findIndex(({ id }) => id === active.id);

    if (index === -1) {
      sessions.unshift(active);
    } else {
      sessions[index] = active;
    }
  }

  return {
    activeSessionId: active?.id,
    model: mapModel(value.model),
    nextCursor: value.history.nextCursor ?? undefined,
    queue: mapQueue(value.queue),
    sequence: value.sequence,
    sessions,
  };
}

function mapEvent(event: AppEvent): ApplicationEvent {
  switch (event.type) {
    case "session.upserted":
      return { sequence: event.sequence, session: mapSummary(event.session), type: event.type };
    case "segment.committed":
      return {
        receipt: {
          characters: event.characterCount,
          mediaDurationMs: event.durationMs ?? 0,
          recognizedSegments: event.recognizedSegmentCount,
          rowVersion: event.rowVersion,
          segment: mapSegment(event.sessionId, event.segment),
          sessionId: event.sessionId,
          totalSegments: event.segmentCount,
        },
        sequence: event.sequence,
        type: event.type,
      };
    case "session.progress":
      return {
        processedAudioMs: event.processedAudioMs ?? 0,
        sequence: event.sequence,
        sessionId: event.sessionId,
        totalAudioMs: event.totalAudioMs ?? undefined,
        type: event.type,
      };
    case "sessions.deleted":
      return { sequence: event.sequence, sessionIds: event.sessionIds, type: event.type };
    case "queue.changed":
      return { queue: mapQueue(event.queue), sequence: event.sequence, type: event.type };
    case "model.changed":
      return { model: mapModel(event.model), sequence: event.sequence, type: event.type };
    case "export.progress":
      return {
        completedItems: event.progress.completedItems,
        operationId: event.progress.operationId,
        sequence: event.sequence,
        totalItems: event.progress.totalItems,
        type: event.type,
      };
    case "export.finished":
      return {
        canceled: event.result.canceled,
        failedSessionIds: event.result.failures.map(({ sessionId }) => sessionId).filter(Boolean),
        operationId: event.result.operationId,
        sequence: event.sequence,
        type: event.type,
      };
    case "close.confirmationRequired":
      return {
        sequence: event.sequence,
        sessionId: event.activeSessionId ?? undefined,
        type: event.type,
      };
    case "close.forceRequired":
      return {
        error: event.error.message,
        sequence: event.sequence,
        sessionId: event.activeSessionId ?? undefined,
        type: event.type,
      };
    case "notification.error":
      return { message: event.error.message, sequence: event.sequence, type: event.type };
  }
}

function historyInput(query: HistoryQuery) {
  const inputKinds: InputKind[] = [];

  if (query.inputKind) {
    inputKinds.push(query.inputKind);
  }

  const statuses: SessionStatus[] = [];

  if (query.status) {
    statuses.push(query.status);
  }

  return {
    cursor: query.cursor ?? null,
    inputKinds,
    limit: query.limit ?? 50,
    query: query.query?.trim() || null,
    sort: "newest" as const,
    statuses,
  };
}

async function currentSession(sessionId: string): Promise<SessionDetail> {
  return recoBridge.getSession(sessionId);
}

const mockQueueSnapshot: QueueSnapshot = {
  autoAdvanceEnabled: false,
  items: [],
  revision: "0",
};

export const recoBridge = {
  async addFiles(language: string | null): Promise<{ canceled: boolean; queue: QueueSnapshot }> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.items.push({
        addedAt: new Date().toISOString(),
        displayName: "選択した音声.wav",
        id: crypto.randomUUID(),
        status: "pending",
        updatedAt: new Date().toISOString(),
      });

      mockQueueSnapshot.revision = incrementMockRevision(mockQueueSnapshot.revision);

      return { canceled: false, queue: structuredClone(mockQueueSnapshot) };
    }

    const result = await commands.queueAddFiles({ language });

    return { canceled: result.canceled, queue: mapQueue(result.queue) };
  },

  async cancelExport(operationId: string): Promise<void> {
    if (hasTauriRuntime()) {
      await commands.exportCancel({ operationId });
    }
  },

  async clearQueue(revision: string): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.items = [];
      mockQueueSnapshot.revision = incrementMockRevision(mockQueueSnapshot.revision);

      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queueClear({ expectedRevision: revision }));
  },

  async copySessions(sessionIds: string[], format: ExportFormat): Promise<void> {
    const content = await this.renderSessions(sessionIds, format);

    if (hasTauriRuntime()) {
      await writeText(content);
    } else {
      await navigator.clipboard.writeText(content);
    }
  },

  async deleteSessions(sessionIds: string[]): Promise<void> {
    if (!hasTauriRuntime()) {
      return;
    }

    const sessions = await Promise.all(sessionIds.map(currentSession));

    await commands.historyDelete({
      sessions: sessions.map(({ id, rowVersion }) => ({
        expectedRowVersion: rowVersion,
        sessionId: id,
      })),
    });
  },

  async exportSessions(
    sessionIds: string[],
    format: ExportFormat,
    _sessionTitle: string,
  ): Promise<ExportResult> {
    if (!hasTauriRuntime()) {
      return { canceled: false, completed: true, operationId: crypto.randomUUID() };
    }

    const result = await commands.exportStart({ format, sessionIds });

    return {
      canceled: result.canceled,
      completed: false,
      operationId: result.operationId ?? undefined,
    };
  },

  async getModelState(): Promise<ModelState> {
    if (!hasTauriRuntime()) {
      return structuredClone(mockSnapshot.model);
    }

    return mapModel((await commands.appGetSnapshot()).model);
  },

  async getQueueState(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queueGet());
  },

  async getSession(sessionId: string): Promise<SessionDetail> {
    if (!hasTauriRuntime()) {
      const session = mockSnapshot.sessions.find(({ id }) => id === sessionId);

      if (!session || !("segments" in session)) {
        throw new Error(`Unknown session: ${sessionId}`);
      }

      return structuredClone(session);
    }

    const first = await commands.historyGet({
      expectedRowVersion: null,
      segmentLimit: 500,
      segmentOffset: 0,
      sessionId,
    });
    const segments = [...first.segments];
    let nextOffset = first.nextSegmentOffset;

    while (nextOffset !== null) {
      const page = await commands.historyGet({
        expectedRowVersion: first.rowVersion,
        segmentLimit: 500,
        segmentOffset: nextOffset,
        sessionId,
      });

      segments.push(...page.segments);
      nextOffset = page.nextSegmentOffset;
    }

    return mapDetail({ ...first, nextSegmentOffset: null, segments });
  },

  async getSnapshot(): Promise<EngineSnapshot> {
    if (!hasTauriRuntime()) {
      return { ...structuredClone(mockSnapshot), sequence: "0" };
    }

    return mapSnapshot(await commands.appGetSnapshot());
  },

  async listAudioInputs(): Promise<AudioInput[]> {
    if (!hasTauriRuntime()) {
      return [
        { channels: 1, id: "default", isDefault: true, name: "MacBookのマイク" },
        { channels: 2, id: "external", isDefault: false, name: "外部マイク" },
      ];
    }

    return commands.audioListInputs();
  },

  async listHistory(query: HistoryQuery = {}): Promise<HistoryPage> {
    if (!hasTauriRuntime()) {
      return { items: structuredClone(mockSnapshot.sessions) };
    }

    const result = await commands.historyQuery(historyInput(query));

    return { items: result.items.map(mapSummary), nextCursor: result.nextCursor ?? undefined };
  },

  async listModels(): Promise<ModelList> {
    if (!hasTauriRuntime()) {
      return { models: [], state: structuredClone(mockSnapshot.model) };
    }

    const result = await commands.modelList();

    return { models: result.models, state: mapModel(result.state) };
  },

  async onApplicationEvent(handler: (event: ApplicationEvent) => void): Promise<() => void> {
    if (!hasTauriRuntime()) {
      return () => undefined;
    }

    return events.appEvent.listen(({ payload }) => handler(mapEvent(payload)));
  },

  async pauseQueue(): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      mockQueueSnapshot.autoAdvanceEnabled = false;

      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queuePause());
  },

  async pauseSession(sessionId: string): Promise<void> {
    if (!hasTauriRuntime()) {
      return;
    }

    const session = await currentSession(sessionId);

    await commands.sessionPause({ expectedRowVersion: session.rowVersion, sessionId });
  },

  async removeQueueItem(itemId: string, revision: string): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queueRemove({ expectedRevision: revision, itemId }));
  },

  async renameSession(
    sessionId: string,
    title: string,
  ): Promise<{ rowVersion: string; sessionId: string; title: string }> {
    if (!hasTauriRuntime()) {
      return { rowVersion: "1", sessionId, title: title.trim() };
    }

    const session = await currentSession(sessionId);
    const renamed = await commands.historyRename({
      expectedRowVersion: session.rowVersion,
      sessionId,
      title,
    });

    return { rowVersion: renamed.rowVersion, sessionId: renamed.id, title: renamed.title };
  },

  async renderSessions(sessionIds: string[], format: ExportFormat): Promise<string> {
    if (!hasTauriRuntime()) {
      return "";
    }

    return commands.historyRender({ format, sessionIds });
  },

  async reorderQueue(revision: string, itemIds: string[]): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queueReorder({ expectedRevision: revision, itemIds }));
  },

  async resolveClose(resolution: "cancel" | "stopAndQuit" | "forceQuit"): Promise<void> {
    if (hasTauriRuntime()) {
      await commands.hostResolveClose(resolution);
    }
  },

  async resumeSession(sessionId: string): Promise<void> {
    if (!hasTauriRuntime()) {
      return;
    }

    const session = await currentSession(sessionId);

    await commands.sessionResume({ expectedRowVersion: session.rowVersion, sessionId });
  },

  async searchHistory(query: HistoryQuery): Promise<HistoryPage> {
    return this.listHistory(query);
  },

  async selectModel(model: ModelReference): Promise<ModelState> {
    if (!hasTauriRuntime()) {
      return { selected: model, status: "ready" };
    }

    return mapModel(await commands.modelSelect(model));
  },

  async startQueue(language: string | null, revision: string): Promise<QueueSnapshot> {
    if (!hasTauriRuntime()) {
      return structuredClone(mockQueueSnapshot);
    }

    return mapQueue(await commands.queueStart({ expectedRevision: revision, language }));
  },

  async startSession(input: StartSessionInput): Promise<SessionDetail> {
    if (input.inputKind === "file") {
      throw new Error("Files must be added through the queue");
    }

    if (!hasTauriRuntime()) {
      throw new Error("Live recording requires the Tauri runtime");
    }

    let source: LiveSource = { type: "systemAudio" };

    if (input.inputKind === "microphone") {
      source = { deviceId: input.deviceId ?? null, type: "microphone" };
    }

    return mapDetail(
      await commands.sessionStart({
        language: input.language,
        source,
        title: input.inputName ?? null,
      }),
    );
  },

  async stopSession(sessionId: string): Promise<void> {
    if (!hasTauriRuntime()) {
      return;
    }

    const session = await currentSession(sessionId);

    await commands.sessionStop({ expectedRowVersion: session.rowVersion, sessionId });
  },
};

function incrementMockRevision(value: string): string {
  return (BigInt(value) + 1n).toString();
}

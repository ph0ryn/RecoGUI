export type SessionStatus =
  | "preparing"
  | "running"
  | "pausing"
  | "paused"
  | "stopping"
  | "completed"
  | "stopped"
  | "failed"
  | "abandoned";

export type InputKind = "file" | "microphone" | "systemAudio";

export interface TranscriptSegment {
  id: string;
  sequence: number;
  startMs: number;
  endMs: number;
  language: string;
  text: string;
}

export interface SessionSummary {
  id: string;
  title: string;
  status: SessionStatus;
  startedAt: string;
  endedAt?: string;
  durationMs: number;
  inputKind: InputKind;
  inputName: string;
  language: string;
  model: string;
  rowVersion: string;
  segmentCount: number;
  characterCount: number;
  snippet?: string;
  errorCode?: string;
  errorMessage?: string;
}

export interface SessionDetail extends SessionSummary {
  segments: TranscriptSegment[];
  segmentsLoaded: true;
}

export type ExportFormat = "txt" | "timestampedTxt" | "markdown" | "json" | "srt" | "vtt";

export interface ModelReference {
  repoId: string;
  revision: string;
}

export interface CachedModelRevision extends ModelReference {
  lastModified: string;
  refs: string[];
  size: string;
  supportedLanguages: string[];
}

export interface ModelState {
  status: "checking" | "unselected" | "unavailable" | "ready" | "error";
  selected: ModelReference | null;
  errorCode?: string;
  errorMessage?: string;
}

export interface ModelList {
  models: CachedModelRevision[];
  state: ModelState;
}

export interface AudioInput {
  channels: number;
  id: string;
  isDefault: boolean;
  name: string;
}

export interface HistoryPage {
  items: SessionSummary[];
  nextCursor?: string;
}

export interface ExportResult {
  canceled: boolean;
  completed?: boolean;
  failedSessionIds?: string[];
  operationId?: string;
}

export interface EngineSnapshot {
  sequence: string;
  sessions: (SessionSummary | SessionDetail)[];
  model: ModelState;
  queue: QueueSnapshot;
  activeSessionId?: string;
  nextCursor?: string;
}

export type QueueItemStatus = "pending" | "invalid";

export interface QueueItem {
  id: string;
  displayName: string;
  status: QueueItemStatus;
  addedAt: string;
  updatedAt: string;
  errorCode?: string;
  errorMessage?: string;
}

export interface QueueSnapshot {
  revision: string;
  autoAdvanceEnabled: boolean;
  items: QueueItem[];
}

export type ApplicationEvent =
  | { type: "session.upserted"; sequence: string; session: SessionSummary }
  | {
      type: "segment.committed";
      sequence: string;
      receipt: PersistedSegmentReceipt;
    }
  | {
      type: "session.progress";
      sequence: string;
      sessionId: string;
      processedAudioMs: number;
      totalAudioMs?: number;
    }
  | { type: "sessions.deleted"; sequence: string; sessionIds: string[] }
  | { type: "queue.changed"; sequence: string; queue: QueueSnapshot }
  | { type: "model.changed"; sequence: string; model: ModelState }
  | {
      type: "export.progress";
      sequence: string;
      operationId: string;
      completedItems: number;
      totalItems: number;
    }
  | {
      type: "export.finished";
      sequence: string;
      operationId: string;
      canceled: boolean;
      failedSessionIds: string[];
    }
  | {
      type: "close.confirmationRequired";
      sequence: string;
      sessionId?: string;
    }
  | {
      type: "close.forceRequired";
      sequence: string;
      sessionId?: string;
      error: string;
    }
  | { type: "notification.error"; sequence: string; message: string };

export interface PersistedSegmentReceipt {
  characters: number;
  mediaDurationMs: number;
  recognizedSegments: number;
  rowVersion: string;
  segment: TranscriptSegment;
  sessionId: string;
  totalSegments: number;
}

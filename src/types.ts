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
  rowVersion: number;
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
  status: "unselected" | "unavailable" | "ready" | "error";
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
  sessions: (SessionSummary | SessionDetail)[];
  model: ModelState;
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
  revision: number;
  autoAdvanceEnabled: boolean;
  items: QueueItem[];
}

export interface SelectedAudioFile {
  displayName: string;
  sourceToken: string;
}

export interface EngineEvent {
  event: string;
  payload: unknown;
  sequence: number;
  sessionId?: string;
}

export interface PersistedSegmentReceipt {
  characters: number;
  mediaDurationMs: number;
  recognizedSegments: number;
  rowVersion: number;
  segment: TranscriptSegment;
  sessionId: string;
  totalSegments: number;
}

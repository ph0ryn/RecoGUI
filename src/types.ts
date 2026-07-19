export type SessionStatus =
  | "preparing"
  | "running"
  | "stopping"
  | "completed"
  | "stopped"
  | "failed"
  | "abandoned";

export type InputKind = "microphone" | "file";

export interface TranscriptSegment {
  id: string;
  sequence: number;
  startMs: number;
  endMs: number;
  text: string;
}

export interface TranscriptionSession {
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
  segmentCount: number;
  characterCount: number;
  snippet?: string;
  errorCode?: string;
  errorMessage?: string;
  segments: TranscriptSegment[];
  segmentsLoaded?: boolean;
}

export type ExportFormat = "txt" | "markdown" | "json" | "srt" | "vtt" | "csv";

export interface ModelState {
  status: "missing" | "downloading" | "verifying" | "loading" | "ready" | "failed";
  progress?: number;
  detail?: string;
}

export interface AudioInput {
  channels: number;
  id: string;
  name: string;
}

export interface HistoryPage {
  items: TranscriptionSession[];
  nextCursor?: string;
}

export interface ExportResult {
  canceled: boolean;
  completed?: boolean;
  failedSessionIds?: string[];
  operationId?: string;
}

export interface EngineSnapshot {
  sessions: TranscriptionSession[];
  model: ModelState;
  activeSessionId?: string;
  nextCursor?: string;
}

export interface EngineEvent {
  event: string;
  payload: unknown;
  sessionId?: string;
}

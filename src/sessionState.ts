import { compareDecimalStrings } from "./eventSequence";

/* oxlint-disable no-ternary */
import type {
  PersistedSegmentReceipt,
  SessionDetail,
  SessionStatus,
  SessionSummary,
  TranscriptSegment,
} from "./types";

export interface SessionEntity extends SessionSummary {
  segments: TranscriptSegment[];
  segmentsLoaded: boolean;
}

export interface SessionState {
  activeSessionId?: string;
  orderedSessionIds: string[];
  sessionsById: Partial<Record<string, SessionEntity>>;
}

export type SessionAction =
  | {
      activeSessionId?: string;
      sessions: (SessionSummary | SessionDetail)[];
      type: "bootstrap";
    }
  | { append?: boolean; sessions: SessionSummary[]; type: "historyPageLoaded" }
  | { session: SessionDetail; type: "detailLoaded" }
  | { session: SessionDetail; type: "canonicalReconciled" }
  | { receipt: PersistedSegmentReceipt; type: "segmentPersisted" }
  | { rowVersion: string; sessionId: string; title: string; type: "sessionRenamed" }
  | { active?: boolean; session: SessionSummary | SessionDetail; type: "sessionStarted" }
  | {
      endedAt?: string;
      characterCount?: number;
      durationMs?: number;
      errorCode?: string;
      errorMessage?: string;
      rowVersion: string;
      segmentCount?: number;
      sessionId: string;
      status: SessionStatus;
      type: "statusChanged";
    }
  | { sessionIds: string[]; type: "sessionsDeleted" };

export const initialSessionState: SessionState = {
  orderedSessionIds: [],
  sessionsById: {},
};

function isDetail(session: SessionSummary | SessionDetail): session is SessionDetail {
  return "segments" in session;
}

function sortSegments(segments: Iterable<TranscriptSegment>): TranscriptSegment[] {
  return [...segments].sort((left, right) => left.sequence - right.sequence);
}

function mergeSegments(
  current: TranscriptSegment[],
  incoming: TranscriptSegment[],
): TranscriptSegment[] {
  const bySequence = new Map(current.map((segment) => [segment.sequence, segment]));

  for (const segment of incoming) {
    bySequence.set(segment.sequence, segment);
  }

  return sortSegments(bySequence.values());
}

function toEntity(session: SessionSummary | SessionDetail): SessionEntity {
  return {
    ...session,
    segments: isDetail(session) ? sortSegments(session.segments) : [],
    segmentsLoaded: isDetail(session),
  };
}

function mergeSummary(current: SessionEntity | undefined, incoming: SessionSummary): SessionEntity {
  if (!current) {
    return toEntity(incoming);
  }

  if (compareDecimalStrings(incoming.rowVersion, current.rowVersion) < 0) {
    return current;
  }

  return {
    ...current,
    ...incoming,
    segments: current.segments,
    segmentsLoaded: current.segmentsLoaded,
  };
}

function mergeDetail(current: SessionEntity | undefined, incoming: SessionDetail): SessionEntity {
  if (!current) {
    return toEntity(incoming);
  }

  if (compareDecimalStrings(incoming.rowVersion, current.rowVersion) < 0) {
    return {
      ...current,
      segments: mergeSegments(current.segments, incoming.segments),
      segmentsLoaded: true,
    };
  }

  return {
    ...current,
    ...incoming,
    segments:
      incoming.rowVersion === current.rowVersion
        ? mergeSegments(current.segments, incoming.segments)
        : sortSegments(incoming.segments),
    segmentsLoaded: true,
  };
}

function reconcileDetail(
  current: SessionEntity | undefined,
  incoming: SessionDetail,
): SessionEntity {
  if (current && compareDecimalStrings(incoming.rowVersion, current.rowVersion) < 0) {
    return current;
  }

  return toEntity(incoming);
}

function addMissingIds(order: string[], sessions: readonly SessionSummary[]): string[] {
  const ids = new Set(order);
  const result = [...order];

  for (const { id } of sessions) {
    if (!ids.has(id)) {
      ids.add(id);
      result.push(id);
    }
  }

  return result;
}

function releasesActiveSession(status: SessionStatus): boolean {
  return ["paused", "completed", "stopped", "failed", "abandoned"].includes(status);
}

function claimsActiveSession(status: SessionStatus): boolean {
  return ["preparing", "running", "pausing", "stopping"].includes(status);
}

export function sessionStateReducer(state: SessionState, action: SessionAction): SessionState {
  switch (action.type) {
    case "bootstrap": {
      const sessionsById = { ...state.sessionsById };

      for (const session of action.sessions) {
        const current = sessionsById[session.id];

        sessionsById[session.id] = isDetail(session)
          ? mergeDetail(current, session)
          : mergeSummary(current, session);
      }

      return {
        activeSessionId:
          action.activeSessionId ??
          action.sessions.find(({ status }) => claimsActiveSession(status))?.id,
        orderedSessionIds: addMissingIds(
          action.sessions.map(({ id }) => id),
          state.orderedSessionIds
            .map((id) => sessionsById[id])
            .filter((session): session is SessionEntity => session !== undefined),
        ),
        sessionsById,
      };
    }

    case "historyPageLoaded": {
      const sessionsById = { ...state.sessionsById };

      for (const session of action.sessions) {
        sessionsById[session.id] = mergeSummary(sessionsById[session.id], session);
      }

      return {
        ...state,
        orderedSessionIds: action.append
          ? addMissingIds(state.orderedSessionIds, action.sessions)
          : addMissingIds(
              action.sessions.map(({ id }) => id),
              state.orderedSessionIds
                .map((id) => sessionsById[id])
                .filter((session): session is SessionEntity => session !== undefined),
            ),
        sessionsById,
      };
    }

    case "detailLoaded":

    case "canonicalReconciled": {
      const current = state.sessionsById[action.session.id];
      const session =
        action.type === "canonicalReconciled"
          ? reconcileDetail(current, action.session)
          : mergeDetail(current, action.session);

      if (session === current) {
        return state;
      }

      return {
        ...state,
        activeSessionId: claimsActiveSession(session.status) ? session.id : state.activeSessionId,
        orderedSessionIds: state.orderedSessionIds.includes(session.id)
          ? state.orderedSessionIds
          : [...state.orderedSessionIds, session.id],
        sessionsById: { ...state.sessionsById, [session.id]: session },
      };
    }

    case "segmentPersisted": {
      const { receipt } = action;
      const current = state.sessionsById[receipt.sessionId];

      if (!current) {
        return state;
      }

      if (compareDecimalStrings(receipt.rowVersion, current.rowVersion) < 0) {
        return {
          ...state,
          sessionsById: {
            ...state.sessionsById,
            [receipt.sessionId]: {
              ...current,
              segments: mergeSegments(current.segments, [receipt.segment]),
            },
          },
        };
      }

      const session: SessionEntity = {
        ...current,
        characterCount: receipt.characters,
        durationMs: receipt.mediaDurationMs,
        rowVersion: receipt.rowVersion,
        segmentCount: receipt.totalSegments,
        segments: mergeSegments(current.segments, [receipt.segment]),
      };

      return {
        ...state,
        sessionsById: { ...state.sessionsById, [receipt.sessionId]: session },
      };
    }

    case "sessionRenamed": {
      const current = state.sessionsById[action.sessionId];

      if (!current || compareDecimalStrings(action.rowVersion, current.rowVersion) < 0) {
        return state;
      }

      return {
        ...state,
        sessionsById: {
          ...state.sessionsById,
          [action.sessionId]: {
            ...current,
            rowVersion: action.rowVersion,
            title: action.title,
          },
        },
      };
    }

    case "sessionStarted": {
      const current = state.sessionsById[action.session.id];
      const session = isDetail(action.session)
        ? mergeDetail(current, action.session)
        : mergeSummary(current, action.session);

      return {
        ...state,
        activeSessionId: action.active === false ? state.activeSessionId : session.id,
        orderedSessionIds: [
          session.id,
          ...state.orderedSessionIds.filter((id) => id !== session.id),
        ],
        sessionsById: { ...state.sessionsById, [session.id]: session },
      };
    }

    case "statusChanged": {
      const current = state.sessionsById[action.sessionId];

      if (!current || compareDecimalStrings(action.rowVersion, current.rowVersion) < 0) {
        return state;
      }

      const session: SessionEntity = {
        ...current,
        characterCount: action.characterCount ?? current.characterCount,
        durationMs: action.durationMs ?? current.durationMs,
        endedAt: action.endedAt ?? current.endedAt,
        errorCode: action.errorCode,
        errorMessage: action.errorMessage,
        rowVersion: action.rowVersion,
        segmentCount: action.segmentCount ?? current.segmentCount,
        status: action.status,
      };
      let activeSessionId = state.activeSessionId;

      if (state.activeSessionId === action.sessionId && releasesActiveSession(action.status)) {
        activeSessionId = undefined;
      } else if (claimsActiveSession(action.status)) {
        activeSessionId = action.sessionId;
      }

      return {
        ...state,
        activeSessionId,
        sessionsById: { ...state.sessionsById, [action.sessionId]: session },
      };
    }

    case "sessionsDeleted": {
      const deleted = new Set(action.sessionIds);
      const sessionsById = Object.fromEntries(
        Object.entries(state.sessionsById).filter(([id]) => !deleted.has(id)),
      );

      return {
        activeSessionId:
          state.activeSessionId && deleted.has(state.activeSessionId)
            ? undefined
            : state.activeSessionId,
        orderedSessionIds: state.orderedSessionIds.filter((id) => !deleted.has(id)),
        sessionsById,
      };
    }
  }
}

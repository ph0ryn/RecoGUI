import { getCurrentWindow } from "@tauri-apps/api/window";
/* oxlint-disable no-ternary, no-nested-ternary, curly, init-declarations, arrow-body-style, @stylistic/padding-line-between-statements */
import {
  type CSSProperties,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from "react";

import "./App.css";
import { loadAppPreferences, saveAppPreferences } from "./appPreferences";
import { recoBridge } from "./bridge";
import { initialSessionState, sessionStateReducer, type SessionEntity } from "./sessionState";
import { useEngineEvents } from "./useEngineEvents";

import type {
  AudioInput,
  CachedModelRevision,
  EngineEvent,
  ExportFormat,
  InputKind,
  ModelState,
  PersistedSegmentReceipt,
  QueueSnapshot,
  SessionStatus,
  QueueItem,
} from "./types";

interface ExportOperation {
  failedSessionIds: string[];
  format: ExportFormat;
  operationId?: string;
  progress: number;
  sessionIds: string[];
  state: "running" | "canceling" | "completed" | "failed" | "canceled";
}

const statusLabels: Record<SessionStatus, string> = {
  abandoned: "異常終了",
  completed: "完了",
  failed: "失敗",
  paused: "一時停止",
  pausing: "一時停止処理中",
  preparing: "準備中",
  running: "処理中",
  stopped: "中断",
  stopping: "停止処理中",
};

const exportLabels: Record<ExportFormat, string> = {
  csv: "CSV（セグメント一覧）",
  json: "JSON（構造化データ）",
  markdown: "Markdown",
  srt: "SRT字幕",
  txt: "テキスト",
  vtt: "WebVTT字幕",
};

const exportFormats = Object.keys(exportLabels) as ExportFormat[];
const terminalStatuses: SessionStatus[] = ["completed", "stopped", "failed", "abandoned"];
const deletableStatuses: SessionStatus[] = [...terminalStatuses, "paused"];
const pausableStatuses: SessionStatus[] = ["preparing", "running", "pausing"];
const emptyQueue: QueueSnapshot = { autoAdvanceEnabled: false, items: [], revision: 0 };

function formatDuration(milliseconds: number): string {
  const totalSeconds = Math.max(0, Math.round(milliseconds / 1_000));
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;

  return hours > 0
    ? `${hours}:${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`
    : `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function formatTimestamp(milliseconds: number): string {
  const minutes = Math.floor(milliseconds / 60_000);
  const seconds = Math.floor((milliseconds % 60_000) / 1_000);

  return `${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}

function formatDate(isoDate: string): string {
  return new Intl.DateTimeFormat("ja-JP", {
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    month: "short",
  }).format(new Date(isoDate));
}

function handleWindowDrag(event: ReactMouseEvent<HTMLElement>): void {
  if (event.button !== 0) return;

  const target = event.target as HTMLElement;

  if (
    target.closest(
      "button, input, select, textarea, a, summary, label, [role='button'], [contenteditable='true']",
    )
  )
    return;

  void getCurrentWindow().startDragging();
}

function dayGroup(isoDate: string): string {
  const value = new Date(isoDate);
  const today = new Date();
  const yesterday = new Date(today);

  yesterday.setDate(today.getDate() - 1);
  if (value.toDateString() === today.toDateString()) return "今日";
  if (value.toDateString() === yesterday.toDateString()) return "昨日";

  return new Intl.DateTimeFormat("ja-JP", { day: "numeric", month: "long" }).format(value);
}

function matchesSession(session: SessionEntity, query: string): boolean {
  const normalized = query.trim().toLocaleLowerCase("ja-JP");

  if (!normalized) return true;

  return [
    session.title,
    session.inputName,
    session.snippet ?? "",
    ...session.segments.map(({ text }) => text),
  ].some((value) => value.toLocaleLowerCase("ja-JP").includes(normalized));
}

function getSnippet(session: SessionEntity, query: string): string | undefined {
  if (!query.trim()) return session.snippet;
  const segment = session.segments.find(({ text }) =>
    text.toLocaleLowerCase("ja-JP").includes(query.trim().toLocaleLowerCase("ja-JP")),
  );

  return segment?.text;
}

function modelStatusText(model: ModelState): string {
  const labels: Record<ModelState["status"], string> = {
    cliMissing: "hf CLIが見つかりません。",
    error: "選択したモデルを読み込めませんでした。",
    loading: "モデルを読み込んでいます。",
    ready: "利用可能",
    unavailable: "選択したモデルがHFキャッシュにありません。",
    unselected: "音声認識モデルが選択されていません。",
  };

  return labels[model.status];
}

function App() {
  const [initialPreferences] = useState(loadAppPreferences);
  const [sessionState, dispatchSessions] = useReducer(sessionStateReducer, initialSessionState);
  const [model, setModel] = useState<ModelState>({ selected: null, status: "loading" });
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [anchorId, setAnchorId] = useState<string>();
  const [query, setQuery] = useState("");
  const [detailQuery, setDetailQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<"all" | SessionStatus>("all");
  const [inputFilter, setInputFilter] = useState<"all" | InputKind>("all");
  const [sortOrder, setSortOrder] = useState<"newest" | "oldest" | "longest">("newest");
  const [isLoading, setIsLoading] = useState(true);
  const [isHistoryLoading, setIsHistoryLoading] = useState(false);
  const [nextCursor, setNextCursor] = useState<string>();
  const [fatalError, setFatalError] = useState<string>();
  const [dialog, setDialog] = useState<
    "new" | "rename" | "delete" | "export" | "settings" | "close" | "forceClose" | null
  >(null);
  const [closeRequest, setCloseRequest] = useState<{ error?: string; sessionId?: string }>();
  const [exportFormat, setExportFormat] = useState<ExportFormat>("txt");
  const [isWorking, setIsWorking] = useState(false);
  const [toast, setToast] = useState<string>();
  const [paneWidth, setPaneWidth] = useState(320);
  const [autoFollow, setAutoFollow] = useState(true);
  const [selectedDeviceId, setSelectedDeviceId] = useState(initialPreferences.defaultInputDeviceId);
  const [audioInputs, setAudioInputs] = useState<AudioInput[]>([]);
  const [exportOperation, setExportOperation] = useState<ExportOperation>();
  const [sessionProgress, setSessionProgress] = useState<Record<string, number>>({});
  const [queue, setQueue] = useState<QueueSnapshot>(emptyQueue);
  const [isQueueWorking, setIsQueueWorking] = useState(false);
  const [isFilterMenuOpen, setIsFilterMenuOpen] = useState(false);
  const [contextMenu, setContextMenu] = useState<{
    session: SessionEntity;
    x: number;
    y: number;
  }>();
  const [renameTarget, setRenameTarget] = useState<SessionEntity>();
  const anchorIndex = useRef<number | undefined>(undefined);
  const transcriptRef = useRef<HTMLDivElement>(null);
  const newButtonRef = useRef<HTMLButtonElement>(null);
  const dialogInvokerRef = useRef<HTMLElement | null>(null);
  const filterMenuRef = useRef<HTMLDivElement>(null);
  const sessions = sessionState.orderedSessionIds
    .map((id) => sessionState.sessionsById[id])
    .filter((session): session is SessionEntity => session !== undefined);
  const { activeSessionId } = sessionState;

  useEffect(() => {
    saveAppPreferences({
      defaultInputDeviceId: selectedDeviceId,
    });
  }, [selectedDeviceId]);

  useEffect(() => {
    void recoBridge.listAudioInputs().then((availableInputs) => {
      setAudioInputs(availableInputs);
      if (selectedDeviceId && !availableInputs.some(({ id }) => id === selectedDeviceId)) {
        setSelectedDeviceId("");
      }
    });
  }, [dialog, selectedDeviceId]);

  const orderedSessions = useMemo(() => {
    return [...sessions]
      .filter((session) => statusFilter === "all" || session.status === statusFilter)
      .filter((session) => inputFilter === "all" || session.inputKind === inputFilter)
      .filter((session) => matchesSession(session, query))
      .sort((left, right) => {
        if (sortOrder === "longest") return right.durationMs - left.durationMs;
        const delta = Date.parse(right.startedAt) - Date.parse(left.startedAt);

        return sortOrder === "newest" ? delta : -delta;
      });
  }, [inputFilter, query, sessions, sortOrder, statusFilter]);

  const activeSession = sessions.find(({ id }) => id === activeSessionId);
  const selectedSessions = sessions.filter(({ id }) => selectedIds.has(id));
  const selectedSession = selectedSessions.length === 1 ? selectedSessions[0] : undefined;
  const displayGroups = useMemo(() => {
    const groups = new Map<string, SessionEntity[]>();

    for (const session of orderedSessions.filter(({ id }) => id !== activeSessionId)) {
      const group = dayGroup(session.startedAt);

      groups.set(group, [...(groups.get(group) ?? []), session]);
    }

    return [...groups.entries()];
  }, [activeSessionId, orderedSessions]);
  const selectableSessions = useMemo(
    () => [
      ...(activeSession && orderedSessions.some(({ id }) => id === activeSession.id)
        ? [activeSession]
        : []),
      ...displayGroups.flatMap(([, groupSessions]) => groupSessions),
    ],
    [activeSession, displayGroups, orderedSessions],
  );

  useEffect(() => {
    let alive = true;

    void Promise.all([recoBridge.getSnapshot(), recoBridge.getQueueState()])
      .then(([snapshot, queueSnapshot]) => {
        if (!alive) return;
        setQueue(queueSnapshot);
        dispatchSessions({
          activeSessionId: snapshot.activeSessionId,
          sessions: snapshot.sessions,
          type: "bootstrap",
        });
        setModel(snapshot.model);
        setNextCursor(snapshot.nextCursor);
        const savedId = localStorage.getItem("reco.lastSessionId");
        const initialId = snapshot.sessions.some(({ id }) => id === savedId)
          ? savedId
          : (snapshot.activeSessionId ?? snapshot.sessions[0]?.id);

        if (initialId) setSelectedIds(new Set([initialId]));
      })
      .catch(() => setFatalError("エンジンに接続できませんでした。アプリを再起動してください。"))
      .finally(() => setIsLoading(false));

    return () => {
      alive = false;
    };
  }, []);

  useEngineEvents({
    onCloseForceRequired: ({ error, sessionId }) => {
      setCloseRequest({ error, sessionId: sessionId ?? undefined });
      setDialog("forceClose");
    },
    onCloseRequested: ({ sessionId }) => {
      setCloseRequest({ sessionId });
      setDialog("close");
    },
    onEngineEvent: handleEngineEvent,
    onSubscriptionError: () => {
      setFatalError("エンジンのイベント購読に失敗しました。アプリを再起動してください。");
    },
  });
  const selectedAudioInput = selectedDeviceId
    ? audioInputs.find(({ id }) => id === selectedDeviceId)
    : audioInputs.find(({ isDefault }) => isDefault);
  const selectedAudioInputLabel = selectedAudioInput
    ? `${selectedAudioInput.name}${selectedAudioInput.isDefault ? " (システム既定)" : ""}`
    : "入力デバイスなし";

  useEffect(() => {
    if (isLoading) return;
    let alive = true;
    const timeout = window.setTimeout(() => {
      setIsHistoryLoading(true);
      const historyRequest = query.trim()
        ? recoBridge.searchHistory({
            inputKind: inputFilter === "all" ? undefined : inputFilter,
            limit: 50,
            query,
            status: statusFilter === "all" ? undefined : statusFilter,
          })
        : recoBridge.listHistory({
            inputKind: inputFilter === "all" ? undefined : inputFilter,
            limit: 50,
            status: statusFilter === "all" ? undefined : statusFilter,
          });

      void historyRequest
        .then((page) => {
          if (!alive) return;
          dispatchSessions({ sessions: page.items, type: "historyPageLoaded" });
          setNextCursor(page.nextCursor);
        })
        .catch(() => setToast("履歴を読み込めませんでした。"))
        .finally(() => {
          if (alive) setIsHistoryLoading(false);
        });
    }, 300);

    return () => {
      alive = false;
      window.clearTimeout(timeout);
    };
  }, [activeSessionId, inputFilter, isLoading, query, statusFilter]);

  useEffect(() => {
    if (selectedIds.size === 1) {
      const id = [...selectedIds][0];

      localStorage.setItem("reco.lastSessionId", id);
    }
  }, [selectedIds]);

  useEffect(() => {
    if (!selectedSession || selectedSession.segmentsLoaded) return;
    let alive = true;

    void recoBridge
      .getSession(selectedSession.id)
      .then((detail) => {
        if (!alive) return;
        dispatchSessions({ session: detail, type: "detailLoaded" });
      })
      .catch(() => setToast("文字起こし本文を読み込めませんでした。"));

    return () => {
      alive = false;
    };
  }, [selectedSession?.id, selectedSession?.segmentsLoaded]);

  useEffect(() => {
    if (!autoFollow || selectedSession?.id !== activeSessionId) return;
    transcriptRef.current?.scrollTo({
      behavior: "auto",
      top: transcriptRef.current.scrollHeight,
    });
  }, [activeSessionId, autoFollow, selectedSession?.id, selectedSession?.segments.length]);

  useEffect(() => {
    if (!toast) return;
    const timeout = window.setTimeout(() => setToast(undefined), 4_000);

    return () => window.clearTimeout(timeout);
  }, [toast]);

  useEffect(() => {
    function selectVisibleTranscript(event: globalThis.KeyboardEvent): void {
      if (
        !event.metaKey ||
        event.key.toLocaleLowerCase() !== "a" ||
        selectedSessions.length !== 1
      ) {
        return;
      }
      const target = event.target;

      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        (target instanceof HTMLElement && target.isContentEditable)
      ) {
        return;
      }
      const transcript = transcriptRef.current?.querySelector<HTMLElement>(".transcript");

      if (!transcript) return;
      event.preventDefault();
      const range = document.createRange();

      range.selectNodeContents(transcript);
      const selection = window.getSelection();

      selection?.removeAllRanges();
      selection?.addRange(range);
    }

    window.addEventListener("keydown", selectVisibleTranscript);

    return () => window.removeEventListener("keydown", selectVisibleTranscript);
  }, [selectedSessions.length]);

  useEffect(() => {
    function handleShortcut(event: globalThis.KeyboardEvent): void {
      if (
        !event.metaKey ||
        event.altKey ||
        event.ctrlKey ||
        event.repeat ||
        event.isComposing ||
        dialog
      )
        return;

      const key = event.key.toLocaleLowerCase();

      if (key === "f") {
        event.preventDefault();
        const input = document.querySelector<HTMLInputElement>(
          event.shiftKey ? ".history-search input" : ".detail-search input",
        );

        input?.focus();
        input?.select();
        return;
      }
      if (key === "backspace" && !event.shiftKey) {
        const target = event.target;
        const isEditable =
          target instanceof HTMLInputElement ||
          target instanceof HTMLTextAreaElement ||
          (target instanceof HTMLElement && target.isContentEditable);

        if (isEditable) return;
        event.preventDefault();
        if (
          !isWorking &&
          selectedSessions.length > 0 &&
          selectedSessions.every(({ status }) => deletableStatuses.includes(status))
        )
          openDialog("delete");
        return;
      }
      if (key === "s" && !event.shiftKey) {
        event.preventDefault();
        if (!isWorking && selectedSessions.length > 0) openDialog("export");
        return;
      }
      if (key === "n") {
        event.preventDefault();
        if (model.status !== "ready") return;
        if (event.shiftKey) {
          if (!isWorking && !isQueueWorking) void startSession("file");
        } else if (!isWorking && activeSessionId === undefined && !queue.autoAdvanceEnabled) {
          void startSession("microphone");
        }
        return;
      }
      if (key === "," && !event.shiftKey) {
        event.preventDefault();
        openDialog("settings");
      }
    }

    window.addEventListener("keydown", handleShortcut);

    return () => window.removeEventListener("keydown", handleShortcut);
  }, [
    activeSessionId,
    dialog,
    isQueueWorking,
    isWorking,
    model.status,
    queue.autoAdvanceEnabled,
    selectedSessions,
  ]);

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(undefined);

    window.addEventListener("click", close);
    window.addEventListener("blur", close);

    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("blur", close);
    };
  }, [contextMenu]);

  useEffect(() => {
    if (!isFilterMenuOpen) return;
    const closeOnPointerDown = (event: PointerEvent) => {
      if (!filterMenuRef.current?.contains(event.target as Node)) setIsFilterMenuOpen(false);
    };
    const closeOnEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") setIsFilterMenuOpen(false);
    };

    window.addEventListener("pointerdown", closeOnPointerDown);
    window.addEventListener("keydown", closeOnEscape);

    return () => {
      window.removeEventListener("pointerdown", closeOnPointerDown);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [isFilterMenuOpen]);

  function handleEngineEvent(message: EngineEvent): void {
    if (message.event === "queue.changed") {
      setQueue(message.payload as QueueSnapshot);
    }
    if (message.event === "engine.snapshot") {
      const snapshot = message.payload as {
        activeSessionId?: string;
        model: ModelState;
        sessions: SessionEntity[];
      };

      dispatchSessions({
        activeSessionId: snapshot.activeSessionId,
        sessions: snapshot.sessions,
        type: "bootstrap",
      });
      setModel(snapshot.model);
    }
    if (message.event === "segment.persisted") {
      const payload = message.payload as Omit<PersistedSegmentReceipt, "sessionId"> & {
        sessionId?: string;
      };
      const sessionId = payload.sessionId ?? message.sessionId;

      if (!sessionId) {
        return;
      }

      if (!sessionState.sessionsById[sessionId]) {
        void recoBridge
          .getSession(sessionId)
          .then((detail) => dispatchSessions({ session: detail, type: "detailLoaded" }))
          .catch(() => undefined);
      }

      dispatchSessions({ receipt: { ...payload, sessionId }, type: "segmentPersisted" });
    }
    if (message.event === "session.progress") {
      const payload = message.payload as { processedAudioMs?: number; totalAudioMs?: number };

      if (message.sessionId && payload.processedAudioMs !== undefined && payload.totalAudioMs) {
        const sessionId = message.sessionId;
        const progress = Math.min(1, Math.max(0, payload.processedAudioMs / payload.totalAudioMs));

        setSessionProgress((current) => ({ ...current, [sessionId]: progress }));
      }
    }
    if (message.event === "session.stateChanged") {
      const payload = message.payload as {
        characters?: number;
        endedAt?: string;
        mediaDurationMs?: number;
        rowVersion?: number;
        state?: SessionStatus;
        totalSegments?: number;
      };

      if (message.sessionId && payload.state && payload.rowVersion !== undefined) {
        if (!sessionState.sessionsById[message.sessionId]) {
          void recoBridge
            .getSession(message.sessionId)
            .then((detail) => dispatchSessions({ session: detail, type: "detailLoaded" }))
            .catch(() => undefined);
        }
        dispatchSessions({
          characterCount: payload.characters,
          durationMs: payload.mediaDurationMs,
          endedAt: payload.endedAt,
          rowVersion: payload.rowVersion,
          segmentCount: payload.totalSegments,
          sessionId: message.sessionId,
          status: payload.state,
          type: "statusChanged",
        });
      }
    }
    if (message.event === "session.completed") {
      const payload = message.payload as {
        characters?: number;
        endedAt?: string;
        mediaDurationMs?: number;
        rowVersion?: number;
        state?: SessionStatus;
        totalSegments?: number;
      };

      if (message.sessionId && payload.rowVersion !== undefined) {
        dispatchSessions({
          characterCount: payload.characters,
          durationMs: payload.mediaDurationMs,
          endedAt: payload.endedAt,
          rowVersion: payload.rowVersion,
          segmentCount: payload.totalSegments,
          sessionId: message.sessionId,
          status: payload.state ?? "completed",
          type: "statusChanged",
        });
      }
    }
    if (message.event === "session.failed") {
      const payload = message.payload as {
        characters?: number;
        code?: string;
        endedAt?: string;
        mediaDurationMs?: number;
        message?: string;
        rowVersion?: number;
        totalSegments?: number;
      };

      if (message.sessionId && payload.rowVersion !== undefined) {
        dispatchSessions({
          characterCount: payload.characters,
          durationMs: payload.mediaDurationMs,
          endedAt: payload.endedAt,
          errorCode: payload.code,
          errorMessage: payload.message,
          rowVersion: payload.rowVersion,
          segmentCount: payload.totalSegments,
          sessionId: message.sessionId,
          status: "failed",
          type: "statusChanged",
        });
      }
    }
    if (message.event === "history.changed" && message.sessionId) {
      void recoBridge
        .getSession(message.sessionId)
        .then((detail) => dispatchSessions({ session: detail, type: "canonicalReconciled" }))
        .catch(() => undefined);
    }
    if (message.event === "export.progress") {
      const payload = message.payload as {
        completedItems?: number;
        completed?: number;
        operationId?: string;
        progress?: number;
        totalItems?: number;
        total?: number;
      };
      const progress =
        payload.progress ??
        ((payload.completedItems ?? payload.completed) !== undefined &&
        (payload.totalItems ?? payload.total)
          ? (payload.completedItems ?? payload.completed ?? 0) /
            (payload.totalItems ?? payload.total ?? 1)
          : undefined);

      setExportOperation((current) =>
        current
          ? {
              ...current,
              operationId: payload.operationId ?? current.operationId,
              progress: progress ?? current.progress,
              state: "running",
            }
          : current,
      );
    }
    if (message.event === "export.completed") {
      const payload = message.payload as {
        failures?: { sessionId: string }[];
        failedSessionIds?: string[];
        operationId?: string;
        status?: string;
      };
      const failedSessionIds =
        payload.failedSessionIds ?? payload.failures?.map(({ sessionId }) => sessionId) ?? [];

      setExportOperation((current) =>
        current
          ? {
              ...current,
              failedSessionIds,
              operationId: payload.operationId ?? current.operationId,
              progress: 1,
              state:
                payload.status === "cancelled"
                  ? "canceled"
                  : failedSessionIds.length
                    ? "failed"
                    : "completed",
            }
          : current,
      );
    }
    if (message.event === "operation.failed") {
      const payload = message.payload as { message?: string; operation?: string };

      if (payload.operation?.startsWith("history.export")) {
        setExportOperation((current) => (current ? { ...current, state: "failed" } : current));
      }

      setToast(payload.message ?? "操作を完了できませんでした。");
    }
  }

  function selectSession(session: SessionEntity, additive: boolean, range: boolean): void {
    const index = selectableSessions.findIndex(({ id }) => id === session.id);

    setDetailQuery("");
    if (range && anchorIndex.current !== undefined) {
      const start = Math.min(anchorIndex.current, index);
      const end = Math.max(anchorIndex.current, index);
      const ids = selectableSessions.slice(start, end + 1).map(({ id }) => id);

      setSelectedIds(new Set(ids));
    } else if (additive) {
      setSelectedIds((current) => {
        const next = new Set(current);

        if (next.has(session.id)) next.delete(session.id);
        else next.add(session.id);

        return next;
      });
      anchorIndex.current = index;
    } else {
      setSelectedIds(new Set([session.id]));
      anchorIndex.current = index;
    }
    setAnchorId(session.id);
    setAutoFollow(true);
  }

  function handleHistoryKeyDown(event: KeyboardEvent<HTMLDivElement>): void {
    if (!selectableSessions.length || !["ArrowDown", "ArrowUp"].includes(event.key)) return;
    event.preventDefault();
    const currentId = selectedIds.size === 1 ? [...selectedIds][0] : anchorId;
    const currentIndex = Math.max(
      0,
      selectableSessions.findIndex(({ id }) => id === currentId),
    );
    const offset = event.key === "ArrowDown" ? 1 : -1;
    const nextIndex = Math.min(selectableSessions.length - 1, Math.max(0, currentIndex + offset));
    const next = selectableSessions[nextIndex];

    selectSession(next, event.metaKey, event.shiftKey);
    document.querySelector<HTMLButtonElement>(`[data-session-id="${next.id}"]`)?.focus();
  }

  function resizePane(event: ReactPointerEvent<HTMLDivElement>): void {
    event.currentTarget.setPointerCapture(event.pointerId);
    const startX = event.clientX;
    const startWidth = paneWidth;

    function move(pointerEvent: PointerEvent): void {
      setPaneWidth(Math.max(280, Math.min(420, startWidth + pointerEvent.clientX - startX)));
    }

    function stop(): void {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    }

    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  }

  function resizePaneWithKeyboard(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
      return;
    }
    event.preventDefault();
    setPaneWidth((current) =>
      Math.max(280, Math.min(420, current + (event.key === "ArrowRight" ? 10 : -10))),
    );
  }

  async function loadMoreHistory(): Promise<void> {
    if (!nextCursor || isHistoryLoading) return;
    setIsHistoryLoading(true);
    try {
      const historyQuery = {
        cursor: nextCursor,
        inputKind: inputFilter === "all" ? undefined : inputFilter,
        limit: 50,
        status: statusFilter === "all" ? undefined : statusFilter,
      };
      const page = query.trim()
        ? await recoBridge.searchHistory({ ...historyQuery, query })
        : await recoBridge.listHistory(historyQuery);

      dispatchSessions({ append: true, sessions: page.items, type: "historyPageLoaded" });
      setNextCursor(page.nextCursor);
    } catch {
      setToast("続きの履歴を読み込めませんでした。");
    } finally {
      setIsHistoryLoading(false);
    }
  }

  async function startSession(inputKind: InputKind): Promise<void> {
    setIsWorking(true);
    try {
      if (inputKind === "file") {
        const files = await recoBridge.pickAudioFiles();

        if (!files?.length) return;
        setQueue(await recoBridge.enqueueFiles(files));
        setDialog(null);
        setToast(`${files.length}件の音声ファイルを受け付けました。`);
        return;
      }
      const input: {
        deviceId?: string;
        inputKind: InputKind;
        inputName?: string;
        inputToken?: string;
      } = {
        deviceId: selectedDeviceId || undefined,
        inputKind,
      };

      const { rowVersion, sessionId } = await recoBridge.startSession(input);
      const startedAt = new Date().toISOString();
      const optimisticSession: SessionEntity = {
        characterCount: 0,
        durationMs: 0,
        id: sessionId,
        inputKind,
        inputName: input.inputName ?? "システムの既定マイク",
        language: "日本語",
        model: "Qwen3-ASR 1.7B JA",
        rowVersion,
        segmentCount: 0,
        segments: [],
        segmentsLoaded: true,
        startedAt,
        status: "preparing",
        title:
          input.inputName ??
          `録音 ${new Intl.DateTimeFormat("ja-JP", { dateStyle: "short", timeStyle: "short" }).format(new Date())}`,
      };

      setDialog(null);
      dispatchSessions({ session: optimisticSession, type: "sessionStarted" });
      setSelectedIds(new Set([sessionId]));
      setToast("録音を開始しました。");
    } catch {
      setToast("文字起こしを開始できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function pauseActive(sessionId: string): Promise<void> {
    const session = sessions.find(({ id }) => id === sessionId);

    setIsWorking(true);
    try {
      if (session?.inputKind === "file") {
        setQueue(await recoBridge.pauseQueue());
      }
      await recoBridge.pauseSession(sessionId);
      setToast("処理待ちを完了して一時停止します。");
    } catch {
      setToast("一時停止できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function resumePaused(sessionId: string): Promise<void> {
    setIsWorking(true);
    try {
      await recoBridge.resumeSession(sessionId);
      setToast("文字起こしを再開します。");
    } catch {
      setToast("再開できませんでした。別の文字起こしが処理中でないか確認してください。");
    } finally {
      setIsWorking(false);
    }
  }

  async function updateQueue(
    action: () => Promise<QueueSnapshot>,
    failureMessage: string,
  ): Promise<void> {
    setIsQueueWorking(true);
    try {
      setQueue(await action());
    } catch {
      try {
        setQueue(await recoBridge.getQueueState());
      } catch {
        // The event stream may still reconcile the canonical queue state.
      }
      setToast(failureMessage);
    } finally {
      setIsQueueWorking(false);
    }
  }

  function reorderQueueItem(itemId: string, offset: -1 | 1): void {
    const index = queue.items.findIndex(({ id }) => id === itemId);
    const nextIndex = index + offset;

    if (index < 0 || nextIndex < 0 || nextIndex >= queue.items.length) return;
    const itemIds = queue.items.map(({ id }) => id);

    [itemIds[index], itemIds[nextIndex]] = [itemIds[nextIndex], itemIds[index]];
    void updateQueue(
      () => recoBridge.reorderQueue(queue.revision, itemIds),
      "キューの順序を変更できませんでした。",
    );
  }

  async function deleteSelected(): Promise<void> {
    const ids = selectedSessions.map(({ id }) => id);

    setIsWorking(true);
    try {
      await recoBridge.deleteSessions(ids);
      const oldSessions = sessions;
      const firstIndex = oldSessions.findIndex(({ id }) => ids.includes(id));
      const remaining = oldSessions.filter(({ id }) => !ids.includes(id));
      const nextIndex = Math.max(0, Math.min(firstIndex, remaining.length - 1));

      dispatchSessions({ sessionIds: ids, type: "sessionsDeleted" });
      if (remaining.length) {
        setSelectedIds(new Set([remaining[nextIndex].id]));
      } else {
        setSelectedIds(new Set());
      }
      setDialog(null);
      setToast(`${ids.length}件を完全に削除しました。`);
    } catch {
      setToast("削除できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function runExport(sessionIds: string[], format: ExportFormat): Promise<void> {
    setIsWorking(true);
    setDialog(null);
    setExportOperation({ failedSessionIds: [], format, progress: 0, sessionIds, state: "running" });
    try {
      const result = await recoBridge.exportSessions(sessionIds, format);

      if (result.canceled) {
        setExportOperation(undefined);
        return;
      }
      setExportOperation((current) => ({
        failedSessionIds: result.failedSessionIds ?? [],
        format,
        operationId: result.operationId ?? current?.operationId,
        progress: result.completed ? 1 : (current?.progress ?? 0),
        sessionIds,
        state: result.failedSessionIds?.length
          ? "failed"
          : result.completed
            ? "completed"
            : "running",
      }));
    } catch {
      setExportOperation((current) => (current ? { ...current, state: "failed" } : current));
      setToast("書き出しを開始できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function exportSelected(): Promise<void> {
    await runExport(
      selectedSessions.map(({ id }) => id),
      exportFormat,
    );
  }

  async function copySelected(): Promise<void> {
    setIsWorking(true);
    try {
      await recoBridge.copySessions(
        selectedSessions.map(({ id }) => id),
        exportFormat,
      );
      setDialog(null);
      setToast("コピーしました。");
    } catch {
      setToast("コピーできませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function renameSession(title: string): Promise<void> {
    if (!renameTarget) return;
    setIsWorking(true);
    try {
      const result = await recoBridge.renameSession(renameTarget.id, title);

      dispatchSessions({ ...result, type: "sessionRenamed" });
      setDialog(null);
      setRenameTarget(undefined);
      setToast("名前を変更しました。");
    } catch {
      setToast("名前を変更できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  async function cancelExport(): Promise<void> {
    if (!exportOperation?.operationId) return;
    setExportOperation((current) => (current ? { ...current, state: "canceling" } : current));
    try {
      await recoBridge.cancelExport(exportOperation.operationId);
    } catch {
      setExportOperation((current) => (current ? { ...current, state: "running" } : current));
      setToast("Exportを中断できませんでした。");
    }
  }

  async function resolveClose(resolution: "cancel" | "stopAndQuit" | "forceQuit"): Promise<void> {
    setIsWorking(true);
    try {
      await recoBridge.resolveClose(resolution);
      setDialog(null);
      if (resolution === "cancel") setCloseRequest(undefined);
    } catch {
      setToast("終了操作を完了できませんでした。");
    } finally {
      setIsWorking(false);
    }
  }

  function openDialog(value: typeof dialog): void {
    dialogInvokerRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setDialog(value);
  }

  function openHistoryContextMenu(
    event: React.MouseEvent<HTMLButtonElement>,
    session: SessionEntity,
  ): void {
    event.preventDefault();
    selectSession(session, false, false);
    setContextMenu({
      session,
      x: Math.min(event.clientX, window.innerWidth - 188),
      y: Math.min(event.clientY, window.innerHeight - 140),
    });
  }

  function closeDialog(): void {
    setDialog(null);
    setRenameTarget(undefined);
    window.setTimeout(() => (dialogInvokerRef.current ?? newButtonRef.current)?.focus(), 0);
  }

  const appStyle = { "--history-width": `${paneWidth}px` } as CSSProperties;
  return (
    <main className="app-shell" style={appStyle}>
      <header className="topbar" data-tauri-drag-region onMouseDown={handleWindowDrag}>
        <div className="topbar-left" data-tauri-drag-region>
          <div className="brand-row">
            <span aria-hidden="true" className="brand-mark">
              <i />
              <i />
              <i />
            </span>
            <strong>RECO</strong>
          </div>
          <div className="machine-state">
            <i aria-hidden="true" />
            {fatalError ? "ENGINE OFFLINE" : "ENGINE READY"}
          </div>
        </div>
      </header>
      <aside aria-label="文字起こし履歴" className="history-pane">
        {queue.items.length > 0 && (
          <QueuePanel
            disabled={isQueueWorking}
            hasActiveSession={activeSessionId !== undefined}
            queue={queue}
            onClear={() =>
              void updateQueue(() => recoBridge.clearQueue(), "キューをクリアできませんでした。")
            }
            onMove={(itemId, offset) => reorderQueueItem(itemId, offset)}
            onPause={() =>
              void updateQueue(() => recoBridge.pauseQueue(), "キューを停止できませんでした。")
            }
            onRemove={(itemId) =>
              void updateQueue(
                () => recoBridge.removeQueueItem(itemId),
                "キューから削除できませんでした。",
              )
            }
            onStart={() =>
              void updateQueue(() => recoBridge.startQueue(), "キューを開始できませんでした。")
            }
          />
        )}
        <div className="history-toolbar">
          <div className="history-search-row">
            <label className="history-search">
              <span className="sr-only">履歴を検索</span>
              <span aria-hidden="true" className="history-search-icon">
                <svg viewBox="0 0 16 16">
                  <circle cx="6.75" cy="6.75" r="3.75" />
                  <path d="m9.5 9.5 3.5 3.5" />
                </svg>
              </span>
              <input
                onChange={(event) => setQuery(event.target.value)}
                placeholder="履歴を検索"
                type="search"
                value={query}
              />
              {query && (
                <button aria-label="検索をクリア" onClick={() => setQuery("")} type="button">
                  ×
                </button>
              )}
            </label>
            <div className="filter-menu-wrap" ref={filterMenuRef}>
              <button
                aria-expanded={isFilterMenuOpen}
                aria-label="履歴のフィルタと並び替え"
                className={`icon-button filter-button${
                  statusFilter !== "all" || inputFilter !== "all" || sortOrder !== "newest"
                    ? " active"
                    : ""
                }`}
                onClick={() => setIsFilterMenuOpen((open) => !open)}
                title="フィルタと並び替え"
                type="button"
              >
                <svg aria-hidden="true" viewBox="0 0 16 16">
                  <path d="M2 3h12L9.5 8v4l-3 1V8L2 3Z" />
                  <path d="M11 10h3M11 13h3" />
                </svg>
              </button>
              {isFilterMenuOpen && (
                <div aria-label="履歴のフィルタと並び替え" className="filter-popover" role="group">
                  <label>
                    <span>状態</span>
                    <select
                      aria-label="状態で絞り込み"
                      onChange={(event) =>
                        setStatusFilter(event.target.value as typeof statusFilter)
                      }
                      value={statusFilter}
                    >
                      <option value="all">すべての状態</option>
                      {Object.entries(statusLabels).map(([value, label]) => (
                        <option key={value} value={value}>
                          {label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label>
                    <span>入力</span>
                    <select
                      aria-label="入力元で絞り込み"
                      onChange={(event) => setInputFilter(event.target.value as typeof inputFilter)}
                      value={inputFilter}
                    >
                      <option value="all">すべての入力</option>
                      <option value="microphone">マイク</option>
                      <option value="file">ファイル</option>
                    </select>
                  </label>
                  <label>
                    <span>並び順</span>
                    <select
                      aria-label="並べ替え"
                      onChange={(event) => setSortOrder(event.target.value as typeof sortOrder)}
                      value={sortOrder}
                    >
                      <option value="newest">新しい順</option>
                      <option value="oldest">古い順</option>
                      <option value="longest">長い順</option>
                    </select>
                  </label>
                </div>
              )}
            </div>
            <button
              aria-label="設定を開く"
              className="icon-button settings-button"
              onClick={() => openDialog("settings")}
              title="設定"
              type="button"
            >
              ⚙
            </button>
          </div>
        </div>

        <div
          className="history-list"
          onKeyDown={handleHistoryKeyDown}
          role="listbox"
          aria-multiselectable="true"
        >
          {isLoading && <HistorySkeleton />}
          {!isLoading && activeSession && matchesSession(activeSession, query) && (
            <section aria-labelledby="active-heading" className="history-group active-group">
              <h2 id="active-heading">処理中</h2>
              <HistoryRow
                query={query}
                selected={selectedIds.has(activeSession.id)}
                session={activeSession}
                onContextMenu={(event) => openHistoryContextMenu(event, activeSession)}
                onSelect={(event) => selectSession(activeSession, event.metaKey, event.shiftKey)}
              />
            </section>
          )}
          {!isLoading &&
            displayGroups.map(([group, items]) => (
              <section aria-labelledby={`group-${group}`} className="history-group" key={group}>
                <h2 id={`group-${group}`}>{group}</h2>
                {items.map((session) => (
                  <HistoryRow
                    key={session.id}
                    query={query}
                    selected={selectedIds.has(session.id)}
                    session={session}
                    onContextMenu={(event) => openHistoryContextMenu(event, session)}
                    onSelect={(event) => selectSession(session, event.metaKey, event.shiftKey)}
                  />
                ))}
              </section>
            ))}
          {!isLoading && orderedSessions.length === 0 && (
            <div className="empty-list">
              <span aria-hidden="true">⌕</span>
              <p>{sessions.length ? "条件に一致する履歴はありません" : "履歴はまだありません"}</p>
              <small>
                {sessions.length
                  ? "検索条件を変更してください。"
                  : "新規文字起こしから始めましょう。"}
              </small>
            </div>
          )}
          {!isLoading && nextCursor && (
            <button
              className="load-more-button"
              disabled={isHistoryLoading}
              onClick={() => void loadMoreHistory()}
              type="button"
            >
              {isHistoryLoading ? "読み込み中…" : "さらに読み込む"}
            </button>
          )}
        </div>
        <button
          aria-label="新規文字起こし"
          className="record-button"
          disabled={isWorking || isQueueWorking}
          onClick={() => openDialog("new")}
          ref={newButtonRef}
          title="新規文字起こし"
          type="button"
        />
        <div className="input-status">
          <svg aria-hidden="true" viewBox="0 0 16 16">
            <rect height="8" rx="2.5" width="5" x="5.5" y="1.5" />
            <path d="M3.5 7.5v1a4.5 4.5 0 0 0 9 0v-1M8 13v2M5.5 15h5" />
          </svg>
          <strong>{selectedAudioInputLabel}</strong>
        </div>
      </aside>

      <div
        aria-label="履歴ペインの幅を変更"
        aria-orientation="vertical"
        aria-valuemax={420}
        aria-valuemin={280}
        aria-valuenow={paneWidth}
        className="pane-resizer"
        onKeyDown={resizePaneWithKeyboard}
        onPointerDown={resizePane}
        role="separator"
        tabIndex={0}
      />

      <section aria-label="選択した文字起こし" className="session-pane">
        {fatalError ? (
          <EmptyState
            icon="!"
            title="エンジンに接続できません"
            description={fatalError}
            actionLabel="再読み込み"
            onAction={() => window.location.reload()}
          />
        ) : isLoading ? (
          <SessionSkeleton />
        ) : selectedSessions.length > 1 ? (
          <MultiSelection
            sessions={selectedSessions}
            onClear={() => setSelectedIds(new Set())}
            onDelete={() => openDialog("delete")}
            onExport={() => openDialog("export")}
          />
        ) : selectedSession ? (
          <>
            <SessionHeader
              detailQuery={detailQuery}
              disabled={isWorking}
              hasActiveSession={activeSessionId !== undefined}
              onDelete={() => openDialog("delete")}
              onExport={() => openDialog("export")}
              onPause={() => void pauseActive(selectedSession.id)}
              onQueryChange={setDetailQuery}
              onResume={() => void resumePaused(selectedSession.id)}
              queueIsRunning={queue.autoAdvanceEnabled}
              progress={sessionProgress[selectedSession.id]}
              session={selectedSession}
            />
            <Transcript
              autoFollow={autoFollow}
              detailQuery={detailQuery}
              onAutoFollowChange={setAutoFollow}
              scrollRef={transcriptRef}
              session={selectedSession}
            />
          </>
        ) : (
          <EmptyState
            actionLabel="新規文字起こし"
            description="マイクで録音するか、音声ファイルを選んでください。"
            icon="◎"
            onAction={() => openDialog("new")}
            title="文字起こしを始めましょう"
          />
        )}
      </section>

      {dialog === "new" && (
        <NewSessionDialog
          fileDisabled={isWorking || isQueueWorking}
          microphoneDisabled={
            isWorking || activeSessionId !== undefined || queue.autoAdvanceEnabled
          }
          model={model}
          onClose={closeDialog}
          onOpenSettings={() => setDialog("settings")}
          onStart={(kind) => void startSession(kind)}
        />
      )}
      {dialog === "delete" && (
        <DeleteDialog
          disabled={isWorking}
          sessions={selectedSessions}
          onClose={closeDialog}
          onConfirm={() => void deleteSelected()}
        />
      )}
      {dialog === "rename" && renameTarget && (
        <RenameDialog
          disabled={isWorking}
          onClose={closeDialog}
          onConfirm={(title) => void renameSession(title)}
          session={renameTarget}
        />
      )}
      {dialog === "export" && (
        <ExportDialog
          disabled={isWorking}
          format={exportFormat}
          onClose={closeDialog}
          onCopy={() => void copySelected()}
          onConfirm={() => void exportSelected()}
          onFormatChange={setExportFormat}
          sessions={selectedSessions}
        />
      )}
      {dialog === "settings" && (
        <SettingsDialog
          audioInputs={audioInputs}
          deviceId={selectedDeviceId}
          disabled={activeSessionId !== undefined || queue.autoAdvanceEnabled}
          model={model}
          onClose={closeDialog}
          onDeviceChange={(deviceId) => {
            setSelectedDeviceId(deviceId);
          }}
          onModelChange={setModel}
        />
      )}
      {dialog === "close" && closeRequest?.sessionId && (
        <CloseConfirmationDialog
          disabled={isWorking}
          onCancel={() => void resolveClose("cancel")}
          onStopAndQuit={() => void resolveClose("stopAndQuit")}
        />
      )}
      {dialog === "forceClose" && (
        <ForceCloseDialog
          disabled={isWorking}
          error={closeRequest?.error}
          onCancel={() => void resolveClose("cancel")}
          onForceQuit={() => void resolveClose("forceQuit")}
        />
      )}
      {exportOperation && (
        <ExportOperationPanel
          operation={exportOperation}
          onCancel={() => void cancelExport()}
          onClose={() => setExportOperation(undefined)}
          onRetry={() => void runExport(exportOperation.failedSessionIds, exportOperation.format)}
        />
      )}
      {contextMenu && (
        <div
          aria-label={`${contextMenu.session.title}の操作`}
          className="history-context-menu"
          onClick={(event) => event.stopPropagation()}
          role="menu"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          <button
            onClick={() => {
              setRenameTarget(contextMenu.session);
              setContextMenu(undefined);
              openDialog("rename");
            }}
            role="menuitem"
            type="button"
          >
            名前を変更…
          </button>
          <button
            onClick={() => {
              setContextMenu(undefined);
              openDialog("export");
            }}
            role="menuitem"
            type="button"
          >
            Export…
          </button>
          <button
            className="context-danger"
            disabled={!deletableStatuses.includes(contextMenu.session.status)}
            onClick={() => {
              setContextMenu(undefined);
              openDialog("delete");
            }}
            role="menuitem"
            type="button"
          >
            完全に削除…
          </button>
        </div>
      )}
      {toast && (
        <div aria-live="polite" className="toast" role="status">
          {toast}
        </div>
      )}
      <div aria-atomic="true" aria-live="polite" className="sr-only">
        {activeSession ? `${activeSession.segmentCount}件の発言を保存しました` : ""}
      </div>
    </main>
  );
}

interface QueuePanelProps {
  disabled: boolean;
  hasActiveSession: boolean;
  queue: QueueSnapshot;
  onClear: () => void;
  onMove: (itemId: string, offset: -1 | 1) => void;
  onPause: () => void;
  onRemove: (itemId: string) => void;
  onStart: () => void;
}

function QueuePanel({
  disabled,
  hasActiveSession,
  onClear,
  onMove,
  onPause,
  onRemove,
  onStart,
  queue,
}: QueuePanelProps) {
  const pendingCount = queue.items.filter(({ status }) => status === "pending").length;

  return (
    <section aria-labelledby="queue-heading" className="queue-panel">
      <div className="queue-heading-row">
        <div>
          <h2 id="queue-heading">処理キュー</h2>
          <span>{queue.items.length}件</span>
        </div>
        <div className="queue-actions">
          {queue.autoAdvanceEnabled ? (
            <button
              aria-label="自動進行を停止"
              className="queue-icon-button"
              disabled={disabled}
              onClick={onPause}
              title="自動進行を停止"
              type="button"
            >
              <svg aria-hidden="true" className="queue-pause-icon" viewBox="0 0 16 16">
                <path d="M5.5 4v8M10.5 4v8" />
              </svg>
            </button>
          ) : (
            <button
              aria-label="キューを開始"
              className="queue-icon-button queue-start-button"
              disabled={disabled || hasActiveSession || pendingCount === 0}
              onClick={onStart}
              title="キューを開始"
              type="button"
            >
              ▶
            </button>
          )}
          <button
            aria-label="キューを全クリア"
            className="queue-icon-button delete-icon-button"
            disabled={disabled || queue.items.length === 0}
            onClick={onClear}
            title="キューを全クリア"
            type="button"
          >
            <svg aria-hidden="true" viewBox="0 0 16 16">
              <path d="M3.5 4.5h9M6 4.5v-2h4v2m1.5 0-.5 9H5l-.5-9M7 7v4m2-4v4" />
            </svg>
          </button>
        </div>
      </div>
      {queue.items.length > 0 && (
        <div className="queue-items" role="list">
          {queue.items.map((item, index) => (
            <QueueRow
              disabled={disabled}
              index={index}
              item={item}
              key={item.id}
              length={queue.items.length}
              onMove={onMove}
              onRemove={onRemove}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function QueueRow({
  disabled,
  index,
  item,
  length,
  onMove,
  onRemove,
}: {
  disabled: boolean;
  index: number;
  item: QueueItem;
  length: number;
  onMove: (itemId: string, offset: -1 | 1) => void;
  onRemove: (itemId: string) => void;
}) {
  return (
    <div className={`queue-row queue-${item.status}`} role="listitem">
      <div className="queue-row-copy">
        <strong title={item.displayName}>{item.displayName}</strong>
        <span>{item.status === "invalid" ? "無効" : "待機中"}</span>
        {item.status === "invalid" && (
          <small>{item.errorMessage ?? "ファイルを削除して、もう一度追加してください。"}</small>
        )}
      </div>
      <div className="queue-row-actions">
        <button
          aria-label={`${item.displayName}を上へ移動`}
          disabled={disabled || index === 0}
          onClick={() => onMove(item.id, -1)}
          type="button"
        >
          ↑
        </button>
        <button
          aria-label={`${item.displayName}を下へ移動`}
          disabled={disabled || index === length - 1}
          onClick={() => onMove(item.id, 1)}
          type="button"
        >
          ↓
        </button>
        <button
          aria-label={`${item.displayName}をキューから削除`}
          disabled={disabled}
          onClick={() => onRemove(item.id)}
          type="button"
        >
          ×
        </button>
      </div>
    </div>
  );
}

interface HistoryRowProps {
  query: string;
  selected: boolean;
  session: SessionEntity;
  onContextMenu: (event: React.MouseEvent<HTMLButtonElement>) => void;
  onSelect: (event: React.MouseEvent<HTMLButtonElement>) => void;
}

function HistoryRow({ onContextMenu, onSelect, query, selected, session }: HistoryRowProps) {
  const snippet = getSnippet(session, query);

  return (
    <button
      aria-selected={selected}
      className={`history-row ${selected ? "selected" : ""}`}
      data-session-id={session.id}
      onClick={onSelect}
      onContextMenu={onContextMenu}
      role="option"
      type="button"
    >
      <div className="history-row-main">
        <strong>{session.title}</strong>
        <span>{formatDate(session.startedAt)}</span>
      </div>
      <div className="history-row-meta">
        <StatusBadge status={session.status} />
        <span>{session.inputKind === "microphone" ? "マイク" : "ファイル"}</span>
        <span>{formatDuration(session.durationMs)}</span>
      </div>
      {snippet && <p className="history-snippet">{snippet}</p>}
    </button>
  );
}

function StatusBadge({ status }: { status: SessionStatus }) {
  return (
    <span className={`status-badge status-${status}`}>
      <span aria-hidden="true" className="status-symbol">
        {status === "running"
          ? "●"
          : status === "completed"
            ? "✓"
            : status === "failed" || status === "abandoned"
              ? "!"
              : "■"}
      </span>
      {statusLabels[status]}
    </span>
  );
}

interface SessionHeaderProps {
  detailQuery: string;
  disabled: boolean;
  hasActiveSession: boolean;
  session: SessionEntity;
  onDelete: () => void;
  onExport: () => void;
  onPause: () => void;
  onQueryChange: (value: string) => void;
  onResume: () => void;
  progress?: number;
  queueIsRunning: boolean;
}

function SessionHeader({
  detailQuery,
  disabled,
  hasActiveSession,
  onDelete,
  onExport,
  onPause,
  onQueryChange,
  onResume,
  progress,
  queueIsRunning,
  session,
}: SessionHeaderProps) {
  return (
    <header className="session-header">
      <div className="session-title-row">
        <div>
          <h1>{session.title}</h1>
        </div>
        <div className="header-actions">
          {(pausableStatuses.includes(session.status) || session.status === "paused") && (
            <>
              {session.status === "paused" ? (
                <button
                  aria-label="文字起こしを再開"
                  className="icon-button session-control-button"
                  disabled={disabled || hasActiveSession || queueIsRunning}
                  onClick={onResume}
                  title="文字起こしを再開"
                  type="button"
                >
                  <svg aria-hidden="true" viewBox="0 0 16 16">
                    <path d="m5 3.5 7 4.5-7 4.5z" />
                  </svg>
                </button>
              ) : (
                <button
                  aria-label="文字起こしを一時停止"
                  className="icon-button session-control-button"
                  disabled={disabled || session.status === "pausing"}
                  onClick={onPause}
                  title="文字起こしを一時停止"
                  type="button"
                >
                  <svg aria-hidden="true" viewBox="0 0 16 16">
                    <path d="M5.5 4v8M10.5 4v8" />
                  </svg>
                </button>
              )}
              <span aria-hidden="true" className="header-action-spacer" />
            </>
          )}
          <button className="secondary-button" onClick={onExport} type="button">
            ⇧ Export
          </button>
          <button
            aria-label="完全に削除"
            className="icon-button delete-icon-button"
            disabled={!deletableStatuses.includes(session.status)}
            onClick={onDelete}
            title="完全に削除"
            type="button"
          >
            <svg aria-hidden="true" viewBox="0 0 16 16">
              <path d="M3.5 4.5h9M6 4.5v-2h4v2m1.5 0-.5 9H5l-.5-9M7 7v4m2-4v4" />
            </svg>
          </button>
        </div>
      </div>
      <div className="session-metadata">
        <span>{session.language}</span>
        <span>{session.model}</span>
        <span>{session.segmentCount}セグメント</span>
        {session.inputKind === "file" && progress !== undefined && (
          <span>
            {session.status === "completed" ? 100 : Math.min(99, Math.round(progress * 100))}%
          </span>
        )}
      </div>
      <label className="detail-search">
        <span aria-hidden="true">
          <svg viewBox="0 0 16 16">
            <circle cx="6.75" cy="6.75" r="3.75" />
            <path d="m9.5 9.5 3.5 3.5" />
          </svg>
        </span>
        <span className="sr-only">この文字起こし内を検索</span>
        <input
          onChange={(event) => onQueryChange(event.target.value)}
          placeholder="本文を検索"
          type="search"
          value={detailQuery}
        />
      </label>
    </header>
  );
}

interface TranscriptProps {
  autoFollow: boolean;
  detailQuery: string;
  scrollRef: React.RefObject<HTMLDivElement | null>;
  session: SessionEntity;
  onAutoFollowChange: (value: boolean) => void;
}

function Transcript({
  autoFollow,
  detailQuery,
  onAutoFollowChange,
  scrollRef,
  session,
}: TranscriptProps) {
  const normalizedQuery = detailQuery.trim().toLocaleLowerCase("ja-JP");
  const segments = session.segments.filter(({ text }) =>
    text.toLocaleLowerCase("ja-JP").includes(normalizedQuery),
  );

  function handleScroll(): void {
    const element = scrollRef.current;

    if (!element || session.status !== "running") return;
    const isNearBottom = element.scrollHeight - element.scrollTop - element.clientHeight <= 80;

    onAutoFollowChange(isNearBottom);
  }

  return (
    <div className="transcript-scroll" onScroll={handleScroll} ref={scrollRef}>
      <article className="transcript" aria-label="文字起こし本文">
        {session.errorMessage && (
          <div className="error-banner" role="alert">
            <strong>{statusLabels[session.status]}</strong>
            <p>{session.errorMessage}</p>
            <small>保存済みの内容は閲覧・Exportできます。</small>
          </div>
        )}
        {detailQuery && <p className="search-summary">{segments.length}件の一致</p>}
        {segments.length ? (
          segments.map((segment) => (
            <section className="segment" id={`segment-${segment.id}`} key={segment.id}>
              <time dateTime={`PT${Math.floor(segment.startMs / 1_000)}S`}>
                {formatTimestamp(segment.startMs)}
              </time>
              <p>{highlightText(segment.text, detailQuery)}</p>
            </section>
          ))
        ) : (
          <div className="empty-transcript">
            <span aria-hidden="true">{detailQuery ? "⌕" : "◌"}</span>
            <p>
              {detailQuery
                ? "一致する発言はありません"
                : session.status === "running"
                  ? "最初の発言を待っています…"
                  : "保存された発言はありません"}
            </p>
          </div>
        )}
      </article>
      {!autoFollow && session.status === "running" && (
        <button
          className="jump-latest"
          onClick={() => {
            onAutoFollowChange(true);
            scrollRef.current?.scrollTo({
              behavior: "auto",
              top: scrollRef.current.scrollHeight,
            });
          }}
          type="button"
        >
          ↓ 最新位置へ
        </button>
      )}
    </div>
  );
}

function highlightText(text: string, query: string): React.ReactNode {
  const normalized = query.trim();

  if (!normalized) return text;
  const index = text.toLocaleLowerCase("ja-JP").indexOf(normalized.toLocaleLowerCase("ja-JP"));

  if (index < 0) return text;

  return (
    <>
      {text.slice(0, index)}
      <mark>{text.slice(index, index + normalized.length)}</mark>
      {text.slice(index + normalized.length)}
    </>
  );
}

function MultiSelection({
  onClear,
  onDelete,
  onExport,
  sessions,
}: {
  sessions: SessionEntity[];
  onClear: () => void;
  onDelete: () => void;
  onExport: () => void;
}) {
  const states = sessions.reduce<Record<string, number>>(
    (result, session) => ({
      ...result,
      [statusLabels[session.status]]: (result[statusLabels[session.status]] ?? 0) + 1,
    }),
    {},
  );
  const hasActive = sessions.some(({ status }) => !deletableStatuses.includes(status));

  return (
    <div className="multi-selection">
      <button className="text-button" onClick={onClear} type="button">
        ← 選択を解除
      </button>
      <div className="multi-card">
        <span aria-hidden="true" className="multi-icon">
          ✓
        </span>
        <h1>{sessions.length}件を選択中</h1>
        <p>
          合計 {formatDuration(sessions.reduce((total, { durationMs }) => total + durationMs, 0))}
        </p>
        <div className="state-summary">
          {Object.entries(states).map(([label, count]) => (
            <span key={label}>
              {label} {count}
            </span>
          ))}
        </div>
        <div className="multi-actions">
          <button className="primary-button" onClick={onExport} type="button">
            ⇧ まとめてExport
          </button>
          <button
            className="danger-secondary"
            disabled={hasActive}
            onClick={onDelete}
            title={hasActive ? "処理中のセッションは削除できません" : undefined}
            type="button"
          >
            完全に削除…
          </button>
        </div>
        {hasActive && (
          <small>処理中のセッションを削除するには、先にStopまたは中断してください。</small>
        )}
      </div>
    </div>
  );
}

function DialogFrame({
  children,
  onClose,
  title,
  titleId,
}: {
  children: React.ReactNode;
  onClose: () => void;
  title: string;
  titleId: string;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const dialogElement = dialogRef.current;

    dialogElement?.showModal();
    dialogElement?.querySelector<HTMLElement>("[autofocus]")?.focus();

    return () => dialogElement?.close();
  }, []);

  function moveActionFocus(event: KeyboardEvent<HTMLDialogElement>): void {
    if (!["ArrowDown", "ArrowLeft", "ArrowRight", "ArrowUp"].includes(event.key)) return;
    if (
      event.altKey ||
      event.ctrlKey ||
      event.metaKey ||
      event.shiftKey ||
      event.nativeEvent.isComposing
    )
      return;
    if (
      event.target instanceof HTMLElement &&
      event.target.closest("input, select, textarea, [contenteditable='true']")
    )
      return;

    const buttons = [
      ...(event.currentTarget.querySelector(".dialog-actions")?.querySelectorAll("button") ?? []),
    ].filter(
      (button): button is HTMLButtonElement =>
        button instanceof HTMLButtonElement && !button.disabled,
    );

    if (!buttons.length) return;
    event.preventDefault();
    const currentIndex = buttons.findIndex((button) => button === document.activeElement);
    const offset = event.key === "ArrowRight" || event.key === "ArrowDown" ? 1 : -1;
    const nextIndex =
      currentIndex < 0
        ? offset > 0
          ? 0
          : buttons.length - 1
        : (currentIndex + offset + buttons.length) % buttons.length;

    buttons[nextIndex]?.focus();
  }

  return (
    <dialog
      aria-labelledby={titleId}
      className="dialog"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      onKeyDownCapture={moveActionFocus}
      ref={dialogRef}
    >
      <div className="dialog-title">
        <h2 id={titleId}>{title}</h2>
        <button
          aria-label="閉じる"
          className="icon-button"
          onClick={onClose}
          title="閉じる"
          type="button"
        >
          ×
        </button>
      </div>
      {children}
    </dialog>
  );
}

function DialogActions({ children }: { children: React.ReactNode }) {
  return <div className="dialog-actions">{children}</div>;
}

function NewSessionDialog({
  fileDisabled,
  microphoneDisabled,
  model,
  onClose,
  onOpenSettings,
  onStart,
}: {
  fileDisabled: boolean;
  microphoneDisabled: boolean;
  model: ModelState;
  onClose: () => void;
  onOpenSettings: () => void;
  onStart: (kind: InputKind) => void;
}) {
  return (
    <DialogFrame onClose={onClose} title="新規文字起こし" titleId="new-session-title">
      {model.status !== "ready" ? (
        <div className="model-setup">
          <span aria-hidden="true">◎</span>
          <h3>音声認識モデルを選択</h3>
          <p>{modelStatusText(model)}</p>
          <button className="primary-button" onClick={onOpenSettings} type="button">
            設定を開く
          </button>
        </div>
      ) : (
        <div className="source-options">
          <button disabled={microphoneDisabled} onClick={() => onStart("microphone")} type="button">
            <span aria-hidden="true" className="source-icon">
              ●
            </span>
            <strong>マイクで録音</strong>
            <small>選択した入力から文字起こしします</small>
          </button>
          <button disabled={fileDisabled} onClick={() => onStart("file")} type="button">
            <span aria-hidden="true" className="source-icon">
              ♪
            </span>
            <strong>音声ファイルを選択</strong>
            <small>複数選択してキューに追加できます</small>
          </button>
        </div>
      )}
      <p className="privacy-note">
        マイクの元音声は保存しません。確定した文字起こしだけが履歴に残ります。
      </p>
    </DialogFrame>
  );
}

function RenameDialog({
  disabled,
  onClose,
  onConfirm,
  session,
}: {
  disabled: boolean;
  onClose: () => void;
  onConfirm: (title: string) => void;
  session: SessionEntity;
}) {
  const [title, setTitle] = useState(session.title);
  const normalized = title.trim();

  return (
    <DialogFrame onClose={onClose} title="名前を変更" titleId="rename-title">
      <form
        className="rename-form"
        onSubmit={(event) => {
          event.preventDefault();
          if (normalized) onConfirm(normalized);
        }}
      >
        <label>
          <span>名前</span>
          <input
            autoFocus
            maxLength={200}
            onChange={(event) => setTitle(event.target.value)}
            onFocus={(event) => event.currentTarget.select()}
            value={title}
          />
        </label>
        <DialogActions>
          <button className="secondary-button" onClick={onClose} type="button">
            キャンセル
          </button>
          <button className="primary-button" disabled={disabled || !normalized} type="submit">
            変更
          </button>
        </DialogActions>
      </form>
    </DialogFrame>
  );
}

function DeleteDialog({
  disabled,
  onClose,
  onConfirm,
  sessions,
}: {
  disabled: boolean;
  sessions: SessionEntity[];
  onClose: () => void;
  onConfirm: () => void;
}) {
  const hasActive = sessions.some(({ status }) => !deletableStatuses.includes(status));

  return (
    <DialogFrame onClose={onClose} title="文字起こしを完全に削除" titleId="delete-title">
      <div className="warning-icon" aria-hidden="true">
        !
      </div>
      <p>
        {sessions.length === 1 ? (
          <>
            「<strong>{sessions[0]?.title}</strong>」を完全に削除します。
          </>
        ) : (
          <>
            <strong>{sessions.length}件</strong>の文字起こしを完全に削除します。
          </>
        )}
      </p>
      <p className="dialog-warning">
        この操作は取り消せません。既にExportしたファイルと元の音声ファイルは削除されません。
      </p>
      {hasActive && (
        <p className="inline-error" role="alert">
          処理中のセッションは削除できません。
        </p>
      )}
      <DialogActions>
        <button autoFocus className="secondary-button" onClick={onClose} type="button">
          キャンセル
        </button>
        <button
          className="danger-button"
          disabled={disabled || hasActive}
          onClick={onConfirm}
          type="button"
        >
          完全に削除
        </button>
      </DialogActions>
    </DialogFrame>
  );
}

function ExportDialog({
  disabled,
  format,
  onClose,
  onCopy,
  onConfirm,
  onFormatChange,
  sessions,
}: {
  disabled: boolean;
  format: ExportFormat;
  sessions: SessionEntity[];
  onClose: () => void;
  onCopy: () => void;
  onConfirm: () => void;
  onFormatChange: (format: ExportFormat) => void;
}) {
  return (
    <DialogFrame
      onClose={onClose}
      title={sessions.length > 1 ? `${sessions.length}件をまとめてExport` : "文字起こしをExport"}
      titleId="export-title"
    >
      <fieldset className="format-list">
        <legend>形式</legend>
        {exportFormats.map((value) => (
          <label key={value}>
            <input
              checked={format === value}
              name="format"
              onChange={() => onFormatChange(value)}
              type="radio"
            />
            <span>
              <strong>{exportLabels[value]}</strong>
              <small>
                {value === "json"
                  ? "設定と診断情報を含みます"
                  : value === "srt" || value === "vtt"
                    ? "タイムスタンプ付き字幕"
                    : ""}
              </small>
            </span>
          </label>
        ))}
      </fieldset>
      {sessions.length > 1 && (
        <p className="info-note">各セッションのファイルとmanifestをZIPにまとめます。</p>
      )}
      <DialogActions>
        <button className="primary-button" disabled={disabled} onClick={onCopy} type="button">
          コピー
        </button>
        <button className="primary-button" disabled={disabled} onClick={onConfirm} type="button">
          保存先を選択…
        </button>
      </DialogActions>
    </DialogFrame>
  );
}

function SettingsDialog({
  audioInputs,
  deviceId,
  disabled,
  model,
  onClose,
  onDeviceChange,
  onModelChange,
}: {
  audioInputs: AudioInput[];
  deviceId: string;
  disabled: boolean;
  model: ModelState;
  onClose: () => void;
  onDeviceChange: (deviceId: string) => void;
  onModelChange: (model: ModelState) => void;
}) {
  const [models, setModels] = useState<CachedModelRevision[]>([]);
  const [modelListError, setModelListError] = useState<string>();
  const [isModelWorking, setIsModelWorking] = useState(false);

  async function reloadModels(): Promise<void> {
    setIsModelWorking(true);
    setModelListError(undefined);
    try {
      const result = await recoBridge.listModels();

      setModels(result.models);
      onModelChange(result.state);
    } catch (error) {
      setModelListError(error instanceof Error ? error.message : String(error));
    } finally {
      setIsModelWorking(false);
    }
  }

  useEffect(() => {
    void reloadModels();
  }, []);

  async function selectModel(value: string): Promise<void> {
    const selected = models.find(({ repoId, revision }) => `${repoId}\n${revision}` === value);

    if (!selected) return;
    setIsModelWorking(true);
    setModelListError(undefined);
    try {
      const reference = { repoId: selected.repoId, revision: selected.revision };

      onModelChange({ selected: reference, status: "loading" });
      onModelChange(await recoBridge.selectModel(reference));
    } catch (error) {
      setModelListError(error instanceof Error ? error.message : String(error));
      onModelChange(await recoBridge.getModelState());
    } finally {
      setIsModelWorking(false);
    }
  }

  const selectedModel = models.find(
    ({ repoId, revision }) =>
      repoId === model.selected?.repoId && revision === model.selected.revision,
  );

  return (
    <DialogFrame onClose={onClose} title="設定" titleId="settings-title">
      <div className="settings-section">
        <h3>音声入力</h3>
        <label>
          使用するマイク
          <select onChange={(event) => onDeviceChange(event.target.value)} value={deviceId}>
            <option value="">システムの既定</option>
            {audioInputs.map((input) => (
              <option key={input.id} value={input.id}>
                {input.name}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="settings-section">
        <h3>音声認識モデル</h3>
        <div className="model-picker">
          <div className="model-picker-control">
            <label>
              <span>使用するモデル</span>
              <select
                disabled={disabled || isModelWorking || model.status === "cliMissing"}
                onChange={(event) => void selectModel(event.target.value)}
                value={model.selected ? `${model.selected.repoId}\n${model.selected.revision}` : ""}
              >
                <option disabled value="">
                  モデルを選択…
                </option>
                {model.selected && !selectedModel && (
                  <option value={`${model.selected.repoId}\n${model.selected.revision}`}>
                    {model.selected.repoId} — {model.selected.revision.slice(0, 8)} — 利用不可
                  </option>
                )}
                {models.map((candidate) => (
                  <option
                    key={`${candidate.repoId}:${candidate.revision}`}
                    value={`${candidate.repoId}\n${candidate.revision}`}
                  >
                    {candidate.repoId} — {candidate.revision.slice(0, 8)} — {candidate.size}
                  </option>
                ))}
              </select>
            </label>
            <button
              className="secondary-button model-refresh-button"
              disabled={disabled || isModelWorking}
              onClick={() => void reloadModels()}
              type="button"
            >
              <span aria-hidden="true">↻</span>
              {isModelWorking ? "確認中…" : "再読み込み"}
            </button>
          </div>
          <div className="model-picker-meta">
            <span className={`model-status model-status-${model.status}`}>
              <span className={`model-dot model-${model.status}`} />
              {modelStatusText(model)}
            </span>
          </div>
        </div>
        {disabled && <p>処理中はモデルを変更できません。</p>}
        {(model.errorMessage || modelListError) && (
          <p role="alert">{model.errorMessage ?? modelListError}</p>
        )}
      </div>
      <div className="settings-section about">
        <h3>このアプリについて</h3>
        <dl>
          <div>
            <dt>RecoGUI</dt>
            <dd>0.1.0</dd>
          </div>
          <div>
            <dt>エンジン</dt>
            <dd>接続済み</dd>
          </div>
          <div>
            <dt>データベース</dt>
            <dd>正常</dd>
          </div>
        </dl>
      </div>
      <DialogActions>
        <button className="primary-button" onClick={onClose} type="button">
          完了
        </button>
      </DialogActions>
    </DialogFrame>
  );
}

function CloseConfirmationDialog({
  disabled,
  onCancel,
  onStopAndQuit,
}: {
  disabled: boolean;
  onCancel: () => void;
  onStopAndQuit: () => void;
}) {
  return (
    <DialogFrame onClose={onCancel} title="録音を停止して終了しますか？" titleId="close-title">
      <p>処理中の文字起こしがあります。Stopと同じ手順で保存を完了してから終了します。</p>
      <p className="dialog-warning">保存済みの文字起こしは履歴に残ります。</p>
      <DialogActions>
        <button autoFocus className="secondary-button" onClick={onCancel} type="button">
          アプリに戻る
        </button>
        <button
          className="primary-button"
          disabled={disabled}
          onClick={onStopAndQuit}
          type="button"
        >
          停止して終了
        </button>
      </DialogActions>
    </DialogFrame>
  );
}

function ForceCloseDialog({
  disabled,
  error,
  onCancel,
  onForceQuit,
}: {
  disabled: boolean;
  error?: string;
  onCancel: () => void;
  onForceQuit: () => void;
}) {
  return (
    <DialogFrame onClose={onCancel} title="正常に停止できませんでした" titleId="force-close-title">
      <div className="warning-icon" aria-hidden="true">
        !
      </div>
      <p>{error ?? "エンジンから停止完了の応答がありません。"}</p>
      <p className="dialog-warning">
        強制終了すると、未処理の音声は失われます。すでに保存された部分は次回起動時に履歴へ残ります。
      </p>
      <DialogActions>
        <button autoFocus className="secondary-button" onClick={onCancel} type="button">
          アプリに戻る
        </button>
        <button className="danger-button" disabled={disabled} onClick={onForceQuit} type="button">
          強制終了
        </button>
      </DialogActions>
    </DialogFrame>
  );
}

function ExportOperationPanel({
  onCancel,
  onClose,
  onRetry,
  operation,
}: {
  operation: ExportOperation;
  onCancel: () => void;
  onClose: () => void;
  onRetry: () => void;
}) {
  const isRunning = operation.state === "running" || operation.state === "canceling";
  const title =
    operation.state === "completed"
      ? "Exportが完了しました"
      : operation.state === "canceled"
        ? "Exportを中断しました"
        : operation.state === "failed"
          ? "一部をExportできませんでした"
          : operation.state === "canceling"
            ? "Exportを中断しています"
            : "Exportしています";

  return (
    <aside aria-live="polite" className="export-operation" role="status">
      <div>
        <strong>{title}</strong>
        <span>
          {operation.sessionIds.length}件 · {exportLabels[operation.format]}
        </span>
      </div>
      {isRunning && (
        <progress aria-label="Exportの進捗" max={1} value={operation.progress || undefined} />
      )}
      <div className="export-operation-actions">
        {operation.state === "running" && (
          <button
            disabled={!operation.operationId}
            onClick={onCancel}
            title={operation.operationId ? undefined : "Exportの開始を待っています"}
            type="button"
          >
            中断
          </button>
        )}
        {operation.state === "failed" && operation.failedSessionIds.length > 0 && (
          <button onClick={onRetry} type="button">
            失敗した{operation.failedSessionIds.length}件を再試行
          </button>
        )}
        {!isRunning && (
          <button aria-label="Export通知を閉じる" onClick={onClose} type="button">
            閉じる
          </button>
        )}
      </div>
    </aside>
  );
}

function EmptyState({
  actionLabel,
  description,
  icon,
  onAction,
  title,
}: {
  actionLabel: string;
  description: string;
  icon: string;
  onAction: () => void;
  title: string;
}) {
  return (
    <div className="empty-state">
      <span aria-hidden="true">{icon}</span>
      <h1>{title}</h1>
      <p>{description}</p>
      <button className="primary-button" onClick={onAction} type="button">
        {actionLabel}
      </button>
    </div>
  );
}

function HistorySkeleton() {
  return (
    <div aria-label="履歴を読み込み中" className="skeleton-list" role="status">
      {[1, 2, 3, 4].map((item) => (
        <div className="skeleton-row" key={item}>
          <i />
          <i />
        </div>
      ))}
    </div>
  );
}

function SessionSkeleton() {
  return (
    <div aria-label="セッションを読み込み中" className="session-skeleton" role="status">
      <i />
      <i />
      <i />
      <i />
    </div>
  );
}

export default App;

// @vitest-environment jsdom
/* oxlint-disable no-ternary, curly, @stylistic/padding-line-between-statements */
import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import { mockSnapshot } from "./mockData";

import type { EngineEvent } from "./types";

const scrollToMock = vi.fn();
const bridgeMocks = vi.hoisted(() => ({
  cancelExport: vi.fn(),
  clearQueue: vi.fn(),
  closeForceHandler: undefined as
    | ((payload: { error: string; sessionId: string | null }) => void)
    | undefined,
  closeHandler: undefined as ((payload: { sessionId: string }) => void) | undefined,
  deleteModel: vi.fn(),
  deleteSessions: vi.fn(),
  downloadModel: vi.fn(),
  enqueueFiles: vi.fn(),
  eventHandler: undefined as ((event: EngineEvent) => void) | undefined,
  exportSessions: vi.fn().mockResolvedValue({ canceled: false, completed: true }),
  getQueueState: vi.fn(),
  getSession: vi.fn(),
  getSnapshot: vi.fn(),
  listAudioInputs: vi.fn().mockResolvedValue([{ channels: 1, id: "7", name: "テストマイク" }]),
  listHistory: vi.fn(),
  onCloseForceRequired: vi.fn(),
  onCloseRequested: vi.fn(),
  onEngineEvent: vi.fn(),
  pauseQueue: vi.fn(),
  pauseSession: vi.fn(),
  pickAudioFiles: vi.fn().mockResolvedValue([{ displayName: "test.wav", sourceToken: "token" }]),
  removeQueueItem: vi.fn(),
  renameSession: vi.fn(),
  reorderQueue: vi.fn(),
  resolveClose: vi.fn(),
  resumeSession: vi.fn(),
  searchHistory: vi.fn(),
  startQueue: vi.fn(),
  startSession: vi.fn().mockResolvedValue({ sessionId: "new-session" }),
  verifyModel: vi.fn(),
}));

vi.mock("./bridge", () => ({
  recoBridge: bridgeMocks,
}));

beforeAll(() => {
  const values = new Map<string, string>();
  const storage: Storage = {
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    get length() {
      return values.size;
    },
    removeItem: (key) => values.delete(key),
    setItem: (key, value) => values.set(key, value),
  };

  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: storage });
  HTMLDialogElement.prototype.showModal = vi.fn(function showModal(this: HTMLDialogElement) {
    this.open = true;
  });
  HTMLDialogElement.prototype.close = vi.fn(function close(this: HTMLDialogElement) {
    this.open = false;
  });
  Element.prototype.scrollTo = scrollToMock;
});

beforeEach(() => {
  localStorage.clear();
  bridgeMocks.getSession.mockImplementation((sessionId: string) => {
    const session = mockSnapshot.sessions.find(({ id }) => id === sessionId) ?? {
      ...mockSnapshot.sessions[0],
      id: sessionId,
      segments: [],
      segmentsLoaded: true,
      status: "preparing" as const,
      title: "新しい録音",
    };

    return Promise.resolve(structuredClone(session));
  });
  bridgeMocks.getSnapshot.mockResolvedValue(structuredClone(mockSnapshot));
  const queue = { autoAdvanceEnabled: false, items: [], revision: 0 };

  bridgeMocks.getQueueState.mockResolvedValue(structuredClone(queue));
  bridgeMocks.enqueueFiles.mockResolvedValue({
    autoAdvanceEnabled: true,
    items: [],
    revision: 1,
  });
  bridgeMocks.startQueue.mockResolvedValue({
    autoAdvanceEnabled: true,
    items: [],
    revision: 2,
  });
  bridgeMocks.pauseQueue.mockResolvedValue(structuredClone(queue));
  bridgeMocks.clearQueue.mockResolvedValue(structuredClone(queue));
  bridgeMocks.removeQueueItem.mockResolvedValue(structuredClone(queue));
  bridgeMocks.reorderQueue.mockResolvedValue(structuredClone(queue));
  bridgeMocks.listHistory.mockResolvedValue({ items: structuredClone(mockSnapshot.sessions) });
  bridgeMocks.searchHistory.mockResolvedValue({ items: structuredClone(mockSnapshot.sessions) });
  bridgeMocks.onEngineEvent.mockImplementation((handler: (event: EngineEvent) => void) => {
    bridgeMocks.eventHandler = handler;

    return Promise.resolve(() => undefined);
  });
  bridgeMocks.onCloseRequested.mockImplementation(
    (handler: (payload: { sessionId: string }) => void) => {
      bridgeMocks.closeHandler = handler;

      return Promise.resolve(() => undefined);
    },
  );
  bridgeMocks.onCloseForceRequired.mockImplementation(
    (handler: (payload: { error: string; sessionId: string | null }) => void) => {
      bridgeMocks.closeForceHandler = handler;

      return Promise.resolve(() => undefined);
    },
  );
  bridgeMocks.deleteSessions.mockClear();
  bridgeMocks.exportSessions.mockClear();
  bridgeMocks.pauseSession.mockClear();
  bridgeMocks.pauseQueue.mockClear();
  bridgeMocks.resumeSession.mockClear();
  bridgeMocks.renameSession.mockImplementation((sessionId: string, title: string) =>
    Promise.resolve({ rowVersion: 7, sessionId, title }),
  );
  bridgeMocks.enqueueFiles.mockClear();
  scrollToMock.mockClear();
});

afterEach(() => {
  cleanup();
});

async function renderLoadedApp() {
  render(<App />);
  await screen.findByRole("heading", { name: "新しい録音" });
}

function useInactiveSnapshot() {
  bridgeMocks.getSnapshot.mockResolvedValue({
    ...structuredClone(mockSnapshot),
    activeSessionId: undefined,
    sessions: structuredClone(mockSnapshot.sessions).map((session) =>
      session.id === "session-live" ? { ...session, status: "paused" as const } : session,
    ),
  });
}

describe("RecoGUI", () => {
  it("keeps the selected history session when a live segment arrives", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("option", { name: /プロジェクト定例/ }));
    expect(screen.getByRole("heading", { name: "プロジェクト定例" })).toBeInTheDocument();

    bridgeMocks.eventHandler?.({
      event: "segment.persisted",
      payload: {
        characters: 242,
        mediaDurationMs: 92_000,
        recognizedSegments: 4,
        rowVersion: 6,
        segment: {
          endMs: 92_000,
          id: "new-segment",
          sequence: 4,
          startMs: 88_000,
          text: "新しい発言",
        },
        totalSegments: 4,
      },
      sequence: 10,
      sessionId: "session-live",
    });

    expect(screen.getByRole("heading", { name: "プロジェクト定例" })).toBeInTheDocument();
    expect(screen.queryByText(/処理中へ戻る/)).not.toBeInTheDocument();
  });

  it("renders a duplicated persisted segment exactly once", async () => {
    await renderLoadedApp();
    const event: EngineEvent = {
      event: "segment.persisted",
      payload: {
        characters: 242,
        mediaDurationMs: 92_000,
        recognizedSegments: 4,
        rowVersion: 6,
        segment: {
          endMs: 92_000,
          id: "session-live:4",
          sequence: 4,
          startMs: 88_000,
          text: "重複しない確定文",
        },
        totalSegments: 4,
      },
      sequence: 20,
      sessionId: "session-live",
    };

    bridgeMocks.eventHandler?.(event);
    bridgeMocks.eventHandler?.({ ...event, sequence: 21 });

    expect(await screen.findByText("重複しない確定文")).toBeInTheDocument();
    expect(screen.getAllByText("重複しない確定文")).toHaveLength(1);
    expect(screen.getByText("4セグメント")).toBeInTheDocument();
  });

  it("shows file transcription progress in the session metadata", async () => {
    bridgeMocks.getSnapshot.mockResolvedValue({
      ...structuredClone(mockSnapshot),
      sessions: structuredClone(mockSnapshot.sessions).map((session) =>
        session.id === "session-live"
          ? { ...session, inputKind: "file" as const, inputName: "lecture.wav" }
          : session,
      ),
    });
    await renderLoadedApp();

    bridgeMocks.eventHandler?.({
      event: "session.progress",
      payload: { processedAudioMs: 15_000, totalAudioMs: 60_000 },
      sequence: 22,
      sessionId: "session-live",
    });

    expect(await screen.findByText("25%")).toBeInTheDocument();
  });

  it("pauses the active session", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "文字起こしを一時停止" }));
    await waitFor(() => expect(bridgeMocks.pauseSession).toHaveBeenCalledWith("session-live"));
  });

  it("shows pause for a running queued file even when the snapshot omits the active id", async () => {
    const user = userEvent.setup();

    bridgeMocks.getSnapshot.mockResolvedValue({
      ...structuredClone(mockSnapshot),
      activeSessionId: undefined,
      sessions: structuredClone(mockSnapshot.sessions).map((session) =>
        session.id === "session-live"
          ? { ...session, inputKind: "file" as const, inputName: "lecture.wav" }
          : session,
      ),
    });
    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "文字起こしを一時停止" }));

    await waitFor(() => expect(bridgeMocks.pauseQueue).toHaveBeenCalledOnce());
    expect(bridgeMocks.pauseSession).toHaveBeenCalledWith("session-live");
  });

  it("resumes a paused session when no other session is active", async () => {
    const user = userEvent.setup();

    useInactiveSnapshot();
    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "文字起こしを再開" }));
    await waitFor(() => expect(bridgeMocks.resumeSession).toHaveBeenCalledWith("session-live"));
  });

  it("submits a single idle file without showing an empty queue", async () => {
    const user = userEvent.setup();

    useInactiveSnapshot();
    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: /新規文字起こし/ }));
    await user.click(screen.getByRole("button", { name: /音声ファイルを選択/ }));

    await waitFor(() =>
      expect(bridgeMocks.enqueueFiles).toHaveBeenCalledWith([
        { displayName: "test.wav", sourceToken: "token" },
      ]),
    );
    expect(bridgeMocks.startSession).not.toHaveBeenCalledWith(
      expect.objectContaining({ inputKind: "file" }),
    );
    expect(screen.queryByRole("region", { name: "処理キュー" })).not.toBeInTheDocument();
  });

  it("keeps file selection available while a session is active", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: /新規文字起こし/ }));

    expect(screen.getByRole("button", { name: /マイクで録音/ })).toBeDisabled();
    expect(screen.getByRole("button", { name: /音声ファイルを選択/ })).toBeEnabled();
  });

  it("reconciles the queue from queue.changed events", async () => {
    await renderLoadedApp();

    bridgeMocks.eventHandler?.({
      event: "queue.changed",
      payload: {
        autoAdvanceEnabled: false,
        items: [
          {
            addedAt: "2026-07-21T00:00:00.000Z",
            displayName: "missing.wav",
            errorMessage: "ファイルが見つかりません",
            id: "queue-invalid",
            status: "invalid",
            updatedAt: "2026-07-21T00:00:00.000Z",
          },
        ],
        revision: 3,
      },
      sequence: 30,
    });

    expect(await screen.findByText("missing.wav")).toBeInTheDocument();
    expect(screen.getByText("ファイルが見つかりません")).toBeInTheDocument();
  });

  it("requires confirmation before permanently deleting a completed session", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("option", { name: /プロジェクト定例/ }));
    await user.click(screen.getByRole("button", { name: "完全に削除" }));

    expect(screen.getByText(/この操作は取り消せません/)).toBeInTheDocument();
    expect(bridgeMocks.deleteSessions).not.toHaveBeenCalled();
    await user.click(
      within(screen.getByRole("dialog")).getByRole("button", { name: "完全に削除" }),
    );
    await waitFor(() => expect(bridgeMocks.deleteSessions).toHaveBeenCalledWith(["session-1"]));
  });

  it("renames a session from the history context menu", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    const row = screen.getByRole("option", { name: /プロジェクト定例/ });

    fireEvent.contextMenu(row, { clientX: 120, clientY: 180 });
    await user.click(screen.getByRole("menuitem", { name: "名前を変更…" }));
    const input = screen.getByRole("textbox", { name: "名前" });

    await user.clear(input);
    await user.type(input, "週次レビュー");
    await user.click(screen.getByRole("button", { name: "変更" }));

    await waitFor(() =>
      expect(bridgeMocks.renameSession).toHaveBeenCalledWith("session-1", "週次レビュー"),
    );
    expect(await screen.findByRole("heading", { name: "週次レビュー" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /週次レビュー/ })).toBeInTheDocument();
  });

  it("exports a selected session in the chosen format", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("option", { name: /プロジェクト定例/ }));
    await user.click(screen.getByRole("button", { name: /Export/ }));
    await user.click(screen.getByRole("radio", { name: /JSON/ }));
    await user.click(screen.getByRole("button", { name: "保存先を選択…" }));

    await waitFor(() =>
      expect(bridgeMocks.exportSessions).toHaveBeenCalledWith(["session-1"], "json"),
    );
  });

  it("supports arrow-key selection and restores focus after a dialog", async () => {
    const user = userEvent.setup();

    useInactiveSnapshot();
    await renderLoadedApp();
    const history = screen.getByRole("listbox");

    fireEvent.keyDown(history, { key: "ArrowDown" });
    expect(await screen.findByRole("heading", { name: "プロジェクト定例" })).toBeInTheDocument();

    const newButton = screen.getByRole("button", { name: /新規文字起こし/ });

    await user.click(newButton);
    await user.click(screen.getByRole("button", { name: "閉じる" }));
    await waitFor(() => expect(newButton).toHaveFocus());
  });

  it("pauses live auto-follow when the transcript is scrolled away from the bottom", async () => {
    await renderLoadedApp();
    const transcript = document.querySelector<HTMLDivElement>(".transcript-scroll");

    expect(transcript).not.toBeNull();
    Object.defineProperties(transcript, {
      clientHeight: { configurable: true, value: 400 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: { configurable: true, value: 100, writable: true },
    });
    fireEvent.scroll(transcript!);

    expect(await screen.findByRole("button", { name: "↓ 最新位置へ" })).toBeInTheDocument();
  });

  it("follows new segments only while the transcript is pinned to the bottom", async () => {
    await renderLoadedApp();
    const transcript = document.querySelector<HTMLDivElement>(".transcript-scroll");

    expect(transcript).not.toBeNull();
    Object.defineProperties(transcript, {
      clientHeight: { configurable: true, value: 400 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: { configurable: true, value: 600, writable: true },
    });
    fireEvent.scroll(transcript!);
    scrollToMock.mockClear();

    bridgeMocks.eventHandler?.({
      event: "segment.persisted",
      payload: {
        characters: 250,
        mediaDurationMs: 100_000,
        recognizedSegments: 4,
        rowVersion: 6,
        segment: {
          endMs: 100_000,
          id: "followed-segment",
          sequence: 4,
          startMs: 95_000,
          text: "末尾では追従する発言",
        },
        totalSegments: 4,
      },
      sequence: 40,
      sessionId: "session-live",
    });

    await waitFor(() =>
      expect(scrollToMock).toHaveBeenCalledWith({ behavior: "auto", top: 1_000 }),
    );

    transcript!.scrollTop = 100;
    fireEvent.scroll(transcript!);
    scrollToMock.mockClear();
    bridgeMocks.eventHandler?.({
      event: "segment.persisted",
      payload: {
        characters: 260,
        mediaDurationMs: 110_000,
        recognizedSegments: 5,
        rowVersion: 7,
        segment: {
          endMs: 110_000,
          id: "unfollowed-segment",
          sequence: 5,
          startMs: 105_000,
          text: "上では位置を保つ発言",
        },
        totalSegments: 5,
      },
      sequence: 41,
      sessionId: "session-live",
    });

    expect(await screen.findByText("上では位置を保つ発言")).toBeInTheDocument();
    expect(scrollToMock).not.toHaveBeenCalled();
  });

  it("debounces full-text history search through the backend", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.type(screen.getByRole("searchbox", { name: "履歴を検索" }), "検索語");

    await waitFor(() =>
      expect(bridgeMocks.searchHistory).toHaveBeenLastCalledWith(
        expect.objectContaining({ query: "検索語" }),
      ),
    );
  });

  it("persists the default microphone device and passes its id when starting", async () => {
    const user = userEvent.setup();

    useInactiveSnapshot();
    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "設定を開く" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "使用するマイク" }), "7");
    await user.click(screen.getByRole("button", { name: "完了" }));
    await user.click(screen.getByRole("button", { name: /新規文字起こし/ }));
    await user.click(screen.getByRole("button", { name: /マイクで録音/ }));

    await waitFor(() =>
      expect(bridgeMocks.startSession).toHaveBeenCalledWith(
        expect.objectContaining({ deviceId: "7", inputKind: "microphone" }),
      ),
    );
    expect(localStorage.getItem("reco.defaultInputDeviceId")).toBe("7");
  });

  it("requires a second explicit confirmation before force quitting", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    bridgeMocks.closeHandler?.({ sessionId: "session-live" });
    expect(
      await screen.findByRole("heading", { name: "録音を停止して終了しますか？" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "停止して終了" }));
    expect(bridgeMocks.resolveClose).toHaveBeenCalledWith("stopAndQuit");

    bridgeMocks.closeForceHandler?.({
      error: "停止がタイムアウトしました",
      sessionId: "session-live",
    });
    expect(
      await screen.findByRole("heading", { name: "正常に停止できませんでした" }),
    ).toBeInTheDocument();
    expect(bridgeMocks.resolveClose).not.toHaveBeenCalledWith("forceQuit");
    await user.click(screen.getByRole("button", { name: "強制終了" }));
    expect(bridgeMocks.resolveClose).toHaveBeenCalledWith("forceQuit");
  });

  it("loads the next history page with the backend cursor", async () => {
    bridgeMocks.getSnapshot.mockResolvedValue({
      ...structuredClone(mockSnapshot),
      nextCursor: "cursor-1",
    });
    bridgeMocks.listHistory.mockResolvedValue({ items: [], nextCursor: undefined });
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "さらに読み込む" }));

    expect(bridgeMocks.listHistory).toHaveBeenCalledWith(
      expect.objectContaining({ cursor: "cursor-1" }),
    );
  });

  it("consumes export progress and offers retry for failed sessions", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("option", { name: /プロジェクト定例/ }));
    await user.click(screen.getByRole("button", { name: /Export/ }));
    await user.click(screen.getByRole("button", { name: "保存先を選択…" }));
    bridgeMocks.eventHandler?.({
      event: "export.progress",
      payload: { operationId: "export-1", progress: 0.5 },
      sequence: 11,
    });

    expect(await screen.findByRole("progressbar", { name: "Exportの進捗" })).toHaveValue(0.5);
    bridgeMocks.eventHandler?.({
      event: "export.completed",
      payload: { failedSessionIds: ["session-1"], operationId: "export-1" },
      sequence: 12,
    });

    expect(await screen.findByRole("button", { name: "失敗した1件を再試行" })).toBeInTheDocument();
  });
});

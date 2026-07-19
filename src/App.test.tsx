// @vitest-environment jsdom
/* oxlint-disable no-ternary, curly, @stylistic/padding-line-between-statements */
import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import { mockSnapshot } from "./mockData";

import type { EngineEvent } from "./types";

const bridgeMocks = vi.hoisted(() => ({
  cancelExport: vi.fn(),
  cancelSession: vi.fn(),
  closeForceHandler: undefined as
    | ((payload: { error: string; sessionId: string | null }) => void)
    | undefined,
  closeHandler: undefined as ((payload: { sessionId: string }) => void) | undefined,
  deleteModel: vi.fn(),
  deleteSessions: vi.fn(),
  downloadModel: vi.fn(),
  eventHandler: undefined as ((event: EngineEvent) => void) | undefined,
  exportSessions: vi.fn().mockResolvedValue({ canceled: false, completed: true }),
  getSession: vi.fn(),
  getSnapshot: vi.fn(),
  listAudioInputs: vi.fn().mockResolvedValue([{ channels: 1, id: "7", name: "テストマイク" }]),
  listHistory: vi.fn(),
  onCloseForceRequired: vi.fn(),
  onCloseRequested: vi.fn(),
  onEngineEvent: vi.fn(),
  pickAudioFile: vi.fn().mockResolvedValue({ inputName: "test.wav", inputToken: "token" }),
  resolveClose: vi.fn(),
  searchHistory: vi.fn(),
  startSession: vi.fn().mockResolvedValue({ sessionId: "new-session" }),
  stopSession: vi.fn(),
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
  Element.prototype.scrollTo = vi.fn();
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
  bridgeMocks.cancelSession.mockClear();
  bridgeMocks.deleteSessions.mockClear();
  bridgeMocks.exportSessions.mockClear();
  bridgeMocks.stopSession.mockClear();
});

afterEach(() => {
  cleanup();
});

async function renderLoadedApp() {
  render(<App />);
  await screen.findByRole("heading", { name: "新しい録音" });
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
        segment: {
          endMs: 92_000,
          id: "new-segment",
          sequence: 4,
          startMs: 88_000,
          text: "新しい発言",
        },
      },
      sessionId: "session-live",
    });

    expect(screen.getByRole("heading", { name: "プロジェクト定例" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /処理中へ戻る/ })).toBeInTheDocument();
  });

  it("sends Stop and Cancel for the active session", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: /Stop/ }));
    await waitFor(() => expect(bridgeMocks.stopSession).toHaveBeenCalledWith("session-live"));

    cleanup();
    await renderLoadedApp();
    await user.click(screen.getByRole("button", { name: "中断" }));
    await waitFor(() => expect(bridgeMocks.cancelSession).toHaveBeenCalledWith("session-live"));
  });

  it("requires confirmation before permanently deleting a completed session", async () => {
    const user = userEvent.setup();

    await renderLoadedApp();
    await user.click(screen.getByRole("option", { name: /プロジェクト定例/ }));
    await user.click(screen.getByLabelText("その他の操作"));
    await user.click(screen.getByRole("button", { name: "完全に削除…" }));

    expect(screen.getByText(/この操作は取り消せません/)).toBeInTheDocument();
    expect(bridgeMocks.deleteSessions).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "完全に削除" }));
    await waitFor(() => expect(bridgeMocks.deleteSessions).toHaveBeenCalledWith(["session-1"]));
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
    });

    expect(await screen.findByRole("progressbar", { name: "Exportの進捗" })).toHaveValue(0.5);
    bridgeMocks.eventHandler?.({
      event: "export.completed",
      payload: { failedSessionIds: ["session-1"], operationId: "export-1" },
    });

    expect(await screen.findByRole("button", { name: "失敗した1件を再試行" })).toBeInTheDocument();
  });
});

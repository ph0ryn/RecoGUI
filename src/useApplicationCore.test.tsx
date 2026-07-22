// @vitest-environment jsdom
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useApplicationCore, type ApplicationCoreSubscriptions } from "./useApplicationCore";

interface Snapshot {
  sequence: string;
  value: string;
}

interface Event {
  sequence: string;
  type: string;
}

function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (error: unknown) => void;
} {
  let resolvePromise: (value: T) => void = () => undefined;
  let rejectPromise: (error: unknown) => void = () => undefined;
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve;
    rejectPromise = reject;
  });

  return { promise, reject: rejectPromise, resolve: resolvePromise };
}

afterEach(cleanup);

function renderHarness(
  subscriptions: ApplicationCoreSubscriptions<Snapshot, Event>,
  handlers: {
    onSnapshot: (snapshot: Snapshot) => void;
    onEvent: (event: Event) => void;
    onError: (error: unknown) => void;
  },
) {
  function Harness() {
    useApplicationCore(handlers, subscriptions);

    return null;
  }

  return render(<Harness />);
}

describe("useApplicationCore", () => {
  it("subscribes before fetching and applies buffered contiguous events", async () => {
    let emit: (event: Event) => void = () => undefined;
    const snapshot = deferred<Snapshot>();
    const onSnapshot = vi.fn();
    const onEvent = vi.fn();
    const onError = vi.fn();
    const unlisten = vi.fn();
    const subscriptions: ApplicationCoreSubscriptions<Snapshot, Event> = {
      getSnapshot: vi.fn(() => snapshot.promise),
      subscribe: vi.fn((handler) => {
        emit = handler;

        return Promise.resolve(unlisten);
      }),
    };

    renderHarness(subscriptions, { onError, onEvent, onSnapshot });
    expect(subscriptions.subscribe).toHaveBeenCalledOnce();
    expect(subscriptions.getSnapshot).not.toHaveBeenCalled();

    await act(async () => {
      await Promise.resolve();
    });

    expect(subscriptions.getSnapshot).toHaveBeenCalledOnce();

    emit({ sequence: "2", type: "second" });
    emit({ sequence: "3", type: "third" });

    await act(async () => {
      snapshot.resolve({ sequence: "1", value: "initial" });
      await Promise.resolve();
    });

    expect(onSnapshot).toHaveBeenCalledWith({ sequence: "1", value: "initial" });

    expect(onEvent.mock.calls.map(([event]) => event)).toEqual([
      { sequence: "2", type: "second" },
      { sequence: "3", type: "third" },
    ]);

    expect(onError).not.toHaveBeenCalled();
  });

  it("refreshes the snapshot on a gap before applying buffered events", async () => {
    let emit: (event: Event) => void = () => undefined;
    const firstSnapshot = deferred<Snapshot>();
    const onSnapshot = vi.fn();
    const onEvent = vi.fn();
    const onError = vi.fn();
    const subscriptions: ApplicationCoreSubscriptions<Snapshot, Event> = {
      getSnapshot: vi
        .fn<() => Promise<Snapshot>>()
        .mockImplementationOnce(() => firstSnapshot.promise)
        .mockResolvedValueOnce({ sequence: "2", value: "refreshed" }),
      subscribe: (handler) => {
        emit = handler;

        return Promise.resolve(() => undefined);
      },
    };

    renderHarness(subscriptions, { onError, onEvent, onSnapshot });

    await act(async () => {
      await Promise.resolve();
    });

    emit({ sequence: "3", type: "after-gap" });

    await act(async () => {
      firstSnapshot.resolve({ sequence: "1", value: "stale" });
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(subscriptions.getSnapshot).toHaveBeenCalledTimes(2);
    expect(onSnapshot.mock.calls.map(([snapshot]) => snapshot.sequence)).toEqual(["1", "2"]);
    expect(onEvent).toHaveBeenCalledWith({ sequence: "3", type: "after-gap" });
    expect(onError).not.toHaveBeenCalled();
  });

  it("cleans up a subscription that resolves after unmount", async () => {
    const subscription = deferred<() => void>();
    const unlisten = vi.fn();
    const snapshot = deferred<Snapshot>();
    const getSnapshot = vi.fn(() => snapshot.promise);
    const subscriptions: ApplicationCoreSubscriptions<Snapshot, Event> = {
      getSnapshot,
      subscribe: () => subscription.promise,
    };
    const rendered = renderHarness(subscriptions, {
      onError: vi.fn(),
      onEvent: vi.fn(),
      onSnapshot: vi.fn(),
    });

    expect(getSnapshot).not.toHaveBeenCalled();

    rendered.unmount();

    await act(async () => {
      subscription.resolve(unlisten);
      snapshot.resolve({ sequence: "0", value: "disposed" });
      await Promise.resolve();
    });

    expect(unlisten).toHaveBeenCalledOnce();
    expect(getSnapshot).not.toHaveBeenCalled();
  });

  it("reports subscription errors without fetching a snapshot", async () => {
    const subscriptionError = new Error("subscription failed");
    const onError = vi.fn();
    const getSnapshot = vi.fn();
    const subscriptions: ApplicationCoreSubscriptions<Snapshot, Event> = {
      getSnapshot,
      subscribe: () => Promise.reject(subscriptionError),
    };

    renderHarness(subscriptions, {
      onError,
      onEvent: vi.fn(),
      onSnapshot: vi.fn(),
    });

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onError).toHaveBeenCalledWith(subscriptionError);
    expect(getSnapshot).not.toHaveBeenCalled();
  });

  it("reports snapshot errors after a successful subscription", async () => {
    const snapshotError = new Error("snapshot failed");
    const onError = vi.fn();
    const subscriptions: ApplicationCoreSubscriptions<Snapshot, Event> = {
      getSnapshot: () => Promise.reject(snapshotError),
      subscribe: () => Promise.resolve(() => undefined),
    };

    renderHarness(subscriptions, {
      onError,
      onEvent: vi.fn(),
      onSnapshot: vi.fn(),
    });

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onError).toHaveBeenCalledWith(snapshotError);
  });
});

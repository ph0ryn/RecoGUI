import { act, cleanup, render } from "@testing-library/react";
// @vitest-environment jsdom
import { StrictMode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  useEngineEvents,
  type EngineEventHandlers,
  type EngineEventSubscriptions,
} from "./useEngineEvents";

interface DeferredSubscription {
  promise: Promise<() => void>;
  resolve: () => void;
  unlisten: ReturnType<typeof vi.fn>;
}

function deferredSubscription(): DeferredSubscription {
  const unlisten = vi.fn();
  let resolvePromise: (unlisten: () => void) => void = () => undefined;
  const promise = new Promise<() => void>((resolve) => {
    resolvePromise = resolve;
  });

  return {
    promise,
    resolve: () => resolvePromise(unlisten),
    unlisten,
  };
}

afterEach(cleanup);

describe("useEngineEvents", () => {
  it("cleans up subscriptions that resolve after StrictMode disposal", async () => {
    const subscriptionsCreated: DeferredSubscription[] = [];
    const subscribe = () => {
      const deferred = deferredSubscription();

      subscriptionsCreated.push(deferred);

      return deferred.promise;
    };
    const subscriptions: EngineEventSubscriptions = {
      onCloseForceRequired: subscribe,
      onCloseRequested: subscribe,
      onEngineEvent: subscribe,
    };
    const handlers: EngineEventHandlers = {
      onCloseForceRequired: vi.fn(),
      onCloseRequested: vi.fn(),
      onEngineEvent: vi.fn(),
      onSubscriptionError: vi.fn(),
    };

    function Harness() {
      useEngineEvents(handlers, subscriptions);

      return null;
    }

    const rendered = render(
      <StrictMode>
        <Harness />
      </StrictMode>,
    );

    expect(subscriptionsCreated).toHaveLength(6);

    await act(async () => {
      for (const deferred of subscriptionsCreated) {
        deferred.resolve();
      }

      await Promise.resolve();
    });

    expect(
      subscriptionsCreated.slice(0, 3).every(({ unlisten }) => unlisten.mock.calls.length === 1),
    ).toBe(true);

    expect(
      subscriptionsCreated.slice(3).every(({ unlisten }) => unlisten.mock.calls.length === 0),
    ).toBe(true);

    rendered.unmount();

    expect(subscriptionsCreated.every(({ unlisten }) => unlisten.mock.calls.length === 1)).toBe(
      true,
    );
  });

  it("cleans up a subscription when unmounted before it resolves", async () => {
    const deferred = deferredSubscription();
    const subscriptions: EngineEventSubscriptions = {
      onCloseForceRequired: () => Promise.resolve(() => undefined),
      onCloseRequested: () => Promise.resolve(() => undefined),
      onEngineEvent: () => deferred.promise,
    };

    function Harness() {
      useEngineEvents(
        {
          onCloseForceRequired: vi.fn(),
          onCloseRequested: vi.fn(),
          onEngineEvent: vi.fn(),
          onSubscriptionError: vi.fn(),
        },
        subscriptions,
      );

      return null;
    }

    const rendered = render(<Harness />);

    rendered.unmount();

    await act(async () => {
      deferred.resolve();
      await Promise.resolve();
    });

    expect(deferred.unlisten).toHaveBeenCalledOnce();
  });

  it("reports a subscription rejection without an unhandled promise", async () => {
    const error = new Error("listen failed");
    const onSubscriptionError = vi.fn();
    const subscriptions: EngineEventSubscriptions = {
      onCloseForceRequired: () => Promise.resolve(() => undefined),
      onCloseRequested: () => Promise.reject(error),
      onEngineEvent: () => Promise.resolve(() => undefined),
    };

    function Harness() {
      useEngineEvents(
        {
          onCloseForceRequired: vi.fn(),
          onCloseRequested: vi.fn(),
          onEngineEvent: vi.fn(),
          onSubscriptionError,
        },
        subscriptions,
      );

      return null;
    }

    render(<Harness />);

    await act(async () => Promise.resolve());

    expect(onSubscriptionError).toHaveBeenCalledWith(error);
  });
});

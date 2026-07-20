import { useEffect, useRef } from "react";

import { recoBridge } from "./bridge";

import type { EngineEvent } from "./types";

type Unlisten = () => void;

export interface EngineEventHandlers {
  onCloseForceRequired: (payload: { error: string; sessionId: string | null }) => void;
  onCloseRequested: (payload: { sessionId: string }) => void;
  onEngineEvent: (event: EngineEvent) => void;
  onSubscriptionError: (error: unknown) => void;
}

export interface EngineEventSubscriptions {
  onCloseForceRequired: (handler: EngineEventHandlers["onCloseForceRequired"]) => Promise<Unlisten>;
  onCloseRequested: (handler: EngineEventHandlers["onCloseRequested"]) => Promise<Unlisten>;
  onEngineEvent: (handler: EngineEventHandlers["onEngineEvent"]) => Promise<Unlisten>;
}

export function useEngineEvents(
  handlers: EngineEventHandlers,
  subscriptions: EngineEventSubscriptions = recoBridge,
): void {
  const handlersRef = useRef(handlers);

  handlersRef.current = handlers;

  useEffect(() => {
    let disposed = false;
    const unlisteners: Unlisten[] = [];

    const subscribe = async (subscription: Promise<Unlisten>): Promise<void> => {
      const unlisten = await subscription.catch((error: unknown) => {
        if (!disposed) {
          handlersRef.current.onSubscriptionError(error);
        }

        return undefined;
      });

      if (!unlisten) {
        return;
      }

      if (disposed) {
        unlisten();
      } else {
        unlisteners.push(unlisten);
      }
    };

    void subscribe(
      subscriptions.onEngineEvent((event) => handlersRef.current.onEngineEvent(event)),
    );

    void subscribe(
      subscriptions.onCloseRequested((payload) => handlersRef.current.onCloseRequested(payload)),
    );

    void subscribe(
      subscriptions.onCloseForceRequired((payload) =>
        handlersRef.current.onCloseForceRequired(payload),
      ),
    );

    return () => {
      disposed = true;

      for (const unlisten of unlisteners) {
        unlisten();
      }

      unlisteners.length = 0;
    };
  }, [subscriptions]);
}

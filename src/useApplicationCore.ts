import { useEffect, useRef } from "react";

import {
  compareDecimalStrings,
  eventsAfterSnapshot,
  incrementDecimalString,
  type SequencedEvent,
} from "./eventSequence";

type Unlisten = () => void;

export interface ApplicationCoreSubscriptions<
  Snapshot extends SequencedEvent,
  Event extends SequencedEvent,
> {
  subscribe: (handler: (event: Event) => void) => Promise<Unlisten>;
  getSnapshot: () => Promise<Snapshot>;
}

export interface ApplicationCoreHandlers<
  Snapshot extends SequencedEvent,
  Event extends SequencedEvent,
> {
  onSnapshot: (snapshot: Snapshot) => void;
  onEvent: (event: Event) => void;
  onError: (error: unknown) => void;
}

/**
 * Subscribe to the single ApplicationCore event stream before taking a snapshot.
 *
 * Events received while the snapshot request is in flight are retained, then only the
 * contiguous events newer than the snapshot sequence are delivered. A sequence gap causes a
 * second snapshot request before any buffered event is applied.
 */
export function useApplicationCore<Snapshot extends SequencedEvent, Event extends SequencedEvent>(
  handlers: ApplicationCoreHandlers<Snapshot, Event>,
  subscriptions: ApplicationCoreSubscriptions<Snapshot, Event>,
): void {
  const handlersRef = useRef(handlers);

  handlersRef.current = handlers;

  useEffect(() => {
    let disposed = false;
    let unlisten: Unlisten | undefined = undefined;
    let currentSequence: string | undefined = undefined;
    let synchronizing = false;
    let syncRequested = false;
    const buffered = new Map<string, Event>();

    const isDisposed = (): boolean => disposed;

    const reportError = (error: unknown): void => {
      if (!disposed) {
        handlersRef.current.onError(error);
      }
    };

    const synchronize = async (): Promise<void> => {
      if (disposed) {
        return;
      }

      if (synchronizing) {
        syncRequested = true;

        return;
      }

      synchronizing = true;
      let gapRetries = 0;

      try {
        do {
          syncRequested = false;
          const snapshot = await subscriptions.getSnapshot();

          if (isDisposed()) {
            return;
          }

          currentSequence = snapshot.sequence;
          handlersRef.current.onSnapshot(snapshot);

          const result = eventsAfterSnapshot(snapshot.sequence, [...buffered.values()]);
          let refreshRequired = false;

          if (!result.contiguous) {
            gapRetries += 1;

            if (gapRetries >= 2) {
              throw new Error(
                "ApplicationCore event sequence gap persisted after snapshot refresh",
              );
            }

            refreshRequired = true;
          } else {
            gapRetries = 0;
            let appliedSequence = snapshot.sequence;

            for (const event of result.events) {
              if (isDisposed()) {
                return;
              }

              appliedSequence = event.sequence;
              currentSequence = appliedSequence;
              handlersRef.current.onEvent(event);
              buffered.delete(event.sequence);
            }

            currentSequence = appliedSequence;

            for (const sequence of buffered.keys()) {
              if (compareDecimalStrings(sequence, appliedSequence) <= 0) {
                buffered.delete(sequence);
              }
            }

            if (
              [...buffered.keys()].some(
                (sequence) => compareDecimalStrings(sequence, appliedSequence) > 0,
              )
            ) {
              syncRequested = true;
            }
          }

          if (refreshRequired) {
            syncRequested = true;
          }
        } while (syncRequested && !isDisposed());
      } catch (error: unknown) {
        reportError(error);
      } finally {
        synchronizing = false;
      }
    };

    const onEvent = (event: Event): void => {
      if (disposed) {
        return;
      }

      try {
        if (currentSequence !== undefined && !synchronizing) {
          const expectedSequence = incrementDecimalString(currentSequence);
          const comparison = compareDecimalStrings(event.sequence, currentSequence);

          if (comparison <= 0) {
            return;
          }

          if (event.sequence === expectedSequence) {
            currentSequence = event.sequence;
            handlersRef.current.onEvent(event);

            return;
          }
        }

        buffered.set(event.sequence, event);

        if (!synchronizing) {
          void synchronize();
        } else {
          syncRequested = true;
        }
      } catch (error: unknown) {
        reportError(error);
      }
    };

    const start = async (): Promise<void> => {
      try {
        const resolvedUnlisten = await subscriptions.subscribe(onEvent);

        if (disposed) {
          resolvedUnlisten();

          return;
        }

        unlisten = resolvedUnlisten;
        await synchronize();
      } catch (error: unknown) {
        reportError(error);
      }
    };

    void start();

    return () => {
      disposed = true;
      syncRequested = false;
      buffered.clear();
      unlisten?.();
      unlisten = undefined;
    };
  }, [subscriptions]);
}

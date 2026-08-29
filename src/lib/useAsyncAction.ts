import { useCallback, useEffect, useRef, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * Non-blocking action hook — the frontend NEVER freezes.
 *
 * Pattern:
 *   1. User clicks → UI immediately shows pending state on that specific item
 *   2. Backend does the work (however long it takes)
 *   3. Backend emits event when done
 *   4. Frontend updates with real result (success or honest failure)
 *   5. Rest of app stays fully interactive the entire time
 *
 * IMPORTANT: This is the REQUIRED pattern for all future features.
 * Never trade honesty for responsiveness — both are required together.
 * - Never show "success" before the backend confirms it
 * - Always show honest pending/in-progress state
 * - Always show real failure if the backend fails
 * - Keep the rest of the app fully interactive while any action is pending
 */

export type ActionState = "idle" | "pending" | "success" | "error";

interface ActionEntry {
  state: ActionState;
  error?: string;
  successMsg?: string;
}

/** How long to wait for a backend event before giving up (ms). */
const EVENT_TIMEOUT_MS = 30_000;

export function useAsyncAction() {
  const [actions, setActions] = useState<Record<string, ActionEntry>>({});
  // Keep a ref in sync so the timeout callback always sees the latest state
  // without needing `actions` in the dependency array of `fire`.
  const actionsRef = useRef(actions);
  actionsRef.current = actions;

  // Track listeners by action key — one active listener per key, not an
  // ever-growing array. Prevents memory leak when the user clicks Sync
  // multiple times (each click used to pile up a new listener that was
  // never cleaned up until component unmount).
  const listenersRef = useRef<Map<string, UnlistenFn>>(new Map());
  // Track safety-net timeouts per action key so we can clear them on
  // success or on unmount.
  const timeoutsRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(
    new Map()
  );

  // Clean up all listeners and timeouts on unmount
  useEffect(() => {
    return () => {
      for (const unlisten of listenersRef.current.values()) {
        unlisten();
      }
      listenersRef.current.clear();
      for (const t of timeoutsRef.current.values()) {
        clearTimeout(t);
      }
      timeoutsRef.current.clear();
    };
  }, []);

  const updateAction = useCallback((key: string, entry: ActionEntry) => {
    setActions((prev) => ({ ...prev, [key]: entry }));
  }, []);

  /** Clear the safety-net timeout for a given action key. */
  const clearSafetyTimeout = useCallback((key: string) => {
    const t = timeoutsRef.current.get(key);
    if (t) {
      clearTimeout(t);
      timeoutsRef.current.delete(key);
    }
  }, []);

  /**
   * Fire a backend action without blocking the UI.
   *
   * @param key - Unique identifier for this action (e.g., "configure-pg", "sync-now")
   * @param fn - The async function to call (the invoke())
   * @param options.successEvent - Tauri event name to listen for (backend pushes result)
   * @param options.successMsg - Message to show on success
   * @param options.onError - Optional error callback
   * @param options.onSuccess - Optional success callback (receives event payload)
   * @param options.refresh - Optional function to call after success (re-fetch data)
   */
  const fire = useCallback(
    (
      key: string,
      fn: () => Promise<unknown>,
      options?: {
        successEvent?: string;
        errorEvent?: string;
        successMsg?: string;
        onError?: (error: string) => void;
        onSuccess?: (data: unknown) => void;
        refresh?: () => void;
      }
    ) => {
      // Immediately show pending state — user sees something happened
      updateAction(key, { state: "pending" });

      // If there's a success event, register the listener FIRST so the
      // backend emit can never race ahead of the JS listener attachment.
      // This prevents the "Syncing…" button getting stuck forever when
      // the spawned thread finishes before the listener is ready.
      if (options?.successEvent) {
        // Clean up any previous listener for this key before adding a new one.
        // Without this, clicking Sync 3x creates 3 listeners that all fire
        // and call refresh(), causing duplicate state updates.
        const prev = listenersRef.current.get(key);
        if (prev) {
          prev();
          listenersRef.current.delete(key);
        }

        // Also clear any existing safety-net timeout for this key.
        clearSafetyTimeout(key);

        listen(options.successEvent, (event) => {
          // Event arrived — cancel the safety-net timeout.
          clearSafetyTimeout(key);

          // Check if the backend reported an error in the payload.
          // e.g. pg-sync-done may carry { pushed: 0, error: "worker busy" }
          const payload = event.payload as Record<string, unknown> | null;
          const backendError = payload?.error;
          const pushed = typeof payload?.pushed === "number" ? payload.pushed : undefined;
          if (backendError && (pushed === undefined || pushed === 0)) {
            updateAction(key, { state: "error", error: String(backendError) });
            options?.onError?.(String(backendError));
            options?.refresh?.();
            const unlisten = listenersRef.current.get(key);
            if (unlisten) {
              unlisten();
              listenersRef.current.delete(key);
            }
            setTimeout(() => updateAction(key, { state: "idle" }), 6000);
            return;
          }

          updateAction(key, { state: "success", successMsg: options.successMsg });
          options.onSuccess?.(event.payload);
          options.refresh?.();
          // Unlisten — we got what we came for.
          const unlisten = listenersRef.current.get(key);
          if (unlisten) {
            unlisten();
            listenersRef.current.delete(key);
          }
          // Auto-clear success state after 4 seconds
          setTimeout(() => updateAction(key, { state: "idle" }), 4000);
        }).then((unlisten) => {
          listenersRef.current.set(key, unlisten);
        });

        // Listen for a dedicated error event from the backend (e.g. "pg-config-error").
        // This covers cases where the backend emits a separate error event instead
        // of piggybacking on the success event.
        if (options?.errorEvent) {
          const errorEventKey = `${key}__error`;
          const prevErr = listenersRef.current.get(errorEventKey);
          if (prevErr) {
            prevErr();
            listenersRef.current.delete(errorEventKey);
          }
          listen(options.errorEvent, (event) => {
            clearSafetyTimeout(key);
            const payload = event.payload as Record<string, unknown> | null;
            const errMsg = payload?.error ? String(payload.error) : "Operation failed";
            updateAction(key, { state: "error", error: errMsg });
            options?.onError?.(errMsg);
            options?.refresh?.();
            // Clean up both listeners
            for (const k of [key, errorEventKey]) {
              const ul = listenersRef.current.get(k);
              if (ul) { ul(); listenersRef.current.delete(k); }
            }
            setTimeout(() => updateAction(key, { state: "idle" }), 6000);
          }).then((unlisten) => {
            listenersRef.current.set(errorEventKey, unlisten);
          });
        }

        // Safety-net timeout: if the backend event never fires (thread
        // crashed, emit missed, etc.), resolve the action so the button
        // doesn't stay stuck "Syncing…" forever.
        const timer = setTimeout(() => {
          // Only resolve if still pending (user may have clicked something else).
          if (actionsRef.current[key]?.state === "pending") {
            const unlisten = listenersRef.current.get(key);
            if (unlisten) {
              unlisten();
              listenersRef.current.delete(key);
            }
            // Also clean up any error event listener
            const errKey = `${key}__error`;
            const errUl = listenersRef.current.get(errKey);
            if (errUl) { errUl(); listenersRef.current.delete(errKey); }
            timeoutsRef.current.delete(key);
            updateAction(key, {
              state: "error",
              error: "Sync timed out — the backend did not respond within 30 seconds.",
            });
            options?.refresh?.();
            setTimeout(() => updateAction(key, { state: "idle" }), 6000);
          }
        }, EVENT_TIMEOUT_MS);
        timeoutsRef.current.set(key, timer);
      }

      // Fire the backend command — does NOT block the UI
      fn()
        .then(() => {
          // If no success event expected, resolve immediately
          if (!options?.successEvent) {
            updateAction(key, { state: "success", successMsg: options?.successMsg });
            options?.onSuccess?.(null);
            options?.refresh?.();
            setTimeout(() => updateAction(key, { state: "idle" }), 4000);
          }
        })
        .catch((e) => {
          // Command failed to dispatch (e.g., network error before backend received it)
          clearSafetyTimeout(key);
          const msg = String(e);
          updateAction(key, { state: "error", error: msg });
          options?.onError?.(msg);
          // Auto-clear error after 6 seconds
          setTimeout(() => updateAction(key, { state: "idle" }), 6000);
        });
    },
    [updateAction, clearSafetyTimeout]
  );

  const getState = useCallback(
    (key: string): ActionState => actions[key]?.state ?? "idle",
    [actions]
  );

  const getError = useCallback(
    (key: string): string | undefined => actions[key]?.error,
    [actions]
  );

  const getSuccess = useCallback(
    (key: string): string | undefined => actions[key]?.successMsg,
    [actions]
  );

  const isPending = useCallback(
    (key: string): boolean => actions[key]?.state === "pending",
    [actions]
  );

  return { fire, getState, getError, getSuccess, isPending, actions };
}

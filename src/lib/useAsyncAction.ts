import { useCallback, useRef, useState } from "react";
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

export function useAsyncAction() {
  const [actions, setActions] = useState<Record<string, ActionEntry>>({});
  const listenersRef = useRef<UnlistenFn[]>([]);

  const updateAction = useCallback((key: string, entry: ActionEntry) => {
    setActions((prev) => ({ ...prev, [key]: entry }));
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

      // Fire the backend command — does NOT block the UI
      fn()
        .then(() => {
          // Command dispatched successfully (backend accepted the request)
          // If there's a success event, wait for it
          if (options?.successEvent) {
            // Listen for the backend's result event
            listen(options.successEvent, (event) => {
              updateAction(key, { state: "success", successMsg: options.successMsg });
              options.onSuccess?.(event.payload);
              options.refresh?.();
              // Auto-clear success state after 4 seconds
              setTimeout(() => updateAction(key, { state: "idle" }), 4000);
            }).then((unlisten) => {
              listenersRef.current.push(unlisten);
            });
          } else {
            // No event expected — show success immediately
            updateAction(key, { state: "success", successMsg: options?.successMsg });
            options?.onSuccess?.(null);
            options?.refresh?.();
            setTimeout(() => updateAction(key, { state: "idle" }), 4000);
          }
        })
        .catch((e) => {
          // Command failed to dispatch (e.g., network error before backend received it)
          const msg = String(e);
          updateAction(key, { state: "error", error: msg });
          options?.onError?.(msg);
          // Auto-clear error after 6 seconds
          setTimeout(() => updateAction(key, { state: "idle" }), 6000);
        });
    },
    [updateAction]
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

import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "../lib/api";

type SetupStep =
  | "checking"
  | "downloading_python"
  | "python_downloaded"
  | "extracting_python"
  | "installing_pip"
  | "pip_installed"
  | "installing_deps"
  | "python_found"
  | "complete"
  | "error";

interface SetupProgress {
  step: SetupStep;
  message: string;
  progress?: number;
  total?: number;
  package?: string;
}

const STEP_ORDER: SetupStep[] = [
  "checking",
  "python_found",
  "downloading_python",
  "python_downloaded",
  "extracting_python",
  "installing_pip",
  "pip_installed",
  "installing_deps",
  "complete",
];

function stepIndex(s: SetupStep): number {
  const idx = STEP_ORDER.indexOf(s);
  return idx >= 0 ? idx : 0;
}

export default function AnprSetupWizard({
  onComplete,
  onSkip,
}: {
  onComplete: () => void;
  onSkip: () => void;
}) {
  const [step, setStep] = useState<SetupStep>("checking");
  const [message, setMessage] = useState("Checking ANPR requirements...");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const unlistenProgress = listen("anpr-setup-progress", (event) => {
      const p = event.payload as SetupProgress;
      setStep(p.step);
      setMessage(p.message);
    });

    const unlistenDone = listen("anpr-setup-done", () => {
      setStep("complete");
      setMessage("ANPR environment ready!");
      setTimeout(onComplete, 1500);
    });

    const unlistenError = listen("anpr-setup-error", (event) => {
      const p = event.payload as { error: string };
      setStep("error");
      setError(p.error);
    });

    // Trigger the setup
    api.ensureAnprSetup().catch(() => {});

    return () => {
      unlistenProgress.then((f) => f());
      unlistenDone.then((f) => f());
      unlistenError.then((f) => f());
    };
  }, [onComplete]);

  const currentIdx = stepIndex(step);
  const isComplete = step === "complete";
  const isError = step === "error";
  const isRunning = !isComplete && !isError;

  const steps: Array<{ key: SetupStep; label: string }> = [
    { key: "checking", label: "Check requirements" },
    { key: "python_found", label: "Python" },
    { key: "installing_deps", label: "Install packages" },
    { key: "complete", label: "Ready" },
  ];

  return (
    <div className="overlay" style={{ zIndex: 1000 }}>
      <div
        className="modal"
        style={{ maxWidth: 520, width: "90%" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="card stack" style={{ padding: 28 }}>
          <h2 style={{ margin: 0, fontSize: 18 }}>ANPR Engine Setup</h2>
          <p className="muted small" style={{ margin: 0 }}>
            The ANPR engine needs Python and OCR packages. This is a one-time setup (~300 MB download).
          </p>

          {/* Step indicators */}
          <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 12 }}>
            {steps.map((s) => {
              const isDone = currentIdx > stepIndex(s.key);
              const isCurrent = step === s.key || (s.key === "installing_deps" && currentIdx >= stepIndex("installing_deps") && !isComplete);
              return (
                <div
                  key={s.key}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 10,
                    padding: "6px 10px",
                    borderRadius: 6,
                    background: isCurrent ? "var(--surface-2, #1a1a2e)" : "transparent",
                  }}
                >
                  <span style={{ fontSize: 14, width: 20, textAlign: "center" }}>
                    {isDone ? "✓" : isCurrent && isRunning ? "⟳" : isComplete ? "✓" : "○"}
                  </span>
                  <span
                    style={{
                      fontSize: 13,
                      fontWeight: isCurrent ? 600 : 400,
                      opacity: isDone || isCurrent ? 1 : 0.5,
                    }}
                  >
                    {s.label}
                  </span>
                </div>
              );
            })}
          </div>

          {/* Current status message */}
          <div
            style={{
              padding: "10px 14px",
              borderRadius: 6,
              background: isError
                ? "color-mix(in srgb, var(--danger, #ef4444) 10%, transparent)"
                : isComplete
                  ? "color-mix(in srgb, var(--success, #22c55e) 10%, transparent)"
                  : "var(--surface-2, #1a1a2e)",
              fontSize: 13,
              color: isError ? "var(--danger, #ef4444)" : "inherit",
            }}
          >
            {isError ? `Error: ${error}` : message}
          </div>

          {/* Actions */}
          <div className="row" style={{ gap: 8, justifyContent: "flex-end", marginTop: 8 }}>
            {isRunning && (
              <button className="ghost" onClick={onSkip}>
                Skip for now
              </button>
            )}
            {isError && (
              <>
                <button className="ghost" onClick={onSkip}>
                  Skip for now
                </button>
                <button
                  className="small"
                  onClick={() => {
                    setError(null);
                    setStep("checking");
                    setMessage("Retrying...");
                    api.ensureAnprSetup().catch(() => {});
                  }}
                >
                  Retry
                </button>
              </>
            )}
            {isComplete && (
              <span className="badge active" style={{ fontSize: 12 }}>
                Ready!
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

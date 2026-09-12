import { Component, type ReactNode } from "react";

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
  onReset?: () => void;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export default class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false, error: null };

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error("[ErrorBoundary]", error, info.componentStack);
  }

  reset = () => {
    this.setState({ hasError: false, error: null });
    this.props.onReset?.();
  };

  render() {
    if (this.state.hasError) {
      if (this.props.fallback) return this.props.fallback;
      return (
        <div className="card stack" style={{ margin: 10, padding: 20 }}>
          <div className="section-title" style={{ color: "var(--error, #d32f2f)" }}>
            Something went wrong
          </div>
          <p className="muted small">
            {this.state.error?.message ?? "An unexpected error occurred."}
          </p>
          <button className="primary" onClick={this.reset} style={{ alignSelf: "flex-start" }}>
            Try again
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

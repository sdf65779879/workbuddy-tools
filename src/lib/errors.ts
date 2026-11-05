import { useApp } from "../stores/appStore";

/** All execution failures share the same dismissible, viewport-centered dialog. */
export function reportError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  useApp.setState({ error: message, notice: null });
}

export function clearExecutionError() {
  useApp.getState().clearError();
}

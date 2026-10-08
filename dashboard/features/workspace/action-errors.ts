// Errors from actions the user started, kept per source so that clearing one
// (a retry, a new attempt, a reconnecting stream) never hides another.
export type ActionErrorSource = "remote" | "delete" | "close-session";

export type ActionErrors = Partial<Record<ActionErrorSource, string>>;

export type ActionErrorUpdate = { source: ActionErrorSource; message: string | null };

const DISPLAY_ORDER: readonly ActionErrorSource[] = ["remote", "close-session", "delete"];

export function actionErrorsReducer(state: ActionErrors, { source, message }: ActionErrorUpdate): ActionErrors {
  if (message === null) {
    if (state[source] === undefined) return state;
    const next = { ...state };
    delete next[source];
    return next;
  }
  if (state[source] === message) return state;
  return { ...state, [source]: message };
}

export function listActionErrors(state: ActionErrors, sources: readonly ActionErrorSource[] = DISPLAY_ORDER) {
  return sources.flatMap((source) => {
    const message = state[source];
    return message === undefined ? [] : [{ source, message }];
  });
}

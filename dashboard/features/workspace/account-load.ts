// Loads the company account, which carries the dashboard's idle policy, and
// keeps retrying transient failures so the dashboard cannot stay unloaded.
export class AccountLoadError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

const MAX_RETRY_DELAY_MS = 30_000;

// A 401 locks the dashboard before it reaches here. Timeouts, rate limits,
// server errors and network failures are transient; other responses, such as
// a suspended company, need someone to act first.
export function isRetryableAccountError(error: unknown) {
  if (error instanceof AccountLoadError) {
    return error.status === 408 || error.status === 429 || error.status >= 500;
  }
  return error instanceof TypeError;
}

export function accountRetryDelay(attempt: number) {
  return Math.min(1_000 * 2 ** attempt, MAX_RETRY_DELAY_MS);
}

type Options<T> = {
  load: () => Promise<T>;
  onLoaded: (value: T) => void;
  // `retryInMs` is null when the failure is not retried automatically.
  onError: (error: unknown, retryInMs: number | null) => void;
};

export function accountLoader<T>({ load, onLoaded, onError }: Options<T>) {
  let stopped = false;
  let running = false;
  let attempt = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const run = async () => {
    timer = undefined;
    running = true;
    try {
      const value = await load();
      if (!stopped) onLoaded(value);
    } catch (error) {
      if (stopped) return;
      const retryInMs = isRetryableAccountError(error) ? accountRetryDelay(attempt++) : null;
      onError(error, retryInMs);
      if (retryInMs !== null) timer = setTimeout(() => { void run(); }, retryInMs);
    } finally {
      running = false;
    }
  };

  void run();
  return {
    retry() {
      if (stopped || running) return;
      if (timer !== undefined) clearTimeout(timer);
      attempt = 0;
      void run();
    },
    stop() {
      stopped = true;
      if (timer !== undefined) clearTimeout(timer);
    },
  };
}

// Reads something again when asked, however often that is: the asks during
// `intervalMs` become one read at its end, a read never starts while
// another runs, and one asked for while it ran follows `intervalMs` after
// it. Nothing is read while `hidden()`: a read asked for meanwhile waits for
// `shown()`. No imports, so node:test can load this module directly.
export type ReloadGate = {
  request: () => void;
  // The tab became visible again.
  shown: () => void;
  stop: () => void;
};

export function reloadGate({ reload, intervalMs, hidden }: {
  reload: () => Promise<unknown>;
  intervalMs: number;
  hidden: () => boolean;
}): ReloadGate {
  let wanted = false;
  let running = false;
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const arm = () => {
    if (!wanted || running || stopped || timer !== undefined || hidden()) return;
    timer = setTimeout(fire, intervalMs);
  };

  const fire = () => {
    timer = undefined;
    if (stopped || hidden()) return;
    wanted = false;
    running = true;
    void Promise.resolve()
      .then(reload)
      .catch(() => {
        // The reader reports its own failures.
      })
      .finally(() => {
        running = false;
        arm();
      });
  };

  return {
    request: () => {
      wanted = true;
      arm();
    },
    shown: arm,
    stop: () => {
      stopped = true;
      clearTimeout(timer);
      timer = undefined;
    },
  };
}

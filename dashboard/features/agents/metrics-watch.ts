// Tells the event socket which devices' live readings this tab shows, so
// the server sends those and no others. What the page shows is whatever
// subscribes to a device's reading (see DeviceStore.readers): the device
// wall's tiles in view, which it draws only of, and a device's own page.
// A hidden tab shows nothing, so it watches nothing, and asks again for
// what it shows when it is shown.
//
// A run of changes, as when the wall scrolls, sends one message, and
// messages go no more often than every WATCH_INTERVAL_MS. Each new socket
// watches nothing until told, so it is sent the set as soon as it opens.
// No React or DOM here, so node:test loads it directly.

// The most messages a tab sends about what it shows: four a second.
export const WATCH_INTERVAL_MS = 250;
// The server watches at most this many devices for a socket, and ignores
// the rest; the most tiles a wall draws is well under it.
export const MAX_WATCHED = 500;

type Timers = {
  setTimeout: (callback: () => void, ms: number) => unknown;
  clearTimeout: (timer: unknown) => void;
  now: () => number;
};

const globalTimers: Timers = {
  setTimeout: (callback, ms) => setTimeout(callback, ms),
  clearTimeout: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
};

// Sends the socket the devices it is to watch.
export type SendWatch = (devices: readonly string[]) => void;

export type MetricsWatch = {
  // What the page shows changed.
  changed(): void;
  setVisible(visible: boolean): void;
  // A socket opened, watching nothing.
  attach(send: SendWatch): void;
  // The socket closed.
  detach(): void;
  stop(): void;
};

function sameSet(left: ReadonlySet<string>, right: ReadonlySet<string>) {
  if (left.size !== right.size) return false;
  for (const id of left) if (!right.has(id)) return false;
  return true;
}

export function metricsWatch({ shown, onWatching, visible: initiallyVisible = true, timers = globalTimers }: {
  // The devices the page shows a reading of.
  shown: () => Iterable<string>;
  // The socket was told to watch these; readings of any other aren't live.
  // `added` are those it did not watch before: all of them on a new socket.
  onWatching: (devices: ReadonlySet<string>, added: string[]) => void;
  visible?: boolean;
  timers?: Timers;
}): MetricsWatch {
  let visible = initiallyVisible;
  let send: SendWatch | null = null;
  // What the current socket watches.
  let watching: ReadonlySet<string> = new Set();
  let sentAt = -Infinity;
  let timer: unknown;
  let stopped = false;

  const wanted = () => {
    const devices = new Set<string>();
    if (!visible) return devices;
    for (const id of shown()) {
      if (devices.size >= MAX_WATCHED) break;
      devices.add(id);
    }
    return devices;
  };

  const flush = () => {
    timer = undefined;
    if (!send) return;
    const next = wanted();
    if (sameSet(next, watching)) return;
    const added = [...next].filter((id) => !watching.has(id));
    watching = next;
    sentAt = timers.now();
    send([...next]);
    onWatching(next, added);
  };

  // A change sends after whatever else changes with it, as the rest of a
  // wall's tiles mounting in the same render, and no sooner than the
  // interval after the last message.
  const schedule = () => {
    if (stopped || !send || timer !== undefined) return;
    timer = timers.setTimeout(flush, Math.max(0, sentAt + WATCH_INTERVAL_MS - timers.now()));
  };

  const cancel = () => {
    if (timer === undefined) return;
    timers.clearTimeout(timer);
    timer = undefined;
  };

  return {
    changed: schedule,
    setVisible(next) {
      if (visible === next) return;
      visible = next;
      schedule();
    },
    attach(next) {
      if (stopped) return;
      cancel();
      send = next;
      watching = new Set();
      sentAt = -Infinity;
      schedule();
    },
    detach() {
      cancel();
      send = null;
    },
    stop() {
      stopped = true;
      cancel();
      send = null;
    },
  };
}

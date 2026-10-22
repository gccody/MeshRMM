import { useCallback, useEffect, useEffectEvent, useState } from "react";
import { useWorkspace } from "../workspace/workspace-context";
import type { JobEvent } from "./model";

export type JobFeed = {
  // Hands the socket's job events to the pages listening.
  handle: (event: JobEvent) => void;
  // Calls `listener` with each job event until the returned function runs.
  subscribe: (listener: (event: JobEvent) => void) => () => void;
};

// The event socket's job events, for the page beneath the shell.
export function useJobFeed(): JobFeed {
  const [listeners] = useState(() => new Set<(event: JobEvent) => void>());
  const handle = useCallback((event: JobEvent) => {
    for (const listener of listeners) listener(event);
  }, [listeners]);
  const subscribe = useCallback((listener: (event: JobEvent) => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, [listeners]);
  return { handle, subscribe };
}

// Calls `listener` with each job event while the page is open.
export function useJobEvents(listener: (event: JobEvent) => void) {
  const { jobs } = useWorkspace();
  const onEvent = useEffectEvent(listener);
  const { subscribe } = jobs;
  useEffect(() => subscribe((event) => onEvent(event)), [subscribe]);
}

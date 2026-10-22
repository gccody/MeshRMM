import { useCallback, useEffect, useEffectEvent, useState } from "react";
import { useWorkspace } from "../workspace/workspace-context";
import type { UpdateEvent } from "./model";

export type UpdateFeed = {
  // Hands the socket's update events to the pages listening.
  handle: (event: UpdateEvent) => void;
  // Calls `listener` with each update event until the returned function runs.
  subscribe: (listener: (event: UpdateEvent) => void) => () => void;
};

// The event socket's update events, for the page beneath the shell.
export function useUpdateFeed(): UpdateFeed {
  const [listeners] = useState(() => new Set<(event: UpdateEvent) => void>());
  const handle = useCallback((event: UpdateEvent) => {
    for (const listener of listeners) listener(event);
  }, [listeners]);
  const subscribe = useCallback((listener: (event: UpdateEvent) => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, [listeners]);
  return { handle, subscribe };
}

// Calls `listener` with each update event while the page is open.
export function useUpdateEvents(listener: (event: UpdateEvent) => void) {
  const { updates } = useWorkspace();
  const onEvent = useEffectEvent(listener);
  const { subscribe } = updates;
  useEffect(() => subscribe((event) => onEvent(event)), [subscribe]);
}

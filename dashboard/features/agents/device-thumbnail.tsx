import { useCallback, useEffect, useSyncExternalStore } from "react";
import { X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Thumbnail, ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

// A tile waits this long before asking for its image, so tiles that only
// scroll past ask for nothing.
const SETTLE_DELAY_MS = 150;
// After a failed load.
const RETRY_DELAY_MS = 60_000;

const formatUpdated = (date: Date | null) =>
  date?.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) ?? null;

// The device's latest screen image, as far as this page has loaded it.
function useThumbnail(store: ThumbnailStore, id: string): Thumbnail | null {
  const subscribe = useCallback((listener: () => void) => store.subscribe(id, listener), [store, id]);
  return useSyncExternalStore(subscribe, () => store.snapshot(id), () => null);
}

// The device's latest screen image, loaded when its tile is drawn and the
// page is in front, and again when the server says it changed. The wall
// draws only the tiles on screen or about to be. A load that failed is
// tried again a while later.
export function useDeviceScreen(store: ThumbnailStore, id: string): Thumbnail | null {
  const thumbnail = useThumbnail(store, id);
  useEffect(() => {
    let timer: number | undefined;
    let cancelled = false;
    const check = async () => {
      window.clearTimeout(timer);
      if (document.visibilityState !== "visible") return;
      if (await store.refresh(id) && !cancelled) timer = window.setTimeout(() => void check(), RETRY_DELAY_MS);
    };
    const onVisibilityChange = () => void check();
    timer = window.setTimeout(() => void check(), SETTLE_DELAY_MS);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [store, id]);
  return thumbnail;
}

// A device's screen image at full size, while it has one.
export function ScreenPreview({ agent, store, returnFocus, onClose }: {
  agent: Agent;
  store: ThumbnailStore;
  returnFocus: React.RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const thumbnail = useThumbnail(store, agent.id);
  if (!thumbnail) return null;
  const updated = formatUpdated(thumbnail.updatedAt);
  const titleId = `thumbnail-title-${agent.id}`;
  return (
    <ModalDialog className="settings-modal thumbnail-modal" labelledBy={titleId} returnFocus={returnFocus} onClose={onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={18} /></button>
      <h2 id={titleId}>{agent.name}</h2>
      <p>{agent.connected ? "Refreshes every 5 minutes while shown." : "Offline. This is the last image it sent."}{updated && <> Captured {updated}.</>}</p>
      <img className="thumbnail-preview" src={thumbnail.url} alt={`The main display of ${agent.name}${updated ? ` at ${updated}` : ""}`} />
    </ModalDialog>
  );
}

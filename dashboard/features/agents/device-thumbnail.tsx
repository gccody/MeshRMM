import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import { X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Thumbnail, ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

// Tiles load their images a little before they scroll into view.
const VISIBILITY_MARGIN = "200px";

const formatUpdated = (date: Date | null) =>
  date?.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) ?? null;

// Whether the element is on screen or about to be.
function useNearViewport(element: HTMLElement | null) {
  const [near, setNear] = useState(false);
  useEffect(() => {
    if (!element) return;
    const observer = new IntersectionObserver(([entry]) => setNear(entry.isIntersecting), { rootMargin: VISIBILITY_MARGIN });
    observer.observe(element);
    return () => observer.disconnect();
  }, [element]);
  return near;
}

// The device's latest screen image, kept current while its tile is visible
// and the page is in front. A hidden tab checks again as soon as it is shown.
export function useDeviceScreen(store: ThumbnailStore, agent: Agent, element: HTMLElement | null): Thumbnail | null {
  const { id, connected } = agent;
  const visible = useNearViewport(element);
  const subscribe = useCallback((listener: () => void) => store.subscribe(id, listener), [store, id]);
  const thumbnail = useSyncExternalStore(subscribe, () => store.snapshot(id), () => null);
  useEffect(() => {
    if (!visible) return;
    let timer: number | undefined;
    let cancelled = false;
    const check = async () => {
      window.clearTimeout(timer);
      if (document.visibilityState !== "visible") return;
      const delay = await store.refresh(id, connected);
      if (!cancelled && delay !== null) timer = window.setTimeout(() => void check(), delay);
    };
    const onVisibilityChange = () => void check();
    void check();
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [store, id, connected, visible]);
  return thumbnail;
}

export function ScreenPreview({ agent, thumbnail, returnFocus, onClose }: {
  agent: Agent;
  thumbnail: Thumbnail;
  returnFocus: React.RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const updated = formatUpdated(thumbnail.updatedAt);
  const titleId = `thumbnail-title-${agent.id}`;
  return (
    <ModalDialog className="settings-modal thumbnail-modal" labelledBy={titleId} returnFocus={returnFocus} onClose={onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={18} /></button>
      <h2 id={titleId}>{agent.name}</h2>
      <p>{agent.connected ? "Refreshes every 5 minutes." : "Offline. This is the last image it sent."}{updated && <> Captured {updated}.</>}</p>
      <img className="thumbnail-preview" src={thumbnail.url} alt={`The main display of ${agent.name}${updated ? ` at ${updated}` : ""}`} />
    </ModalDialog>
  );
}

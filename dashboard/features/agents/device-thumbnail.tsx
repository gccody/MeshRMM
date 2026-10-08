
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Monitor, X } from "lucide-react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Thumbnail, ThumbnailStore } from "./thumbnails";
import type { Agent } from "./types";

// Rows load their images a little before they scroll into view.
const VISIBILITY_MARGIN = "200px";

const formatUpdated = (date: Date | null) =>
  date?.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) ?? null;

// Whether the element is on screen or about to be.
function useNearViewport(element: React.RefObject<HTMLElement | null>) {
  const [near, setNear] = useState(false);
  useEffect(() => {
    const node = element.current;
    if (!node) return;
    const observer = new IntersectionObserver(([entry]) => setNear(entry.isIntersecting), { rootMargin: VISIBILITY_MARGIN });
    observer.observe(node);
    return () => observer.disconnect();
  }, [element]);
  return near;
}

// Keeps a visible row's image current while the page is in front. A hidden
// tab checks again as soon as it is shown.
function useThumbnail(store: ThumbnailStore, agent: Agent, visible: boolean): Thumbnail | null {
  const { id, connected } = agent;
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

// The device's main display in its row, or its icon until an image arrives.
// Choosing the image shows it larger.
export function DeviceThumbnail({ agent, store }: { agent: Agent; store: ThumbnailStore }) {
  const container = useRef<HTMLSpanElement>(null);
  const opener = useRef<HTMLButtonElement>(null);
  const visible = useNearViewport(container);
  const thumbnail = useThumbnail(store, agent, visible);
  const [previewOpen, setPreviewOpen] = useState(false);
  const online = agent.connected ? " online" : "";

  return (
    <span className="device-visual" ref={container}>
      {thumbnail ? (
        <button ref={opener} type="button" className={`device-thumbnail${online}`} onClick={() => setPreviewOpen(true)} aria-label={`Show the screen of ${agent.name}`} aria-haspopup="dialog">
          <img src={thumbnail.url} alt="" />
          <span />
        </button>
      ) : (
        <span className={`device-avatar${online}`}><Monitor size={20} /><span /></span>
      )}
      {previewOpen && thumbnail && <ThumbnailPreview agent={agent} thumbnail={thumbnail} returnFocus={opener} onClose={() => setPreviewOpen(false)} />}
    </span>
  );
}

function ThumbnailPreview({ agent, thumbnail, returnFocus, onClose }: {
  agent: Agent;
  thumbnail: Thumbnail;
  returnFocus: React.RefObject<HTMLButtonElement | null>;
  onClose: () => void;
}) {
  const updated = formatUpdated(thumbnail.updatedAt);
  const titleId = `thumbnail-title-${agent.id}`;
  return (
    <ModalDialog className="settings-modal thumbnail-modal" labelledBy={titleId} returnFocus={returnFocus} onClose={onClose}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <p className="eyebrow">Main display</p>
      <h2 id={titleId}>{agent.name}</h2>
      <p>
        {agent.connected ? "Updates every 5 minutes while the device is online." : "The device is offline. This is the last image it sent."}
        {updated && <> Updated {updated}.</>}
      </p>
      <img className="thumbnail-preview" src={thumbnail.url} alt={`The main display of ${agent.name}${updated ? ` at ${updated}` : ""}`} />
    </ModalDialog>
  );
}

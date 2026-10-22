import { ArrowDownToLine, ArrowUpFromLine, CircleAlert, CircleCheck, Copy, LoaderCircle, Pause, RotateCw, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { type AuthorizedFetch, AuthenticationRequired, RequestError, errorText } from "../../lib/http";
import { sha256OfBlob } from "../../lib/sha256";
import type { DevicePlatform } from "../agents/types";
import { formatBytes } from "../metrics/model";
import { cancelUpload, createFolder, createUpload, downloadProgress, finishUpload, sendUpload, startBrowserDownload, uploadStatus } from "./files-api";
import { type DownloadView, type FileAccount, joinRelative, percent, rate } from "./model";
import { type Picked, emptyFolders } from "./picked";

// Uploads that run at once; the server allows a user six transfers.
const CONCURRENT_UPLOADS = 3;
// Files up to this size are hashed by the browser's own crypto in one go.
const NATIVE_HASH_BYTES = 64 * 1024 * 1024;
// Times an upload goes on by itself after its connection drops.
const AUTOMATIC_RESUMES = 3;
// A download the browser hasn't asked for in this long didn't start.
const DOWNLOAD_START_MS = 60_000;

export type UploadState = "queued" | "uploading" | "verifying" | "done" | "failed" | "paused" | "conflict" | "cancelled";

export type UploadItem = {
  kind: "upload";
  key: number;
  name: string;
  path: string;
  // The folder it lands in, to refresh when it does.
  folder: string;
  size: number;
  account: FileAccount;
  overwrite: boolean;
  state: UploadState;
  sent: number;
  hashed: number;
  startedAt: number;
  sha256?: string;
  error?: string;
};

export type DownloadItem = {
  kind: "download";
  key: number;
  id: string;
  name: string;
  archive: boolean;
  size: number;
  files: number;
  state: "starting" | "sending" | "done" | "failed";
  bytes: number;
  offset: number;
  startedAt: number;
  sha256?: string;
  error?: string;
};

export type TransferItem = UploadItem | DownloadItem;

type Options = {
  authorizedFetch: AuthorizedFetch;
  deviceId: string;
  platform: DevicePlatform;
  connected: boolean;
  // A folder changed: an upload landed in it.
  onLanded: (folder: string) => void;
};

const hashOf = async (file: File, onProgress: (bytes: number) => void, signal: AbortSignal) => {
  if (file.size > NATIVE_HASH_BYTES) return sha256OfBlob(file, onProgress, signal);
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  onProgress(file.size);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
};

const offlineCodes = new Set(["device_offline", "device_disconnected", "device_timeout", "network"]);

// The queue of uploads and the downloads started from the page, and what
// each is doing.
export function useTransfers({ authorizedFetch, deviceId, platform, connected, onLanded }: Options) {
  const [items, setItems] = useState<TransferItem[]>([]);
  const nextKey = useRef(1);
  const files = useRef(new Map<number, File>());
  const uploads = useRef(new Map<number, string>());
  const hashes = useRef(new Map<number, Promise<string>>());
  const stops = useRef(new Map<number, AbortController>());
  const resumes = useRef(new Map<number, number>());
  const running = useRef(new Set<number>());
  const latest = useRef({ authorizedFetch, onLanded, connected, items });
  useEffect(() => {
    latest.current = { authorizedFetch, onLanded, connected, items };
  });

  const update = useCallback((key: number, change: Partial<UploadItem> | Partial<DownloadItem>) => {
    setItems((current) => current.map((item) => (item.key === key ? ({ ...item, ...change } as TransferItem) : item)));
  }, []);

  const run = useCallback(async (item: UploadItem) => {
    const { key } = item;
    const file = files.current.get(key);
    if (!file) return;
    const fetch = latest.current.authorizedFetch;
    const stop = new AbortController();
    stops.current.set(key, stop);
    update(key, { state: "uploading", error: undefined, startedAt: Date.now() });
    try {
      let id = uploads.current.get(key);
      let received = 0;
      if (id) {
        try {
          received = (await uploadStatus(fetch, id)).received;
        } catch (error) {
          // The device or server forgot it; start again.
          if (!(error instanceof RequestError && (error.code === "upload_not_found" || error.code === "not_found"))) throw error;
          uploads.current.delete(key);
          id = undefined;
        }
      }
      if (!id) {
        const created = await createUpload(fetch, deviceId, item.account, item.path, item.size, item.overwrite);
        id = created.id;
        received = created.received;
        uploads.current.set(key, id);
      }
      let hash = hashes.current.get(key);
      if (!hash) {
        hash = hashOf(file, (hashed) => update(key, { hashed }), stop.signal);
        hashes.current.set(key, hash);
        // Its failure surfaces when it's awaited below.
        hash.catch(() => hashes.current.delete(key));
      }
      update(key, { sent: received });
      if (received < item.size) {
        const done = await sendUpload(id, received, file.slice(received), (sent) => update(key, { sent: received + sent }), stop.signal);
        if (done.received !== item.size) throw new RequestError("The device didn’t get the whole file.", 409, "incomplete");
      }
      update(key, { state: "verifying", sent: item.size });
      const sha256 = await hash;
      await finishUpload(fetch, id, sha256);
      uploads.current.delete(key);
      files.current.delete(key);
      update(key, { state: "done", sha256 });
      latest.current.onLanded(item.folder);
    } catch (error) {
      if (stop.signal.aborted) return;
      const code = error instanceof RequestError ? error.code : undefined;
      if (error instanceof AuthenticationRequired) {
        update(key, { state: "paused", error: "Sign in again to go on." });
      } else if (code === "already_exists") {
        update(key, { state: "conflict", error: undefined });
      } else if (code === "checksum_mismatch") {
        uploads.current.delete(key);
        update(key, { state: "failed", error: errorText(error, "The file arrived damaged.") });
      } else if (code === "too_many_transfers") {
        update(key, { state: "queued" });
      } else if (code && offlineCodes.has(code) || code === "incomplete" || code === "upload_interrupted") {
        const tries = resumes.current.get(key) ?? 0;
        if (latest.current.connected && tries < AUTOMATIC_RESUMES) {
          resumes.current.set(key, tries + 1);
          update(key, { state: "queued", error: undefined });
        } else {
          update(key, { state: "paused", error: errorText(error, "The connection dropped.") });
        }
      } else {
        update(key, { state: "failed", error: errorText(error, "The upload failed.") });
      }
    } finally {
      stops.current.delete(key);
      running.current.delete(key);
    }
  }, [deviceId, update]);

  // Starts queued uploads while there's room.
  useEffect(() => {
    const active = items.filter((item) => item.kind === "upload" && (item.state === "uploading" || item.state === "verifying")).length;
    const room = CONCURRENT_UPLOADS - active;
    if (room <= 0 || !connected) return;
    const next = items
      .filter((item): item is UploadItem => item.kind === "upload" && item.state === "queued" && !running.current.has(item.key))
      .slice(0, room);
    for (const item of next) {
      running.current.add(item.key);
      void run(item);
    }
  }, [items, connected, run]);

  // A device back online picks up where its uploads left off.
  const [wasConnected, setWasConnected] = useState(connected);
  if (connected !== wasConnected) {
    setWasConnected(connected);
    if (connected) {
      setItems((current) =>
        current.map((item) => (item.kind === "upload" && item.state === "paused" ? { ...item, state: "queued", error: undefined } : item)),
      );
    }
  }

  // Downloads' progress, which the server follows as it relays them.
  const watching = items.some((item) => item.kind === "download" && (item.state === "starting" || item.state === "sending"));
  useEffect(() => {
    if (!watching) return;
    const timer = window.setInterval(() => {
      for (const item of latest.current.items) {
        if (item.kind !== "download" || (item.state !== "starting" && item.state !== "sending")) continue;
        downloadProgress(latest.current.authorizedFetch, item.id).then(
          ({ progress }) => {
            const state = progress.state === "ready"
              ? Date.now() - item.startedAt > DOWNLOAD_START_MS ? "failed" : "starting"
              : progress.state;
            update(item.key, {
              state,
              bytes: progress.bytes,
              offset: progress.offset,
              sha256: progress.sha256,
              error: state === "failed" ? progress.error ?? "The browser didn’t start the download; it may have blocked it." : undefined,
            });
          },
          () => {},
        );
      }
    }, 1000);
    return () => window.clearInterval(timer);
  }, [watching, update]);

  const enqueue = useCallback(async (picked: Picked, folder: string, account: FileAccount) => {
    const fetch = latest.current.authorizedFetch;
    // Empty folders get made now; files make the folders they're in.
    for (const relative of emptyFolders(picked)) {
      try {
        await createFolder(fetch, deviceId, account, joinRelative(platform, folder, relative));
      } catch {
        // An existing folder is as good.
      }
    }
    const added: UploadItem[] = picked.files.map(({ file, relative }) => {
      const key = nextKey.current++;
      files.current.set(key, file);
      const path = joinRelative(platform, folder, relative);
      const parts = relative.split("/");
      return {
        kind: "upload",
        key,
        name: parts.at(-1) ?? file.name,
        path,
        folder: parts.length > 1 ? joinRelative(platform, folder, parts.slice(0, -1).join("/")) : folder,
        size: file.size,
        account,
        overwrite: false,
        state: "queued",
        sent: 0,
        hashed: 0,
        startedAt: Date.now(),
      };
    });
    setItems((current) => [...current, ...added]);
    if (picked.folders.length && !added.length) latest.current.onLanded(folder);
  }, [deviceId, platform]);

  const addDownload = useCallback((view: DownloadView) => {
    const key = nextKey.current++;
    setItems((current) => [
      ...current,
      {
        kind: "download",
        key,
        id: view.id,
        name: view.kind === "archive" ? `${view.name}.zip` : view.name,
        archive: view.kind === "archive",
        size: view.size,
        files: view.files,
        state: "starting",
        bytes: 0,
        offset: 0,
        startedAt: Date.now(),
      },
    ]);
    startBrowserDownload(view.url);
  }, []);

  const cancel = useCallback((key: number) => {
    stops.current.get(key)?.abort();
    const id = uploads.current.get(key);
    uploads.current.delete(key);
    files.current.delete(key);
    hashes.current.delete(key);
    update(key, { state: "cancelled", error: undefined });
    if (id) void cancelUpload(latest.current.authorizedFetch, id).catch(() => {});
  }, [update]);

  const resume = useCallback((key: number) => {
    resumes.current.delete(key);
    update(key, { state: "queued", error: undefined });
  }, [update]);

  const replace = useCallback((key: number) => update(key, { state: "queued", overwrite: true, error: undefined }), [update]);

  const clearFinished = useCallback(() => {
    setItems((current) =>
      current.filter((item) =>
        item.kind === "upload"
          ? !["done", "cancelled", "failed"].includes(item.state)
          : item.state === "starting" || item.state === "sending",
      ),
    );
  }, []);

  // Leaving the page stops uploads, so the browser asks first.
  const busy = items.some((item) => item.kind === "upload" && ["queued", "uploading", "verifying"].includes(item.state));
  useEffect(() => {
    if (!busy) return;
    const warn = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [busy]);

  return { items, enqueue, addDownload, cancel, resume, replace, skip: cancel, clearFinished };
}

export type Transfers = ReturnType<typeof useTransfers>;

function uploadStatusText(item: UploadItem, now: number) {
  switch (item.state) {
    case "queued":
      return "Waiting";
    case "uploading": {
      const speed = rate(item.sent, item.startedAt, now);
      return `Uploading ${percent(item.sent, item.size).toFixed(0)}%${speed ? ` · ${formatBytes(speed)}/s` : ""}`;
    }
    case "verifying":
      return item.hashed < item.size ? `Checking ${percent(item.hashed, item.size).toFixed(0)}%` : "Checking on the device";
    case "done":
      return "Uploaded and checked";
    case "conflict":
      return "A file with this name is there already";
    case "paused":
      return item.error ?? "Paused";
    case "cancelled":
      return "Cancelled";
    case "failed":
      return item.error ?? "Failed";
  }
}

function downloadStatusText(item: DownloadItem, now: number) {
  switch (item.state) {
    case "starting":
      return "Starting in your browser";
    case "sending": {
      const speed = rate(item.bytes, item.startedAt, now);
      const total = item.archive ? `${formatBytes(item.bytes)} sent` : `${percent(item.offset + item.bytes, item.size).toFixed(0)}%`;
      return `Downloading ${total}${speed ? ` · ${formatBytes(speed)}/s` : ""}`;
    }
    case "done":
      return item.offset > 0 ? "Resumed and finished" : "Downloaded and checked";
    case "failed":
      return item.error ?? "Failed";
  }
}

// The list of transfers under the folder.
export function TransfersPanel({ transfers, platformLabel }: { transfers: Transfers; platformLabel: string }) {
  const { items, cancel, resume, replace, skip, clearFinished } = transfers;
  const [now, setNow] = useState(() => Date.now());
  const [copied, setCopied] = useState<number | null>(null);
  const moving = items.some((item) => item.state === "uploading" || item.state === "sending");
  useEffect(() => {
    if (!moving) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [moving]);
  if (!items.length) return null;
  const active = items.filter((item) => ["queued", "uploading", "verifying", "starting", "sending"].includes(item.state)).length;
  const finished = items.length - active - items.filter((item) => item.state === "paused" || item.state === "conflict").length;
  const copy = (item: TransferItem) => {
    if (!item.sha256) return;
    void navigator.clipboard?.writeText(item.sha256).then(() => setCopied(item.key), () => {});
  };

  return (
    <section className="file-transfers" aria-labelledby="file-transfers-title">
      <header>
        <h2 id="file-transfers-title">Transfers</h2>
        <span role="status">{active ? `${active} in progress` : "All done"}</span>
        {finished > 0 && <button type="button" className="link-button" onClick={clearFinished}>Clear finished</button>}
      </header>
      <ul>
        {items.map((item) => {
          const upload = item.kind === "upload";
          const bytes = upload ? item.sent : item.offset + item.bytes;
          const progress = upload
            ? item.state === "verifying" ? percent(item.hashed, item.size) : percent(item.sent, item.size)
            : item.archive ? null : percent(bytes, item.size);
          const status = upload ? uploadStatusText(item, now) : downloadStatusText(item, now);
          const failed = item.state === "failed";
          const done = item.state === "done";
          return (
            <li key={item.key} className={`file-transfer ${item.state}`}>
              <span className="file-transfer-icon" aria-hidden="true">
                {done ? <CircleCheck size={16} /> : failed ? <CircleAlert size={16} /> : item.state === "paused" ? <Pause size={16} /> : upload ? <ArrowUpFromLine size={16} /> : <ArrowDownToLine size={16} />}
              </span>
              <div className="file-transfer-body">
                <p>
                  <strong title={upload ? item.path : item.name}>{item.name}</strong>
                  <small>{upload ? `to ${platformLabel}` : `from ${platformLabel}`} · {formatBytes(item.size)}{!upload && item.archive ? ` in ${item.files} files` : ""}</small>
                </p>
                {(item.state === "uploading" || item.state === "verifying" || item.state === "sending" || item.state === "queued" || item.state === "starting") && (
                  progress === null
                    ? <progress aria-label={`${item.name}: ${status}`} />
                    : <progress aria-label={`${item.name}: ${status}`} max={100} value={progress} />
                )}
                <p className="file-transfer-status">
                  {(item.state === "uploading" || item.state === "verifying" || item.state === "sending" || item.state === "starting") && <LoaderCircle size={13} className="spin" aria-hidden="true" />}
                  <span>{status}</span>
                  {item.sha256 && (
                    <button type="button" className="file-digest-button" onClick={() => copy(item)} title="Copy the SHA-256">
                      SHA-256 <code>{item.sha256.slice(0, 16)}…</code><Copy size={12} aria-hidden="true" />
                      {copied === item.key && <span className="sr-only"> copied</span>}
                    </button>
                  )}
                </p>
              </div>
              <span className="file-transfer-actions">
                {upload && item.state === "conflict" && <>
                  <button type="button" className="secondary-button" onClick={() => replace(item.key)}>Replace</button>
                  <button type="button" className="secondary-button" onClick={() => skip(item.key)}>Skip</button>
                </>}
                {upload && (item.state === "paused" || item.state === "failed") && (
                  <button type="button" className="secondary-button" onClick={() => resume(item.key)}><RotateCw size={14} aria-hidden="true" />{item.state === "paused" ? "Resume" : "Retry"}</button>
                )}
                {upload && ["queued", "uploading", "verifying", "paused"].includes(item.state) && (
                  <button type="button" className="icon-button ghost" onClick={() => cancel(item.key)} aria-label={`Cancel uploading ${item.name}`} title="Cancel"><X size={15} /></button>
                )}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

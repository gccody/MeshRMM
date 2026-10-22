import { CircleAlert, CircleCheck, LoaderCircle, X } from "lucide-react";
import { type RefObject, useEffect, useId, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { NOT_ONLINE_HINT, type Wake, type WakeState, isSettling, relayText, wakeHeadline, wakeState, wakeTitle, wakeTone } from "./model";
import { fetchWakes, wakeDevice, wakeDevices } from "./wake-api";

// Relays report within a second or two; a wake still sending is read again
// this often.
const SETTLE_POLL_MS = 2_000;
// How often the dialog notices a watch ending.
const TICK_MS = 5_000;

// The current time, every few seconds.
export function useNow(intervalMs = TICK_MS) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);
  return now;
}

// Reads wakes again while their relays haven't all reported.
export function useSettling(wakes: readonly Wake[] | null, onLoaded: (wakes: Wake[]) => void) {
  const { authorizedFetch } = useWorkspace();
  const settling = (wakes ?? []).filter(isSettling).map((wake) => wake.id).join(",");
  const loaded = useRef(onLoaded);
  useEffect(() => {
    loaded.current = onLoaded;
  });
  useEffect(() => {
    if (!settling) return;
    const timer = window.setTimeout(() => {
      fetchWakes(authorizedFetch, settling.split(",")).then((fresh) => loaded.current(fresh), () => {});
    }, SETTLE_POLL_MS);
    return () => window.clearTimeout(timer);
  }, [authorizedFetch, settling]);
}

// Wakes the chosen offline devices as soon as it opens, then shows which
// devices sent the packets and watches for each device to come online.
export function WakeDialog({ agents, returnFocus, onClose }: {
  // Offline devices only.
  agents: readonly Agent[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const titleId = useId();
  const [wakes, setWakes] = useState<Wake[] | null>(null);
  const [skipped, setSkipped] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const started = useRef(false);
  const now = useNow();
  const ids = useRef(agents.map((agent) => agent.id));

  useEffect(() => {
    // Strict mode runs effects twice; each device is woken once.
    if (started.current) return;
    started.current = true;
    const run = async () => {
      try {
        if (ids.current.length === 1) {
          setWakes([await wakeDevice(authorizedFetch, ids.current[0])]);
        } else {
          const result = await wakeDevices(authorizedFetch, ids.current);
          setWakes(result.wakes);
          setSkipped(result.skipped.length);
        }
      } catch (failure) {
        if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The devices could not be woken."));
      }
    };
    void run();
  }, [authorizedFetch]);

  useSettling(wakes, (fresh) => setWakes((current) => current?.map((wake) => fresh.find((candidate) => candidate.id === wake.id) ?? wake) ?? null));

  const connected = new Set(inventory.agents.filter((agent) => agent.connected).map((agent) => agent.id));
  // A device that came online is read again, so it stays woken if it goes
  // offline before the dialog closes.
  const cameOnline = (wakes ?? []).filter((wake) => wake.online_at === null && connected.has(wake.device_id)).map((wake) => wake.id).join(",");
  useEffect(() => {
    if (!cameOnline) return;
    fetchWakes(authorizedFetch, cameOnline.split(",")).then(
      (fresh) => setWakes((current) => current?.map((wake) => fresh.find((candidate) => candidate.id === wake.id) ?? wake) ?? null),
      () => {},
    );
  }, [authorizedFetch, cameOnline]);
  const names = agents.map((agent) => agent.name);
  const single = agents.length === 1;

  return (
    <ModalDialog className="settings-modal toolbox-modal wake-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{wakeTitle(names)}</h2>
      <p>Online devices on the same network broadcast a Wake-on-LAN packet for {single ? "it" : "each one"}. This watches for {single ? "it" : "them"} to come online for up to 10 minutes; closing it doesn&apos;t stop anything.</p>
      {!wakes && !error && <p className="page-status" role="status"><LoaderCircle size={16} className="spin" aria-hidden="true" /> Asking devices on {single ? `${names[0]}’s` : "their"} network…</p>}
      {error && <p role="alert" className="installer-error">{error}</p>}
      {wakes && (
        <ul className="wake-list" aria-live="polite">
          {wakes.map((wake) => (
            <WakeItem key={wake.id} wake={wake} name={wake.device_name ?? agents.find((agent) => agent.id === wake.device_id)?.name ?? "Deleted device"} state={wakeState(wake, connected.has(wake.device_id), now)} />
          ))}
        </ul>
      )}
      {skipped > 0 && <p className="field-help">{skipped} of the chosen devices {skipped === 1 ? "was" : "were"} online already or no longer enrolled.</p>}
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </ModalDialog>
  );
}

// One wake: where it stands, which devices relayed it and how.
export function WakeItem({ wake, name, state }: { wake: Wake; name: string; state: WakeState }) {
  const tone = wakeTone(state);
  const Icon = tone === "success" ? CircleCheck : tone === "problem" ? CircleAlert : LoaderCircle;
  return (
    <li className={`wake-item wake-${tone}`}>
      <strong><Icon size={16} aria-hidden="true" className={tone === "pending" ? "spin" : undefined} />{wakeHeadline(state, name)}</strong>
      {(state.kind === "no_relay" || state.kind === "failed") && <p>{state.reason}</p>}
      {state.kind === "not_online" && <p>{NOT_ONLINE_HINT}</p>}
      {wake.relays.length > 0 && (
        <ul className="wake-relays" aria-label="Relays">
          {wake.relays.map((relay) => (
            <li key={`${relay.device_id}-${relay.broadcast}`} className={`wake-relay-${relay.status}`}>
              <span>{relay.name}</span>
              <span>{relayText(relay)}</span>
            </li>
          ))}
        </ul>
      )}
      {wake.mac_addresses.length > 0 && <small>Wakes {wake.mac_addresses.join(", ")}</small>}
    </li>
  );
}

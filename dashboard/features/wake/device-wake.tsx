import { Power } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { formatRelative } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { type Wake, wakeState } from "./model";
import { WakeDialog, WakeItem, useNow, useSettling } from "./wake-dialog";
import { fetchRecentWakes } from "./wake-api";

// How long the device page shows the last wake of an offline device.
const SHOWN_FOR_MS = 24 * 60 * 60_000;

// The device page's Wake button, for an offline device, and the device's
// latest wake of the last day: who asked, through which devices, and
// whether it came online.
export function DeviceWake({ agent }: { agent: Agent }) {
  const { authorizedFetch, can } = useWorkspace();
  const [waking, setWaking] = useState(false);
  const opener = useRef<HTMLButtonElement>(null);
  const load = useCallback(() => fetchRecentWakes(authorizedFetch, agent.id), [authorizedFetch, agent.id]);
  const { data, reload, setData } = useResource(load, "The device’s wake requests could not be loaded.");
  const now = useNow();
  const latest = data?.[0];
  useSettling(latest ? [latest] : null, (fresh) => setData(() => fresh));
  // The device coming online is news for the last wake.
  const wasConnected = useRef(agent.connected);
  useEffect(() => {
    if (agent.connected && !wasConnected.current) reload();
    wasConnected.current = agent.connected;
  }, [agent.connected, reload]);

  // Once an online device's watch is over, its last wake is old news.
  const shown: Wake | undefined = latest && now - latest.created_at < SHOWN_FOR_MS && (!agent.connected || now < latest.watch_until) ? latest : undefined;
  const canWake = !agent.connected && can("devices.wake");
  if (!shown && !canWake) return null;
  return (
    <div className="device-wake">
      {canWake && (
        <button ref={opener} type="button" className="secondary-button" onClick={() => setWaking(true)} aria-haspopup="dialog">
          <Power size={15} aria-hidden="true" />Wake
        </button>
      )}
      {shown && (
        <ul className="wake-list" aria-label="Last wake">
          <WakeItem wake={shown} name={agent.name} state={wakeState(shown, agent.connected, now)} />
          <li className="wake-meta">Asked by {shown.requested_by} {formatRelative(shown.created_at, now)}</li>
        </ul>
      )}
      {waking && <WakeDialog agents={[agent]} returnFocus={opener} onClose={() => { setWaking(false); reload(); }} />}
    </div>
  );
}

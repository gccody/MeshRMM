import { Download, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { useNavigate } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { DEFAULT_WAIT_HOURS, MAX_WAIT_HOURS, type OfflineChoice, waitHoursProblem } from "../automation/model";
import { useWorkspace } from "../workspace/workspace-context";
import { CATEGORIES, CATEGORY_LABELS, DEFAULT_REBOOT_DELAY_MINUTES, type DeviceUpdateSummary, type RebootBehavior, type UpdateCategory, describeSelection, rebootDelayProblem } from "./model";
import { RebootFields } from "./reboot-fields";
import { startUpdateJob } from "./updates-api";

// The categories offered first; the rest are under "More categories".
const COMMON: readonly UpdateCategory[] = ["security", "critical", "definition", "update_rollup", "driver", "other"];

// Installs updates of chosen categories, or all, on the chosen devices as
// a job, then opens the job's page.
export function BulkInstallDialog({ devices, returnFocus, onClose }: {
  devices: readonly DeviceUpdateSummary[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const navigate = useNavigate();
  const titleId = useId();
  const [categories, setCategories] = useState<UpdateCategory[]>(["security", "critical"]);
  const [reboot, setReboot] = useState<RebootBehavior>("if_required");
  const [delay, setDelay] = useState(DEFAULT_REBOOT_DELAY_MINUTES);
  const [offline, setOffline] = useState<OfflineChoice>("wait");
  const [waitHours, setWaitHours] = useState(String(DEFAULT_WAIT_HOURS));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const connected = new Set(inventory.agents.filter((agent) => agent.connected).map((agent) => agent.id));
  const offlineCount = devices.filter((device) => !connected.has(device.device_id)).length;
  const unsupported = devices.filter((device) => !device.supported).length;
  const pacman = devices.filter((device) => device.manager === "pacman").length;
  const problem = (reboot !== "never" ? rebootDelayProblem(delay) : null) ?? (offline === "wait" ? waitHoursProblem(waitHours) : null);

  const toggle = (category: UpdateCategory, checked: boolean) =>
    setCategories((current) => (checked ? [...current, category] : current.filter((candidate) => candidate !== category)));

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const job = await startUpdateJob(
        authorizedFetch,
        devices.map((device) => device.device_id),
        { action: "install", categories: CATEGORIES.filter((category) => categories.includes(category)), reboot, reboot_delay_minutes: delay },
        { offline, wait_hours: offline === "wait" ? Number(waitHours) : DEFAULT_WAIT_HOURS },
      );
      onClose();
      void navigate(`/job?id=${encodeURIComponent(job.id)}`);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The install could not be started."));
      setBusy(false);
    }
  };

  const notes = [
    `Installs on ${devices.length} ${devices.length === 1 ? "device" : "devices"}.`,
    offlineCount ? (offline === "wait"
      ? `${offlineCount} ${offlineCount === 1 ? "is" : "are"} offline and will install if ${offlineCount === 1 ? "it connects" : "they connect"} within ${Number(waitHours) || DEFAULT_WAIT_HOURS} hours.`
      : `${offlineCount} ${offlineCount === 1 ? "is" : "are"} offline and will be skipped.`) : null,
    unsupported ? `${unsupported} ${unsupported === 1 ? "has" : "have"} no supported update manager and will be skipped.` : null,
    pacman && categories.length ? `On Arch Linux every pending package is “Other”, and pacman installs all of them at once.` : null,
  ].filter(Boolean);

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Install updates on {devices.length === 1 ? devices[0].device_name : `${devices.length} devices`}</h2>
      <p>Each device looks for updates, installs those of the chosen kinds that are pending then, and looks again. The job&apos;s page follows every device.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <CategoryChoice categories={categories} onToggle={toggle} />
          <RebootFields name="bulk-reboot" reboot={reboot} delay={delay} onReboot={setReboot} onDelay={setDelay} />
          <fieldset className="choice-list">
            <legend>Offline devices</legend>
            <label><input type="radio" name="bulk-offline" checked={offline === "wait"} onChange={() => setOffline("wait")} /><span><strong>Install when they come online</strong>If they connect within the time below.</span></label>
            <label><input type="radio" name="bulk-offline" checked={offline === "skip"} onChange={() => setOffline("skip")} /><span><strong>Skip them</strong>Only devices online now install.</span></label>
          </fieldset>
          {offline === "wait" && (
            <label htmlFor="bulk-wait">Wait up to
              <span className="input-with-unit">
                <input id="bulk-wait" type="number" inputMode="numeric" min={1} max={MAX_WAIT_HOURS} step={1} value={waitHours} onChange={(event) => setWaitHours(event.target.value)} />
                <span aria-hidden="true">hours</span>
              </span>
            </label>
          )}
          <p className="job-preview" role="status">{describeSelection({ categories })}. {notes.join(" ")}</p>
        </fieldset>
        {(error || problem) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null || devices.length === 0}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Download size={16} />} Install updates
          </button>
        </div>
      </form>
    </ModalDialog>
  );
}

// Which kinds of update to install; none chosen installs every one.
export function CategoryChoice({ categories, onToggle }: { categories: readonly UpdateCategory[]; onToggle: (category: UpdateCategory, checked: boolean) => void }) {
  const [more, setMore] = useState(() => categories.some((category) => !COMMON.includes(category)));
  const shown = more ? CATEGORIES : COMMON;
  return (
    <fieldset className="category-choice">
      <legend>Install</legend>
      <div>
        {shown.map((category) => (
          <label key={category}>
            <input type="checkbox" checked={categories.includes(category)} onChange={(event) => onToggle(category, event.target.checked)} />
            <span>{CATEGORY_LABELS[category]}</span>
          </label>
        ))}
        {!more && <button type="button" className="link-button" onClick={() => setMore(true)}>More categories</button>}
      </div>
      <small className="field-help">{categories.length ? `${describeSelection({ categories })} only.` : "Every pending update, whatever its category."} Linux distributions mark security updates; their other packages are “Other”.</small>
    </fieldset>
  );
}

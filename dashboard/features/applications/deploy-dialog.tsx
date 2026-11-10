import { LoaderCircle, Rocket, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useMemo, useState } from "react";
import { useNavigate } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { DevicePlatform } from "../agents/types";
import { DevicePicker } from "../device-picker/device-picker";
import { devicesOn, platformCounts } from "../device-picker/model";
import { DEFAULT_WAIT_HOURS, MAX_WAIT_HOURS, type OfflineChoice, type TargetKind, coveredAgents, waitHoursProblem } from "../automation/model";
import type { TagMatch } from "../tags/model";
import { TagTargetFields } from "../tags/tag-chip";
import { useWorkspace } from "../workspace/workspace-context";
import { startApplicationJob } from "./applications-api";
import type { RunRequest } from "./model";

// Installs, updates or uninstalls on chosen devices, companies, tagged
// devices or every device, as a job, then opens the job's page.
export function DeployDialog({ title, intro, request, platforms, deviceIds, returnFocus, onClose }: {
  title: string;
  intro: string;
  request: RunRequest;
  // The platforms it applies to; devices on others are skipped.
  platforms: readonly DevicePlatform[];
  // The devices chosen already.
  deviceIds?: readonly string[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const navigate = useNavigate();
  const titleId = useId();
  const wide = can("jobs.run_wide");
  const [target, setTarget] = useState<TargetKind>("devices");
  const [chosenDevices, setChosenDevices] = useState<readonly string[]>(deviceIds ?? []);
  const [chosenCompanies, setChosenCompanies] = useState<readonly string[]>([]);
  const [chosenTags, setChosenTags] = useState<string[]>([]);
  const [tagMatch, setTagMatch] = useState<TagMatch>("any");
  const [offline, setOffline] = useState<OfflineChoice>("wait");
  const [waitHours, setWaitHours] = useState(String(DEFAULT_WAIT_HOURS));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const covered = useMemo(
    () => coveredAgents(inventory.agents, { kind: target, deviceIds: chosenDevices, companyIds: chosenCompanies, tagIds: chosenTags, tagMatch }),
    [inventory.agents, target, chosenDevices, chosenCompanies, chosenTags, tagMatch],
  );
  const { skipped, offline: offlineCount } = useMemo(() => platformCounts(covered, platforms), [covered, platforms]);
  // Chosen devices on other platforms stay listed so they can be unticked.
  // The list is made again only when those change, not on every tick.
  const byId = useMemo(() => new Map(inventory.agents.map((agent) => [agent.id, agent])), [inventory.agents]);
  const otherChosen = chosenDevices.filter((id) => {
    const agent = byId.get(id);
    return agent !== undefined && !platforms.includes(agent.platform);
  }).join(",");
  const pickable = useMemo(() => devicesOn(inventory.agents, platforms, otherChosen ? otherChosen.split(",") : []), [inventory.agents, platforms, otherChosen]);
  const problem = target === "devices" && !chosenDevices.length ? "Choose at least one device."
    : target === "companies" && !chosenCompanies.length ? "Choose at least one company."
    : target === "tags" && !chosenTags.length ? "Choose at least one tag."
    : offline === "wait" ? waitHoursProblem(waitHours) : null;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const job = await startApplicationJob(authorizedFetch, request, {
        target,
        device_ids: target === "devices" ? [...chosenDevices] : [],
        company_ids: target === "companies" ? [...chosenCompanies] : [],
        tag_ids: target === "tags" ? chosenTags : [],
        tag_match: tagMatch,
        offline,
        wait_hours: offline === "wait" ? Number(waitHours) : DEFAULT_WAIT_HOURS,
      });
      onClose();
      void navigate(`/job?id=${encodeURIComponent(job.id)}`);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The job could not be started."));
      setBusy(false);
    }
  };

  const notes = !covered.length ? ["No devices are chosen."] : [
    `Runs on ${covered.length} ${covered.length === 1 ? "device" : "devices"}.`,
    skipped ? `${skipped} ${skipped === 1 ? "is" : "are"} on another operating system and will be skipped.` : null,
    offlineCount ? (offline === "wait"
      ? `${offlineCount} ${offlineCount === 1 ? "is" : "are"} offline and will run if ${offlineCount === 1 ? "it connects" : "they connect"} within ${Number(waitHours) || DEFAULT_WAIT_HOURS} hours.`
      : `${offlineCount} ${offlineCount === 1 ? "is" : "are"} offline and will be skipped.`) : null,
  ].filter(Boolean);

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{title}</h2>
      <p>{intro} The job&apos;s page follows every device and shows how each application went.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <fieldset className="choice-list">
            <legend>Devices</legend>
            <label><input type="radio" name="deploy-target" checked={target === "devices"} onChange={() => setTarget("devices")} /><span><strong>Chosen devices</strong>{chosenDevices.length ? `${chosenDevices.length} chosen` : "Choose them below."}</span></label>
            {wide && <>
              <label><input type="radio" name="deploy-target" checked={target === "companies"} onChange={() => setTarget("companies")} disabled={!inventory.companies.length} /><span><strong>Whole companies</strong>{inventory.companies.length ? "Every device in the companies chosen below, as they are when the job starts." : "There are no companies yet."}</span></label>
              <label><input type="radio" name="deploy-target" checked={target === "tags"} onChange={() => setTarget("tags")} disabled={!inventory.tags.length} /><span><strong>Tagged devices</strong>{inventory.tags.length ? "Every device with the tags chosen below, as they are when the job starts." : "There are no tags yet."}</span></label>
              <label><input type="radio" name="deploy-target" checked={target === "all"} onChange={() => setTarget("all")} /><span><strong>Every device</strong>All {inventory.agents.length} devices.</span></label>
            </>}
          </fieldset>
          {target === "devices" && <DevicePicker agents={pickable} chosen={chosenDevices} onChange={setChosenDevices} language={null} />}
          {target === "tags" && <TagTargetFields tags={inventory.tags} chosen={chosenTags} match={tagMatch} onChange={(change) => {
            if (change.tagIds) setChosenTags(change.tagIds);
            if (change.tagMatch) setTagMatch(change.tagMatch);
          }} />}
          {target === "companies" && (
            <fieldset className="choice-list picker-list">
              <legend className="sr-only">Companies</legend>
              {inventory.companies.map((company) => (
                <label key={company.id}>
                  <input type="checkbox" checked={chosenCompanies.includes(company.id)} onChange={(event) => setChosenCompanies((current) => event.target.checked ? [...current, company.id] : current.filter((id) => id !== company.id))} />
                  <strong>{company.name}</strong>
                </label>
              ))}
            </fieldset>
          )}
          <fieldset className="choice-list">
            <legend>Offline devices</legend>
            <label><input type="radio" name="deploy-offline" checked={offline === "wait"} onChange={() => setOffline("wait")} /><span><strong>Run when they come online</strong>If they connect within the time below.</span></label>
            <label><input type="radio" name="deploy-offline" checked={offline === "skip"} onChange={() => setOffline("skip")} /><span><strong>Skip them</strong>Only devices online now run it.</span></label>
          </fieldset>
          {offline === "wait" && (
            <label htmlFor="deploy-wait">Wait up to
              <span className="input-with-unit">
                <input id="deploy-wait" type="number" inputMode="numeric" min={1} max={MAX_WAIT_HOURS} step={1} value={waitHours} onChange={(event) => setWaitHours(event.target.value)} />
                <span aria-hidden="true">hours</span>
              </span>
            </label>
          )}
          <p className="job-preview" role="status">{notes.join(" ")}</p>
        </fieldset>
        {(error || (problem && covered.length > 0)) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null || covered.length === skipped}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Rocket size={16} />} Start job
          </button>
        </div>
      </form>
    </ModalDialog>
  );
}

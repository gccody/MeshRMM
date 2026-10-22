import { LoaderCircle, Play, Send, X } from "lucide-react";
import { type FormEvent, type RefObject, useEffect, useId, useState } from "react";
import { useNavigate } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { LANGUAGE_LABELS, RUN_AS_LABELS, type RunAs, type Toolbox, formatBytes, groupByFolder } from "../toolbox/model";
import { fetchToolbox } from "../toolbox/toolbox-api";
import { useWorkspace } from "../workspace/workspace-context";
import { startJob } from "./automation-api";
import { DevicePicker } from "./device-picker";
import { DEFAULT_WAIT_HOURS, type Destination, type JobKind, MAX_WAIT_HOURS, type OfflineChoice, type TargetKind, jobPreview, previewText, waitHoursProblem } from "./model";

const DESTINATION_HELP: Record<Destination, { label: string; detail: string }> = {
  user: { label: "The signed-in user's Documents", detail: "Their MeshRMM Transferred Files folder. With nobody signed in, Public Documents." },
  public: { label: "Public Documents", detail: "The shared MeshRMM Transferred Files folder every user can open." },
};

type Props = {
  // What to send: a script or a file, chosen already or not.
  kind: JobKind;
  itemId?: string;
  // The devices chosen already, such as the Devices page's selection.
  deviceIds?: readonly string[];
  // The toolbox, when the opener has it; loaded otherwise.
  toolbox?: Pick<Toolbox, "scripts" | "files"> | null;
  onClose: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
};

/**
 * Runs a script, or sends a file, on chosen devices, whole companies or
 * every device, then opens the job's page.
 */
export function StartJobModal({ kind: initialKind, itemId, deviceIds, toolbox: givenToolbox, onClose, returnFocus }: Props) {
  const { authorizedFetch, inventory, can } = useWorkspace();
  const navigate = useNavigate();
  const titleId = useId();
  const [loaded, setLoaded] = useState<Pick<Toolbox, "scripts" | "files"> | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const toolbox = givenToolbox ?? loaded;
  const kinds = ([can("scripts.run") && "script", can("files.deliver") && "file"].filter(Boolean) as JobKind[]);
  const [kind, setKind] = useState<JobKind>(kinds.includes(initialKind) ? initialKind : kinds[0] ?? initialKind);
  const [scriptId, setScriptId] = useState(initialKind === "script" ? itemId ?? "" : "");
  const [fileId, setFileId] = useState(initialKind === "file" ? itemId ?? "" : "");
  const [runAs, setRunAs] = useState<RunAs>("user");
  const [destination, setDestination] = useState<Destination>("user");
  const wide = can("jobs.run_wide");
  const [target, setTarget] = useState<TargetKind>("devices");
  const [chosenDevices, setChosenDevices] = useState<readonly string[]>(deviceIds ?? []);
  const [chosenCompanies, setChosenCompanies] = useState<readonly string[]>([]);
  const [offline, setOffline] = useState<OfflineChoice>("wait");
  const [waitHours, setWaitHours] = useState(String(DEFAULT_WAIT_HOURS));
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);

  useEffect(() => {
    if (givenToolbox) return;
    let cancelled = false;
    fetchToolbox(authorizedFetch)
      .then((next) => { if (!cancelled) setLoaded(next); })
      .catch((error: unknown) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) setLoadError(errorText(error, "The toolbox could not be loaded."));
      });
    return () => { cancelled = true; };
  }, [authorizedFetch, givenToolbox]);

  const script = toolbox?.scripts.find((candidate) => candidate.id === scriptId);
  const file = toolbox?.files.find((candidate) => candidate.id === fileId);
  const language = kind === "script" ? script?.language ?? null : null;
  const chosen = { kind: target, deviceIds: chosenDevices, companyIds: chosenCompanies };
  const preview = jobPreview(inventory.agents, chosen, language);
  const waitProblem = offline === "wait" ? waitHoursProblem(waitHours) : null;
  const item = kind === "script" ? script : file;
  const problem = !item ? (kind === "script" ? "Choose a script." : "Choose a file.")
    : target === "devices" && !chosenDevices.length ? "Choose at least one device."
    : target === "companies" && !chosenCompanies.length ? "Choose at least one company."
    : waitProblem;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem || !item) return;
    setStarting(true);
    setStartError(null);
    try {
      const job = await startJob(authorizedFetch, {
        ...(kind === "script" ? { script_id: item.id, run_as: runAs } : { file_id: item.id, destination }),
        target,
        device_ids: target === "devices" ? [...chosenDevices] : [],
        company_ids: target === "companies" ? [...chosenCompanies] : [],
        offline,
        wait_hours: offline === "wait" ? Number(waitHours) : DEFAULT_WAIT_HOURS,
      });
      onClose();
      void navigate(`/job?id=${encodeURIComponent(job.id)}`);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setStartError(errorText(error, "The job could not be started."));
      setStarting(false);
    }
  };

  const scriptFolders = groupByFolder(toolbox?.scripts ?? []);
  const fileFolders = groupByFolder(toolbox?.files ?? []);

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{kind === "script" ? "Run a script on devices" : "Send a file to devices"}</h2>
      <p>Each device runs it in the background. The job&apos;s page follows every device and shows each one&apos;s result.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={starting}>
          {kinds.length > 1 && !itemId && (
            <div className="segmented" role="group" aria-label="What to send">
              <button type="button" aria-pressed={kind === "script"} onClick={() => setKind("script")}>Run a script</button>
              <button type="button" aria-pressed={kind === "file"} onClick={() => setKind("file")}>Send a file</button>
            </div>
          )}
          {kind === "script" ? (
            <>
              <label htmlFor="job-script">Script
                <select id="job-script" value={scriptId} onChange={(event) => setScriptId(event.target.value)} disabled={!toolbox}>
                  <option value="" disabled>{toolbox ? (toolbox.scripts.length ? "Choose a script" : "No scripts yet") : "Loading scripts…"}</option>
                  {scriptFolders.map(({ folder, items }) => (
                    <optgroup key={folder} label={folder || "Top level"}>
                      {items.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.name} ({LANGUAGE_LABELS[candidate.language]})</option>)}
                    </optgroup>
                  ))}
                </select>
              </label>
              {script?.description && <small className="field-help">{script.description}</small>}
              <fieldset className="run-as-choice">
                <legend>Run as</legend>
                {(["user", "system"] as const).map((choice) => (
                  <label key={choice}>
                    <input type="radio" name="job-run-as" value={choice} checked={runAs === choice} onChange={() => setRunAs(choice)} />
                    <span><strong>{RUN_AS_LABELS[choice]}</strong>{choice === "user" ? "The person signed in to each device. Where nobody is, the system account." : "SYSTEM on Windows, root on macOS and Linux."}</span>
                  </label>
                ))}
              </fieldset>
            </>
          ) : (
            <>
              <label htmlFor="job-file">File
                <select id="job-file" value={fileId} onChange={(event) => setFileId(event.target.value)} disabled={!toolbox}>
                  <option value="" disabled>{toolbox ? (toolbox.files.length ? "Choose a file" : "No files yet") : "Loading files…"}</option>
                  {fileFolders.map(({ folder, items }) => (
                    <optgroup key={folder} label={folder || "Top level"}>
                      {items.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.name} ({formatBytes(candidate.size_bytes)})</option>)}
                    </optgroup>
                  ))}
                </select>
              </label>
              <fieldset className="run-as-choice">
                <legend>Save to</legend>
                {(["user", "public"] as const).map((choice) => (
                  <label key={choice}>
                    <input type="radio" name="job-destination" value={choice} checked={destination === choice} onChange={() => setDestination(choice)} />
                    <span><strong>{DESTINATION_HELP[choice].label}</strong>{DESTINATION_HELP[choice].detail}</span>
                  </label>
                ))}
              </fieldset>
            </>
          )}
          {loadError && <p role="alert" className="installer-error">{loadError}</p>}

          <fieldset className="choice-list">
            <legend>Devices</legend>
            <label><input type="radio" name="job-target" checked={target === "devices"} onChange={() => setTarget("devices")} /><span><strong>Chosen devices</strong>{chosenDevices.length ? `${chosenDevices.length} chosen` : "Choose them below."}</span></label>
            {wide && <>
              <label><input type="radio" name="job-target" checked={target === "companies"} onChange={() => setTarget("companies")} disabled={!inventory.companies.length} /><span><strong>Whole companies</strong>{inventory.companies.length ? "Every device in the companies chosen below, as they are when the job starts." : "There are no companies yet."}</span></label>
              <label><input type="radio" name="job-target" checked={target === "all"} onChange={() => setTarget("all")} /><span><strong>Every device</strong>All {inventory.agents.length} devices.</span></label>
            </>}
          </fieldset>
          {target === "devices" && <DevicePicker agents={inventory.agents} chosen={chosenDevices} onChange={setChosenDevices} language={language} />}
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
            <label><input type="radio" name="job-offline" checked={offline === "wait"} onChange={() => setOffline("wait")} /><span><strong>Run when they come online</strong>If they connect within the time below.</span></label>
            <label><input type="radio" name="job-offline" checked={offline === "skip"} onChange={() => setOffline("skip")} /><span><strong>Skip them</strong>Only devices online now run it.</span></label>
          </fieldset>
          {offline === "wait" && (
            <label htmlFor="job-wait">Wait up to
              <span className="input-with-unit">
                <input id="job-wait" type="number" inputMode="numeric" min={1} max={MAX_WAIT_HOURS} step={1} value={waitHours} onChange={(event) => setWaitHours(event.target.value)} />
                <span aria-hidden="true">hours</span>
              </span>
            </label>
          )}
          <p className="job-preview" role="status">{previewText(preview, language, offline, Number(waitHours) || DEFAULT_WAIT_HOURS)}</p>
        </fieldset>
        {(startError || (problem && item)) && <p role="alert" className="installer-error">{startError ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={starting || problem !== null || preview.devices === 0}>
            {starting ? <LoaderCircle size={16} className="spin" /> : kind === "script" ? <Play size={16} /> : <Send size={16} />}
            {kind === "script" ? "Run on devices" : "Send to devices"}
          </button>
        </div>
      </form>
    </ModalDialog>
  );
}

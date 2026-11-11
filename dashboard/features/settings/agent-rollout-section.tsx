import { LoaderCircle, Pause, Play, RefreshCw } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, jsonBody } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import {
  type AgentRollout,
  REACH_TEXT,
  type RolloutChange,
  loweringText,
  nextSteps,
  parsePercent,
  progressLines,
  stageText,
} from "./agent-rollout";

const PATH = "/v1/settings/agent-rollout";
const LOAD_FAILED = "The Agent update settings could not be loaded.";
const SAVE_FAILED = "The Agent update settings could not be saved.";

// Settings › Agent updates: whether each new Agent release goes to every
// device at once or in stages the administrator advances, and how far the
// release the server ships has reached.
export function AgentRolloutSection({ hidden }: { hidden: boolean }) {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(async () => expectJson<AgentRollout>(await authorizedFetch(PATH), LOAD_FAILED), [authorizedFetch]);
  const { data: rollout, error: loadError, reload, setData } = useResource(load, LOAD_FAILED);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [customPercent, setCustomPercent] = useState("");
  const [firstPercent, setFirstPercent] = useState<string | null>(null);

  const change = async (body: RolloutChange) => {
    setBusy(true);
    setError(null);
    try {
      const changed = await expectJson<AgentRollout>(await authorizedFetch(PATH, jsonBody(body, "PATCH")), SAVE_FAILED);
      setData(() => changed);
      return true;
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, SAVE_FAILED));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const advanceTo = async (percent: number) => {
    if (!rollout) return;
    const lowering = loweringText(rollout.percent, percent);
    if (lowering && !window.confirm(`Lower the stage to ${percent}%? ${lowering}`)) return;
    if (await change({ percent })) setCustomPercent("");
  };

  const setCustom = (event: FormEvent) => {
    event.preventDefault();
    const percent = parsePercent(customPercent);
    if (percent !== undefined) void advanceTo(percent);
  };

  const saveFirstPercent = async (event: FormEvent) => {
    event.preventDefault();
    const percent = parsePercent(firstPercent ?? "");
    if (percent !== undefined && (await change({ first_percent: percent }))) setFirstPercent(null);
  };

  const custom = parsePercent(customPercent);
  const first = firstPercent === null ? undefined : parsePercent(firstPercent);

  return (
    <section className="settings-section agent-rollout" id="settings-agent-updates" role="tabpanel" aria-labelledby="settings-tab-agent-updates" hidden={hidden} tabIndex={0}>
      <h2>Agent updates</h2>
      <p>Devices install new Agent releases from this server on their own. Choose whether a release goes to every device at once, or to some devices first.</p>
      {!rollout ? (
        loadError
          ? <><p role="alert" className="form-error">{loadError}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading Agent update settings…</p>
      ) : <>
        <p className="agent-rollout-release">{rollout.release
          ? <>This server ships Agent <strong>{rollout.release}</strong>.</>
          : "This server has no Agent release to offer: its downloads are missing."}</p>
        <fieldset className="agent-rollout-mode" disabled={busy}>
          <legend>New releases go to</legend>
          <label><input type="radio" name="agent-rollout-mode" checked={rollout.mode === "everyone"} onChange={() => void change({ mode: "everyone" })} /> Every device at once</label>
          <label><input type="radio" name="agent-rollout-mode" checked={rollout.mode === "staged"} onChange={() => void change({ mode: "staged" })} aria-describedby="agent-rollout-staged-help" /> Some devices first, then more as I advance it</label>
          <p id="agent-rollout-staged-help" className="field-help">Turning this on starts the current release at the first stage.</p>
        </fieldset>
        <p role="status" className="agent-rollout-stage">{stageText(rollout)}</p>
        {rollout.mode === "staged" && <>
          <div className="agent-rollout-actions" role="group" aria-label="Advance the rollout">
            {nextSteps(rollout.percent).map((step) => (
              <button key={step} type="button" className="secondary-button" disabled={busy} onClick={() => void advanceTo(step)}>{step === 100 ? "Offer to every device" : `Advance to ${step}%`}</button>
            ))}
            <button type="button" className="secondary-button" disabled={busy} onClick={() => void change({ paused: !rollout.paused })}>
              {rollout.paused ? <><Play size={15} /> Resume</> : <><Pause size={15} /> Pause</>}
            </button>
          </div>
          <form className="agent-rollout-inline" onSubmit={setCustom}>
            <label htmlFor="agent-rollout-percent">Offer to this share of devices (%)<input id="agent-rollout-percent" type="number" inputMode="numeric" min={1} max={100} step={1} placeholder={String(rollout.percent)} value={customPercent} onChange={(event) => setCustomPercent(event.target.value)} aria-invalid={customPercent !== "" && custom === undefined} aria-describedby="agent-rollout-percent-help" /></label>
            <button type="submit" className="secondary-button" disabled={busy || custom === undefined || custom === rollout.percent}>Set</button>
          </form>
          <p id="agent-rollout-percent-help" className="field-help">Any share from 1 to 100%. Lowering it stops offering the release to devices above the new share; devices that installed it keep it. Pausing doesn’t undo installs either.</p>
          <form className="agent-rollout-inline" onSubmit={(event) => void saveFirstPercent(event)}>
            <label htmlFor="agent-rollout-first">Start each new release at (%)<input id="agent-rollout-first" type="number" inputMode="numeric" min={1} max={100} step={1} value={firstPercent ?? String(rollout.first_percent)} onChange={(event) => setFirstPercent(event.target.value)} aria-invalid={first === undefined && firstPercent !== null} /></label>
            <button type="submit" className="secondary-button" disabled={busy || first === undefined || first === rollout.first_percent}>Save</button>
          </form>
          <p className="field-help">When this server is upgraded, its new release starts again at this share, not paused.</p>
        </>}
        <p className="field-help">{REACH_TEXT}</p>
        {rollout.release && rollout.devices && (
          <div className="agent-rollout-progress">
            <h3>Enrolled devices</h3>
            <ul>{progressLines(rollout.release, rollout.devices).map((line) => <li key={line}>{line}</li>)}</ul>
            <p className="field-help">From the version each device reported at its last update check.</p>
            <button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Refresh</button>
          </div>
        )}
        {error && <p role="alert" className="form-error">{error}</p>}
      </>}
    </section>
  );
}

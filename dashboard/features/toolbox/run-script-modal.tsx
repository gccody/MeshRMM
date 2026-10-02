"use client";

import { LoaderCircle, Play, SquareTerminal, X } from "lucide-react";
import { type FormEvent, type RefObject, useEffect, useMemo, useState } from "react";
import { AuthenticationRequired } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { type RunAs, type ScriptRun, type ToolboxScript, LANGUAGE_LABELS, RUN_AS_LABELS, groupByFolder, isRunFinished } from "./model";
import { RunResult, useScriptRun } from "./run-result";
import { fetchToolbox, runScript } from "./toolbox-api";

type Props = {
  agents: Agent[];
  /** The scripts to choose from; loaded when not given. */
  scripts?: ToolboxScript[];
  initialAgentId?: string;
  initialScriptId?: string;
  onClose: () => void;
  returnFocus?: RefObject<HTMLElement | null>;
};

/** Picks a script, a device and an account, runs it, and shows the result. */
export function RunScriptModal({ agents, scripts: givenScripts, initialAgentId, initialScriptId, onClose, returnFocus }: Props) {
  const { authorizedFetch } = useWorkspace();
  const [loadedScripts, setLoadedScripts] = useState<ToolboxScript[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const scripts = givenScripts ?? loadedScripts;
  const [scriptId, setScriptId] = useState(initialScriptId ?? "");
  const [agentId, setAgentId] = useState(initialAgentId ?? "");
  const [runAs, setRunAs] = useState<RunAs>("user");
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [started, setStarted] = useState<ScriptRun | null>(null);
  const { run, error: followError } = useScriptRun(authorizedFetch, started);

  useEffect(() => {
    if (givenScripts) return;
    let cancelled = false;
    fetchToolbox(authorizedFetch)
      .then((toolbox) => { if (!cancelled) setLoadedScripts(toolbox.scripts); })
      .catch((error: unknown) => {
        if (!cancelled && !(error instanceof AuthenticationRequired)) {
          setLoadError(error instanceof Error ? error.message : "Scripts could not be loaded.");
        }
      });
    return () => { cancelled = true; };
  }, [authorizedFetch, givenScripts]);

  const sortedAgents = useMemo(() => [...agents].sort((left, right) => left.name.localeCompare(right.name)), [agents]);
  const folders = useMemo(() => groupByFolder(scripts ?? []), [scripts]);
  const script = scripts?.find((candidate) => candidate.id === scriptId);
  const agent = agents.find((candidate) => candidate.id === agentId);
  const canRun = Boolean(script && agent?.connected && !starting);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!script || !agent) return;
    setStarting(true);
    setStartError(null);
    try {
      setStarted(await runScript(authorizedFetch, agent.id, script.id, runAs));
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) {
        setStartError(error instanceof Error ? error.message : "The script could not be run.");
      }
    } finally {
      setStarting(false);
    }
  };

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="run-script-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><SquareTerminal size={22} /></div>
      <p className="eyebrow">Toolbox</p>
      <h2 id="run-script-title">Run a script</h2>
      {run ? (
        <>
          <RunResult run={run} deviceName={agent?.name ?? run.device_id} error={followError} />
          <div className="connection-reason-actions">
            <button type="button" className="secondary-button" onClick={() => setStarted(null)} disabled={!isRunFinished(run)}>Run again</button>
            <button type="button" className="primary-button" onClick={onClose}>Done</button>
          </div>
        </>
      ) : (
        <>
          <p>The script runs on the device in the background. Its output appears here when it finishes.</p>
          <form onSubmit={(event) => void submit(event)}>
            <label htmlFor="run-script">Script
              <select id="run-script" value={scriptId} onChange={(event) => setScriptId(event.target.value)} disabled={!scripts}>
                <option value="" disabled>{scripts ? (scripts.length ? "Choose a script" : "No scripts yet") : "Loading scripts…"}</option>
                {folders.map(({ folder, items }) => (
                  <optgroup key={folder} label={folder || "Top level"}>
                    {items.map((item) => <option key={item.id} value={item.id}>{item.name} ({LANGUAGE_LABELS[item.language]})</option>)}
                  </optgroup>
                ))}
              </select>
            </label>
            {script?.description && <small className="field-help">{script.description}</small>}
            {loadError && <p role="alert" className="installer-error">{loadError}</p>}
            <label htmlFor="run-device">Device
              <select id="run-device" value={agentId} onChange={(event) => setAgentId(event.target.value)}>
                <option value="" disabled>Choose a device</option>
                {sortedAgents.map((candidate) => (
                  <option key={candidate.id} value={candidate.id} disabled={!candidate.connected}>
                    {candidate.name}{candidate.connected ? "" : " (offline)"}
                  </option>
                ))}
              </select>
            </label>
            <fieldset className="run-as-choice">
              <legend>Run as</legend>
              {(["user", "system"] as const).map((choice) => (
                <label key={choice}>
                  <input type="radio" name="run-as" value={choice} checked={runAs === choice} onChange={() => setRunAs(choice)} />
                  <span><strong>{RUN_AS_LABELS[choice]}</strong>{choice === "user" ? "The person signed in to the device. If nobody is, the script runs as SYSTEM." : "The device's system account, with full control of the computer."}</span>
                </label>
              ))}
            </fieldset>
            {startError && <p role="alert" className="installer-error">{startError}</p>}
            <div className="connection-reason-actions">
              <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
              <button className="primary-button" disabled={!canRun}>{starting ? <LoaderCircle size={16} className="spin" /> : <Play size={16} />} Run script</button>
            </div>
          </form>
        </>
      )}
    </ModalDialog>
  );
}

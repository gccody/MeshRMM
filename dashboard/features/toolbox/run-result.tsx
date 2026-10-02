"use client";

import { CircleAlert, CircleCheck, LoaderCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { AuthenticationRequired, type AuthorizedFetch } from "../../lib/http";
import {
  LANGUAGE_LABELS,
  LOST_RUN_EXPLANATION,
  type ScriptRun,
  isRunFinished,
  ranAsLabel,
  runOutcome,
  runTone,
} from "./model";
import { fetchRun } from "./toolbox-api";

const POLL_INTERVAL_MS = 1500;

/**
 * Follows a run until it finishes, starting from `initial`, and loads its
 * output. A run that is already finished is read once for its output.
 */
export function useScriptRun(authorizedFetch: AuthorizedFetch, initial: ScriptRun | null) {
  const [run, setRun] = useState<ScriptRun | null>(initial);
  const [error, setError] = useState<string | null>(null);
  const [source, setSource] = useState(initial);
  if (initial !== source) {
    setSource(initial);
    setRun(initial);
    setError(null);
  }

  const id = run?.id;
  const pending = run !== null && !isRunFinished(run);
  // Lists leave output out, so a finished run from a list is read once.
  const needsOutput = run !== null && isRunFinished(run) && run.stdout === "" && run.stderr === "" && run === initial;
  useEffect(() => {
    if (!id || (!pending && !needsOutput)) return;
    let cancelled = false;
    let timer: number | undefined;
    const load = async () => {
      try {
        const next = await fetchRun(authorizedFetch, id);
        if (cancelled) return;
        setError(null);
        setRun(next);
        if (!isRunFinished(next)) timer = window.setTimeout(() => void load(), POLL_INTERVAL_MS);
      } catch (loadError) {
        if (cancelled || loadError instanceof AuthenticationRequired) return;
        setError(loadError instanceof Error ? loadError.message : "The run could not be loaded.");
        timer = window.setTimeout(() => void load(), POLL_INTERVAL_MS * 2);
      }
    };
    timer = window.setTimeout(() => void load(), needsOutput ? 0 : POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [authorizedFetch, id, pending, needsOutput]);

  return { run, error };
}

const formatTime = (unixMs: number) =>
  new Date(unixMs).toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" });

/** A run's outcome and output. */
export function RunResult({ run, deviceName, error }: { run: ScriptRun; deviceName: string; error?: string | null }) {
  const tone = runTone(run);
  const Icon = tone === "pending" ? LoaderCircle : tone === "success" ? CircleCheck : CircleAlert;
  return (
    <section className="run-result" aria-live="polite">
      <div className={`run-outcome run-outcome-${tone}`}>
        <Icon size={18} className={tone === "pending" ? "spin" : undefined} aria-hidden="true" />
        <div>
          <strong>{runOutcome(run)}</strong>
          <span>{run.script_name} on {deviceName}</span>
        </div>
      </div>
      <dl className="run-facts">
        <div><dt>Ran as</dt><dd>{ranAsLabel(run)}</dd></div>
        <div><dt>Interpreter</dt><dd>{LANGUAGE_LABELS[run.language]}</dd></div>
        <div><dt>Started</dt><dd>{formatTime(run.created_at_unix_ms)}</dd></div>
        {run.completed_at_unix_ms !== undefined && <div><dt>Finished</dt><dd>{formatTime(run.completed_at_unix_ms)}</dd></div>}
      </dl>
      {run.status === "lost" && <p className="run-note">{LOST_RUN_EXPLANATION}</p>}
      {run.error && <p className="run-note run-note-problem">{run.error}</p>}
      {error && <p className="run-note run-note-problem" role="alert">{error} Retrying…</p>}
      {run.status === "pending" ? (
        <p className="run-note">Waiting for the device to finish. Output appears when the script exits.</p>
      ) : (
        <>
          <RunOutput label="Output" text={run.stdout} empty="The script wrote no output." />
          {run.stderr && <RunOutput label="Errors" text={run.stderr} problem />}
          {run.output_truncated && <p className="run-note">Output was cut at 512 KiB per stream.</p>}
        </>
      )}
    </section>
  );
}

function RunOutput({ label, text, empty, problem = false }: { label: string; text: string; empty?: string; problem?: boolean }) {
  return (
    <div className="run-output">
      <span>{label}</span>
      <pre className={problem ? "run-output-problem" : undefined}>{text || empty}</pre>
    </div>
  );
}

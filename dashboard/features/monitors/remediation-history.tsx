import { CircleAlert, LoaderCircle } from "lucide-react";
import { useState } from "react";
import { formatDateTime, formatRelative } from "../../lib/format";
import { AuthenticationRequired, errorText } from "../../lib/http";
import type { Alert } from "../alerts/model";
import { useWorkspace } from "../workspace/workspace-context";
import {
  ATTEMPT_STATUS_LABELS,
  REMEDIATION_LABELS,
  REMEDIATION_STATE_LABELS,
  RUN_AS_LABELS,
  type RemediationAttempt,
  attemptTone,
} from "./model";
import { fetchRemediations } from "./monitors-api";

// A monitor's output on its alert, and its self-healing attempts, each
// behind a disclosure; the attempts load when it opens.
export function AlertDetails({ alert }: { alert: Alert }) {
  const { authorizedFetch } = useWorkspace();
  const [attempts, setAttempts] = useState<RemediationAttempt[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const healing = Boolean(alert.remediation_state || alert.remediation_attempts);

  const load = async () => {
    setError(null);
    try {
      setAttempts(await fetchRemediations(authorizedFetch, alert.id));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The self-healing attempts could not be loaded."));
    }
  };

  if (!alert.output && !healing) return null;
  const count = alert.remediation_attempts ?? 0;
  return (
    <>
      {alert.output && (
        <details className="alert-output">
          <summary>Output</summary>
          <pre>{alert.output}</pre>
        </details>
      )}
      {healing && (
        <details className="alert-output" onToggle={(event) => { if (event.currentTarget.open) void load(); }}>
          <summary>
            {alert.remediation_state ? REMEDIATION_STATE_LABELS[alert.remediation_state] : "Self-healing"}
            {count > 0 && ` · ${count} ${count === 1 ? "attempt" : "attempts"}`}
          </summary>
          {error ? <p className="form-error" role="alert"><CircleAlert size={15} aria-hidden="true" />{error}</p>
            : !attempts ? <p role="status"><LoaderCircle size={15} className="spin" aria-hidden="true" /> Loading attempts…</p>
              : !attempts.length ? <p className="field-help">No attempts yet.</p>
                : (
                  <ol className="remediation-attempts">
                    {attempts.map((attempt) => (
                      <li key={attempt.id}>
                        <span className={`run-pill run-pill-${attemptTone(attempt.status)}`}>{ATTEMPT_STATUS_LABELS[attempt.status]}</span>
                        <strong>
                          {attempt.attempt}. {attempt.action === "script" ? `Ran “${attempt.target || "a script"}”` : REMEDIATION_LABELS[attempt.action]}
                          {attempt.action !== "script" && attempt.target ? ` “${attempt.target}”` : ""}
                          {attempt.run_as ? ` as ${RUN_AS_LABELS[attempt.run_as]}` : ""}
                        </strong>
                        <span title={formatDateTime(attempt.started_at)}>
                          {formatRelative(attempt.started_at)}
                          {attempt.exit_code !== null && ` · exit code ${attempt.exit_code}`}
                          {attempt.detail && ` · ${attempt.detail}`}
                        </span>
                        {attempt.output && <pre>{attempt.output}</pre>}
                      </li>
                    ))}
                  </ol>
                )}
        </details>
      )}
    </>
  );
}

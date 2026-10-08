import { ClipboardList, LoaderCircle, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { AuthenticationRequired, errorText, expectJson } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { useWorkspace } from "../workspace/workspace-context";
import { AUDIT_CATEGORIES, type AuditEvent, type AuditPage, eventLabel, targetLabel } from "./model";

const PAGE_SIZE = 50;

// What happened on this server, newest first.
export function AuditPanel() {
  const { authorizedFetch } = useWorkspace();
  const [category, setCategory] = useState("");
  const [events, setEvents] = useState<AuditEvent[] | null>(null);
  const [next, setNext] = useState<string | undefined>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [attempt, setAttempt] = useState(0);

  const fetchPage = useCallback(async (before?: string) => {
    const query = new URLSearchParams({ limit: String(PAGE_SIZE) });
    if (category) query.set("action", category);
    if (before) query.set("before", before);
    return expectJson<AuditPage>(await authorizedFetch(`/v1/audit?${query}`), "The audit log could not be loaded.");
  }, [authorizedFetch, category]);

  // The newest page, again whenever the filter changes or Refresh is chosen.
  useEffect(() => {
    let cancelled = false;
    fetchPage().then(
      (page) => {
        if (cancelled) return;
        setEvents(page.events);
        setNext(page.next);
        setError(null);
      },
      (failure: unknown) => {
        if (!cancelled && !(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The audit log could not be loaded."));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [fetchPage, attempt]);

  const loadMore = async () => {
    if (!next) return;
    setBusy(true);
    try {
      const page = await fetchPage(next);
      setEvents((current) => [...(current ?? []), ...page.events]);
      setNext(page.next);
      setError(null);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "More events could not be loaded."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="agent-panel">
      <div className="panel-header">
        <div><h2>Events</h2><span>{events ? `${events.length}${next ? "+" : ""} shown` : "Loading…"}</span></div>
        <div className="heading-actions">
          <button type="button" className="secondary-button" onClick={() => setAttempt((count) => count + 1)} disabled={busy}><RefreshCw size={16} /> Refresh</button>
        </div>
      </div>
      <div className="table-toolbar">
        <label className="status-filter audit-filter">
          <span className="sr-only">Show</span>
          <select value={category} onChange={(event) => setCategory(event.target.value)} aria-label="Show events about">
            {AUDIT_CATEGORIES.map(({ prefix, label }) => <option key={prefix} value={prefix}>{label}</option>)}
          </select>
        </label>
      </div>
      {error && <p role="alert" className="form-error table-message">{error}</p>}
      {events && (
        <table className="data-table audit-table" aria-label="Audit events">
          <thead><tr><th scope="col">When</th><th scope="col">Who</th><th scope="col">What</th><th scope="col">Address</th></tr></thead>
          <tbody>
            {events.map((event) => (
              <tr key={event.id}>
                <td className="nowrap">{formatDateTime(event.created_at)}</td>
                <td>{event.actor_label}</td>
                <td>
                  <strong>{eventLabel(event)}</strong>
                  <span>{targetLabel(event)}</span>
                  {Object.keys(event.metadata).length > 0 && (
                    <details className="audit-details"><summary>Details</summary><pre>{JSON.stringify(event.metadata, null, 2)}</pre></details>
                  )}
                </td>
                <td>{event.ip ?? <span className="muted-text">—</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {events && !events.length && <div className="empty-state"><ClipboardList size={28} /><strong>No events</strong><span>Nothing matches this filter yet.</span></div>}
      {!events && !error && <p role="status" className="field-help table-message"><LoaderCircle size={14} className="spin" /> Loading…</p>}
      {next && <div className="panel-footer"><button type="button" className="secondary-button" onClick={() => void loadMore()} disabled={busy}>{busy && <LoaderCircle size={15} className="spin" />} Show older events</button></div>}
    </section>
  );
}

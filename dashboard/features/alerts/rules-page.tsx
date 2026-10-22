import { CircleAlert, ListChecks, LoaderCircle, Pencil, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { useCallback, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { deleteRule, fetchChannels, fetchRules } from "./alerts-api";
import { SeverityBadge } from "./alert-table";
import { AlertAreaTabs } from "./area-tabs";
import { type AlertRule, type ChannelList, scopeLabel } from "./model";
import { RuleEditor } from "./rule-editor";

const byName = (left: AlertRule, right: AlertRule) => left.name.localeCompare(right.name, undefined, { sensitivity: "base" });

// `/alert-rules`: what raises alerts, for whom, and who hears about it.
export function RulesPage() {
  const { authorizedFetch, can, inventory } = useWorkspace();
  const canManage = can("alerts.manage");
  const [dialog, setDialog] = useState<{ rule: AlertRule | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [rules, channels] = await Promise.all([
      fetchRules(authorizedFetch),
      canManage ? fetchChannels(authorizedFetch) : Promise.resolve<ChannelList | null>(null),
    ]);
    return { rules: rules.sort(byName), channels };
  }, [authorizedFetch, canManage]);
  const { data, error, reload, setData } = useResource(load, "The alert rules could not be loaded.");
  const companyNames = useMemo(() => new Map(inventory.companies.map((company) => [company.id, company.name])), [inventory.companies]);
  const channelNames = useMemo(() => new Map((data?.channels?.channels ?? []).map((channel) => [channel.id, channel.name])), [data]);

  const open = (rule: AlertRule | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ rule });
  };

  const remove = async (rule: AlertRule) => {
    if (!window.confirm(`Delete the rule “${rule.name}”? Its alerts, open and resolved, are deleted with it.`)) return;
    setBusyId(rule.id);
    setActionError(null);
    try {
      await deleteRule(authorizedFetch, rule.id);
      setData((current) => ({ ...current, rules: current.rules.filter((candidate) => candidate.id !== rule.id) }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The rule could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  const notifies = (rule: AlertRule) => {
    if (!rule.channel_ids.length) return "Nobody";
    if (!data?.channels) return `${rule.channel_ids.length} ${rule.channel_ids.length === 1 ? "channel" : "channels"}`;
    return rule.channel_ids.map((id) => channelNames.get(id) ?? "Deleted channel").join(", ");
  };

  return (
    <>
      <HeaderActions>
        {canManage && data && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New rule</button>}
      </HeaderActions>
      <AlertAreaTabs current="alert-rules" />
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><CircleAlert size={22} aria-hidden="true" /><strong>{error}</strong><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></div>
            : <div className="empty-state"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading rules…</strong></div>
        ) : data.rules.length === 0 ? (
          <div className="empty-state">
            <ListChecks size={22} aria-hidden="true" />
            <strong>No alert rules yet</strong>
            <span>{canManage
              ? "Create a rule to raise an alert when a device goes offline, runs low on disk space, stays busy, or a script fails."
              : "Ask an administrator to create one."}</span>
          </div>
        ) : (
          <table className="data-table" aria-label="Alert rules">
            <thead>
              <tr>
                <th scope="col">Rule</th>
                <th scope="col">Severity</th>
                <th scope="col">Applies to</th>
                <th scope="col">Notifies</th>
                <th scope="col"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {data.rules.map((rule) => (
                <tr key={rule.id} className={rule.enabled ? undefined : "row-disabled"}>
                  <td>
                    <strong>{rule.name}{!rule.enabled && <span className="badge badge-muted">Off</span>}</strong>
                    <span>{rule.description}</span>
                  </td>
                  <td><SeverityBadge severity={rule.severity} /></td>
                  <td>{scopeLabel(rule, companyNames, inventory.tags)}</td>
                  <td>{notifies(rule)}{rule.channel_ids.length > 0 && rule.notify_on_resolve && <span>Also when resolved</span>}</td>
                  <td className="row-actions">
                    {canManage && <>
                      <button type="button" className="close-session-button" onClick={(event) => open(rule, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${rule.name}`} title="Edit rule"><Pencil size={16} /></button>
                      <button type="button" className="agent-delete-button" disabled={busyId === rule.id} onClick={() => void remove(rule)} aria-label={`Delete ${rule.name}`} title="Delete rule">{busyId === rule.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>
                    </>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {dialog && data && (
        <RuleEditor
          rule={dialog.rule}
          channels={data.channels?.channels ?? []}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onSaved={(saved) => {
            setData((current) => ({ ...current, rules: [...current.rules.filter((candidate) => candidate.id !== saved.id), saved].sort(byName) }));
            setDialog(null);
          }}
        />
      )}
    </>
  );
}

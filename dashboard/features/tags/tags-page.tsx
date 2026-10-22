import { CircleAlert, LoaderCircle, Pencil, Plus, Search, Tags, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { DEFAULT_DEVICE_FILTERS, serializeDeviceFilters } from "../agents/device-filters";
import type { Tag } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { type TagSummary, matchesTagName } from "./model";
import { TagChip } from "./tag-chip";
import { TagDialog } from "./tag-dialogs";
import { deleteTag, fetchTags } from "./tags-api";

type Counts = { total: number; online: number };

const devicesWith = (tag: string) => `/${serializeDeviceFilters({ ...DEFAULT_DEVICE_FILTERS, tags: [tag] })}`;

const describe = ({ total, online }: Counts) =>
  total === 0 ? "No devices" : `${total} ${total === 1 ? "device" : "devices"} · ${online} online`;

const count = (value: number, one: string, many: string) => `${value} ${value === 1 ? one : many}`;

// What targets a tag: its policies and alert rules, in words.
function targetedBy(summary: TagSummary | undefined) {
  if (!summary) return null;
  const parts = [];
  if (summary.policy_count) parts.push(count(summary.policy_count, "policy", "policies"));
  if (summary.rule_count) parts.push(count(summary.rule_count, "alert rule", "alert rules"));
  return parts.length ? parts.join(" and ") : null;
}

// The tags devices carry, with how many devices have each and what targets
// it. The tags and device counts come from the live inventory; the policies
// and rules that use each tag from the server.
export function TagsPage() {
  const { authorizedFetch, can, inventory } = useWorkspace();
  const canManage = can("tags.manage");
  const [dialog, setDialog] = useState<{ tag: Tag | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [summaries, setSummaries] = useState<ReadonlyMap<string, TagSummary>>(() => new Map());
  const [query, setQuery] = useState("");
  const opener = useRef<HTMLElement | null>(null);
  const { agents, tags, hasData } = inventory;

  // Policies and rules change rarely; reading them again whenever the tags
  // change keeps the counts current enough.
  useEffect(() => {
    let current = true;
    fetchTags(authorizedFetch).then(
      (listed) => { if (current) setSummaries(new Map(listed.map((summary) => [summary.id, summary]))); },
      (failure: unknown) => { if (current && !(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The tags could not be loaded.")); },
    );
    return () => { current = false; };
  }, [authorizedFetch, tags]);

  const counts = useMemo(() => {
    const result = new Map<string, Counts>();
    for (const agent of agents) {
      for (const id of agent.tag_ids ?? []) {
        const count = result.get(id) ?? { total: 0, online: 0 };
        count.total += 1;
        if (agent.connected) count.online += 1;
        result.set(id, count);
      }
    }
    return result;
  }, [agents]);
  const countOf = (id: string) => counts.get(id) ?? { total: 0, online: 0 };
  const shown = tags.filter((tag) => matchesTagName(tag.name, query));

  const open = (tag: Tag | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ tag });
  };

  const remove = async (tag: Tag) => {
    const { total } = countOf(tag.id);
    const parts = [`Delete the tag “${tag.name}”?`];
    if (total) parts.push(total === 1 ? "Its device loses it." : `Its ${total} devices lose it.`);
    const targets = targetedBy(summaries.get(tag.id));
    if (targets) parts.push(`${targets} target it and will no longer cover its devices.`);
    if (!window.confirm(parts.join(" "))) return;
    setBusyId(tag.id);
    setActionError(null);
    try {
      await deleteTag(authorizedFetch, tag.id);
      if (inventory.connection !== "live") void inventory.loadAgents();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setActionError(errorText(failure, "The tag could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  if (!hasData) {
    return <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading tags…</p></section>;
  }

  return (
    <>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <HeaderActions>
        {tags.length > 0 && <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search tags" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search tags" /></label>}
        {canManage && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New tag</button>}
      </HeaderActions>
      <section className="agent-panel">
        {tags.length === 0 ? (
          <div className="empty-state">
            <Tags size={22} aria-hidden="true" />
            <strong>No tags yet</strong>
            <span>{canManage
              ? "Create tags for any grouping besides companies, such as servers, a site or an owner, then tag devices from the Devices page."
              : "Ask an administrator to create some."}</span>
          </div>
        ) : shown.length === 0 ? (
          <div className="empty-state">
            <Tags size={22} aria-hidden="true" />
            <strong>No matching tags</strong>
            <button type="button" className="secondary-button" onClick={() => setQuery("")}>Clear search</button>
          </div>
        ) : (
          <table className="data-table tag-table" aria-label="Tags">
            <thead><tr><th scope="col">Tag</th><th scope="col">Devices</th><th scope="col">Targeted by</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {shown.map((tag) => (
                <tr key={tag.id}>
                  <td><Link to={devicesWith(tag.id)} className="tag-link" aria-label={`Devices tagged ${tag.name}`}><TagChip tag={tag} /></Link></td>
                  <td>{describe(countOf(tag.id))}</td>
                  <td>{summaries.has(tag.id) ? targetedBy(summaries.get(tag.id)) ?? "Nothing" : "–"}</td>
                  <td className="row-actions">
                    {canManage && <>
                      <button type="button" className="close-session-button" onClick={(event) => open(tag, event.currentTarget)} aria-haspopup="dialog" aria-label={`Edit ${tag.name}`} title="Rename or recolor"><Pencil size={16} /></button>
                      <button type="button" className="agent-delete-button" disabled={busyId === tag.id} onClick={() => void remove(tag)} aria-label={`Delete ${tag.name}`} title="Delete tag">{busyId === tag.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>
                    </>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {dialog && <TagDialog tag={dialog.tag} returnFocus={opener} onClose={() => setDialog(null)} />}
    </>
  );
}

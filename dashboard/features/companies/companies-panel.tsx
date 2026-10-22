import { Building2, CircleAlert, LoaderCircle, Pencil, Plus, Search, Trash2, X } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import { AuthenticationRequired, errorText, expectOk } from "../../lib/http";
import { NO_COMPANY, serializeDeviceFilters, DEFAULT_DEVICE_FILTERS } from "../agents/device-filters";
import { matchesCompanyName } from "../agents/model";
import type { Company } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { GroupingTabs } from "./area-tabs";
import { CompanyDialog } from "./company-dialogs";

type Counts = { total: number; online: number };

const devicesIn = (company: string) => `/${serializeDeviceFilters({ ...DEFAULT_DEVICE_FILTERS, company })}`;

const describe = ({ total, online }: Counts) =>
  total === 0 ? "No devices" : `${total} ${total === 1 ? "device" : "devices"} · ${online} online`;

// The companies devices are grouped into, with how many devices each has.
// Both come from the live inventory.
export function CompaniesPanel() {
  const { authorizedFetch, can, inventory } = useWorkspace();
  const canManage = can("companies.manage");
  const [dialog, setDialog] = useState<{ company: Company | null } | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const opener = useRef<HTMLElement | null>(null);
  const { agents, companies, hasData } = inventory;

  const counts = useMemo(() => {
    const result = new Map<string, Counts>();
    for (const agent of agents) {
      const key = agent.company_id ?? NO_COMPANY;
      const count = result.get(key) ?? { total: 0, online: 0 };
      count.total += 1;
      if (agent.connected) count.online += 1;
      result.set(key, count);
    }
    return result;
  }, [agents]);
  const countOf = (key: string) => counts.get(key) ?? { total: 0, online: 0 };
  const unassigned = countOf(NO_COMPANY);
  const shown = companies.filter((company) => matchesCompanyName(company.name, query));
  const showUnassigned = unassigned.total > 0 && matchesCompanyName("No company", query);

  const open = (company: Company | null, button: HTMLElement) => {
    opener.current = button;
    setDialog({ company });
  };

  const remove = async (company: Company) => {
    const { total } = countOf(company.id);
    const devices = total === 1 ? "Its device stays enrolled" : `Its ${total} devices stay enrolled`;
    if (!window.confirm(`Delete the company “${company.name}”?${total ? ` ${devices}, in no company.` : ""}`)) return;
    setBusyId(company.id);
    setActionError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/companies/${encodeURIComponent(company.id)}`, { method: "DELETE" }), "The company could not be deleted.");
      if (inventory.connection !== "live") void inventory.loadAgents();
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The company could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  if (!hasData) {
    return <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading companies…</p></section>;
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
        {companies.length > 0 && <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search companies" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search companies" /></label>}
        {canManage && <button type="button" className="primary-button" onClick={(event) => open(null, event.currentTarget)} aria-haspopup="dialog"><Plus size={16} /> New company</button>}
      </HeaderActions>
      <GroupingTabs current="companies" />
      <section className="agent-panel">
        {companies.length === 0 ? (
          <div className="empty-state">
            <Building2 size={22} aria-hidden="true" />
            <strong>No companies yet</strong>
            <span>{canManage
              ? "Create a company for each customer or site, then move its devices into it or enroll them straight into it."
              : "Ask an administrator to create one."}</span>
          </div>
        ) : shown.length === 0 && !showUnassigned ? (
          <div className="empty-state">
            <Building2 size={22} aria-hidden="true" />
            <strong>No matching companies</strong>
            <button type="button" className="secondary-button" onClick={() => setQuery("")}>Clear search</button>
          </div>
        ) : (
          <table className="data-table company-table" aria-label="Companies">
            <thead><tr><th scope="col">Company</th><th scope="col">Devices</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {shown.map((company) => (
                <tr key={company.id}>
                  <td><strong><Link to={devicesIn(company.id)}>{company.name}</Link></strong></td>
                  <td>{describe(countOf(company.id))}</td>
                  <td className="row-actions">
                    {canManage && <>
                      <button type="button" className="close-session-button" onClick={(event) => open(company, event.currentTarget)} aria-haspopup="dialog" aria-label={`Rename ${company.name}`} title="Rename company"><Pencil size={16} /></button>
                      <button type="button" className="agent-delete-button" disabled={busyId === company.id} onClick={() => void remove(company)} aria-label={`Delete ${company.name}`} title="Delete company">{busyId === company.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>
                    </>}
                  </td>
                </tr>
              ))}
              {showUnassigned && (
                <tr>
                  <td><strong><Link to={devicesIn(NO_COMPANY)}>No company</Link></strong><span>Devices not in any company</span></td>
                  <td>{describe(unassigned)}</td>
                  <td />
                </tr>
              )}
            </tbody>
          </table>
        )}
      </section>
      {dialog && <CompanyDialog company={dialog.company} returnFocus={opener} onClose={() => setDialog(null)} />}
    </>
  );
}

import { ArrowLeft, Building2, LoaderCircle } from "lucide-react";
import { useEffect, useMemo } from "react";
import { Link, useSearchParams } from "react-router";
import { DEFAULT_DEVICE_FILTERS, serializeDeviceFilters } from "../agents/device-filters";
import { useCompanies, useDevices } from "../agents/use-devices";
import { FieldValues } from "../custom-fields/field-values";
import type { FieldOwner } from "../custom-fields/fields-api";
import { useWorkspace } from "../workspace/workspace-context";

// `/company?id=…`: one company, its devices, and its custom fields, which
// its devices without their own values show.
export function CompanyPage() {
  const { can, inventory, setPageTitle } = useWorkspace();
  const [searchParams] = useSearchParams();
  const companyId = searchParams.get("id") ?? "";
  const companies = useCompanies(inventory.devices);
  const agents = useDevices(inventory.devices);
  const company = companies.find((candidate) => candidate.id === companyId);
  const owner = useMemo<FieldOwner>(() => ({ kind: "company", id: companyId }), [companyId]);
  const devices = agents.filter((agent) => agent.company_id === companyId);
  const online = devices.filter((agent) => agent.connected).length;

  useEffect(() => {
    setPageTitle(company?.name ?? null);
    return () => setPageTitle(null);
  }, [company?.name, setPageTitle]);

  if (!inventory.hasData) {
    return <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading the company…</p></section>;
  }
  if (!company) {
    return (
      <div className="empty-state">
        <Building2 size={22} aria-hidden="true" />
        <strong>Company not found</strong>
        <span>It may have been deleted.</span>
        <Link className="secondary-button" to="/companies">Back to companies</Link>
      </div>
    );
  }
  return (
    <section className="agent-panel company-page">
      <Link to="/companies" className="back-link"><ArrowLeft size={15} aria-hidden="true" />Companies</Link>
      <p className="field-help">
        <Link to={`/${serializeDeviceFilters({ ...DEFAULT_DEVICE_FILTERS, company: company.id })}`}>
          {devices.length === 0 ? "No devices" : `${devices.length} ${devices.length === 1 ? "device" : "devices"}`}
        </Link>
        {devices.length > 0 && ` · ${online} online`}
      </p>
      <FieldValues key={company.id} owner={owner} canEdit={can("companies.manage")} />
    </section>
  );
}

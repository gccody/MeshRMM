import { LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent, Company } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";

const MAX_NAME_LENGTH = 120;

// The live inventory hears about a change from the event socket. While that
// is down, reading the list shows the change at once.
function useInventoryCatchUp() {
  const { inventory } = useWorkspace();
  return () => {
    if (inventory.connection !== "live") void inventory.loadAgents();
  };
}

// Names a new company, or renames one.
export function CompanyDialog({ company, returnFocus, onClose, onSaved }: {
  company: Company | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved?: (company: Company) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const catchUp = useInventoryCatchUp();
  const titleId = useId();
  const [name, setName] = useState(company?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const body = { name: name.trim() };
      const response = company
        ? await authorizedFetch(`/v1/companies/${encodeURIComponent(company.id)}`, jsonBody(body, "PATCH"))
        : await authorizedFetch("/v1/companies", jsonBody(body));
      const saved = await expectJson<Company>(response, "The company could not be saved.");
      catchUp();
      onSaved?.(saved);
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The company could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{company ? `Rename ${company.name}` : "New company"}</h2>
      {!company && <p>Group devices by the company they belong to, such as each customer you support.</p>}
      <form onSubmit={(event) => void submit(event)}>
        <label>Name<input required maxLength={MAX_NAME_LENGTH} value={name} disabled={busy} onChange={(event) => setName(event.target.value)} /></label>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || !name.trim()}>{busy && <LoaderCircle size={16} className="spin" />} {company ? "Rename" : "Create company"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}

// Moves a device to another company, or out of its company.
export function MoveDeviceDialog({ agent, returnFocus, onClose }: {
  agent: Agent;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const catchUp = useInventoryCatchUp();
  const titleId = useId();
  const [companyId, setCompanyId] = useState(agent.company_id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { companies } = inventory;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await expectOk(
        await authorizedFetch(`/v1/agents/${encodeURIComponent(agent.id)}/company`, jsonBody({ company_id: companyId || null }, "PUT")),
        "The device could not be moved.",
      );
      catchUp();
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The device could not be moved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>Move {agent.name}</h2>
      <p>{companies.length ? "Choose the company this device belongs to." : "There are no companies yet. Create one on the Companies page."}</p>
      <form onSubmit={(event) => void submit(event)}>
        <label>Company
          <select value={companyId} disabled={busy} onChange={(event) => setCompanyId(event.target.value)}>
            <option value="">No company</option>
            {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
          </select>
        </label>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || companyId === (agent.company_id ?? "")}>{busy && <LoaderCircle size={16} className="spin" />} Move device</button>
        </div>
      </form>
    </ModalDialog>
  );
}

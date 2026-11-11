import { KeyRound, LoaderCircle, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { useNow } from "../../lib/use-now";
import { useResource } from "../../lib/use-resource";
import { useCompanies } from "../agents/use-devices";
import { GroupingTabs } from "../companies/area-tabs";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { type DeploymentToken, STATE_LABELS, canRevoke, expiryText, lastUsedText, revoked, usesText } from "./model";
import { NewDeploymentTokenDialog } from "./new-token-dialog";

type TokenList = { tokens: DeploymentToken[] };

// Deployment tokens: one installer or command that enrolls any number of
// computers into a company, for Group Policy, an MDM or a script.
export function DeploymentPage() {
  const { authorizedFetch, inventory } = useWorkspace();
  const companies = useCompanies(inventory.devices);
  const load = useCallback(
    async () => expectJson<TokenList>(await authorizedFetch("/v1/deployment-tokens"), "The deployment tokens could not be loaded."),
    [authorizedFetch],
  );
  const { data, error: loadError, reload, setData } = useResource(load, "The deployment tokens could not be loaded.");
  const [creating, setCreating] = useState(false);
  const opener = useRef<HTMLButtonElement>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  const now = useNow(60_000);

  const revoke = async (token: DeploymentToken) => {
    if (!window.confirm(`Revoke “${token.name}”? It enrolls no more computers, and installs still using it fail. The ${token.uses === 1 ? "device" : "devices"} it enrolled stay.`)) return;
    setBusyId(token.id);
    setError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/deployment-tokens/${encodeURIComponent(token.id)}`, { method: "DELETE" }), "The deployment token could not be revoked.");
      setData((current) => ({ tokens: current.tokens.map((candidate) => (candidate.id === token.id ? revoked(candidate) : candidate)) }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The deployment token could not be revoked."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      <HeaderActions>
        <button ref={opener} type="button" className="primary-button" disabled={companies.length === 0} title={companies.length === 0 ? "Create a company first" : undefined} onClick={() => setCreating(true)} aria-haspopup="dialog"><Plus size={16} /> New deployment token</button>
      </HeaderActions>
      <GroupingTabs current="deployment" />
      <section className="agent-panel">
        {!data ? (
          error
            ? <div className="empty-state"><p role="alert" className="form-error">{error}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></div>
            : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading deployment tokens…</p>
        ) : data.tokens.length === 0 ? (
          <div className="empty-state">
            <KeyRound size={22} aria-hidden="true" />
            <strong>No deployment tokens</strong>
            <span>{companies.length === 0
              ? "A deployment token enrolls computers into a company. Create a company first."
              : "Make one to install the Agent on many computers at once with Group Policy, Intune, Jamf or a script. Every computer that runs it joins its company."}</span>
          </div>
        ) : (
          <>
            {error && <p role="alert" className="form-error">{error}</p>}
            <table className="data-table deployment-table" aria-label="Deployment tokens">
              <thead><tr><th scope="col">Token</th><th scope="col">Company</th><th scope="col">Devices</th><th scope="col">Expiry</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
              <tbody>
                {data.tokens.map((token) => (
                  <tr key={token.id}>
                    <td>
                      <strong>{token.name}</strong>
                      <span className={token.state === "active" ? "badge badge-good" : "badge badge-muted"}>{STATE_LABELS[token.state]}</span>
                      <span>Made by {token.created_by.email ?? "a deleted user"} · {formatDateTime(token.created_at)}</span>
                    </td>
                    <td>{token.company_name}</td>
                    <td>{usesText(token)}<span>{lastUsedText(token, now)}</span></td>
                    <td title={formatDateTime(token.revoked_at ?? token.expires_at)}>{expiryText(token, now)}</td>
                    <td className="row-actions">
                      {canRevoke(token) && (
                        <button type="button" className="danger-button" onClick={() => void revoke(token)} disabled={busyId === token.id} aria-label={`Revoke ${token.name}`}>
                          {busyId === token.id ? <LoaderCircle size={15} className="spin" /> : <Trash2 size={15} />} Revoke
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}
      </section>
      {creating && (
        <NewDeploymentTokenDialog
          companies={companies}
          initialCompanyId={companies.length === 1 ? companies[0].id : ""}
          returnFocus={opener}
          onCreated={({ token }) => setData((current) => ({ tokens: [token, ...current.tokens] }))}
          onClose={() => setCreating(false)}
        />
      )}
    </>
  );
}

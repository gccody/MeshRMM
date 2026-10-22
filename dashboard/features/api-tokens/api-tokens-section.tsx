import { KeyRound, LoaderCircle, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { useResource } from "../../lib/use-resource";
import { useNow } from "../../lib/use-now";
import { useWorkspace } from "../workspace/workspace-context";
import { type ApiToken, type ApiTokenList, type CreatedApiToken, expiryText, isExpired, lastUsedText, scopeText } from "./model";
import { NewTokenDialog } from "./new-token-dialog";

// The signed-in user's API tokens, on their account page.
export function ApiTokensSection() {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(
    async () => expectJson<ApiTokenList>(await authorizedFetch("/v1/api-tokens"), "Your API tokens could not be loaded."),
    [authorizedFetch],
  );
  const { data, error: loadError, reload, setData } = useResource(load, "Your API tokens could not be loaded.");
  const [creating, setCreating] = useState(false);
  const opener = useRef<HTMLButtonElement>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;

  const revoke = async (token: ApiToken) => {
    if (!window.confirm(`Revoke “${token.name}”? Anything using it stops working at once.`)) return;
    setBusyId(token.id);
    setError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/api-tokens/${encodeURIComponent(token.id)}`, { method: "DELETE" }), "The token could not be revoked.");
      setData((current) => ({ ...current, tokens: current.tokens.filter((candidate) => candidate.id !== token.id) }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The token could not be revoked."));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2>API tokens</h2><p>For scripts and integrations that call the MeshRMM API as you.</p></div>
      {!data ? (
        error
          ? <div className="form-stack"><p role="alert" className="form-error">{error}</p><div><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></div></div>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <div className="form-stack">
          {!data.policy.enabled && <p className="field-help field-problem">API tokens are turned off on this server, so none of these work. An administrator can turn them on in Authentication.</p>}
          {data.tokens.length > 0 ? <TokenList tokens={data.tokens} busyId={busyId} onRevoke={(token) => void revoke(token)} /> : <p className="field-help">You have no API tokens.</p>}
          {error && <p role="alert" className="form-error">{error}</p>}
          {data.policy.enabled && (
            <div><button ref={opener} type="button" className="secondary-button" onClick={() => setCreating(true)}><Plus size={15} /> New API token</button></div>
          )}
        </div>
      )}
      {creating && data && (
        <NewTokenDialog
          policy={data.policy}
          permissions={data.permissions}
          returnFocus={opener}
          onCreated={(created) => setData((current) => ({ ...current, tokens: [listed(created), ...current.tokens] }))}
          onClose={() => setCreating(false)}
        />
      )}
    </section>
  );
}

// A new token as the list shows it, without its secret.
function listed({ id, name, user, permissions, created_at, expires_at, last_used_at, last_used_ip }: CreatedApiToken): ApiToken {
  return { id, name, user, permissions, created_at, expires_at, last_used_at, last_used_ip };
}

// Tokens with what they may do, when they were used and when they expire.
// `showUser` names each one's user, for administrators.
export function TokenList({ tokens, busyId, onRevoke, showUser = false }: {
  tokens: ApiToken[];
  busyId: string | null;
  onRevoke: (token: ApiToken) => void;
  showUser?: boolean;
}) {
  const now = useNow(60_000);
  return (
    <ul className="session-list api-token-list">
      {tokens.map((token) => (
        <li key={token.id}>
          <div>
            <strong><KeyRound size={14} aria-hidden="true" /> {token.name}{isExpired(token, now) && <span className="badge badge-warn">Expired</span>}{token.permissions !== null && <span className="badge badge-muted" title={token.permissions.join(", ")}>Limited</span>}</strong>
            {showUser && <span>{token.user.display_name} ({token.user.email})</span>}
            <span>{scopeText(token)}</span>
            <span title={token.expires_at === null ? undefined : formatDateTime(token.expires_at)}>Made {formatDateTime(token.created_at)} · {expiryText(token, now)} · {lastUsedText(token, now)}</span>
          </div>
          <button type="button" className="danger-button" onClick={() => onRevoke(token)} disabled={busyId === token.id} aria-label={`Revoke ${token.name}`}>
            {busyId === token.id ? <LoaderCircle size={15} className="spin" /> : <Trash2 size={15} />} Revoke
          </button>
        </li>
      ))}
    </ul>
  );
}

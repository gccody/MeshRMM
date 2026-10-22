import { LoaderCircle, RefreshCw } from "lucide-react";
import { type FormEvent, useCallback, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { useResource } from "../../lib/use-resource";
import { useWorkspace } from "../workspace/workspace-context";
import { TokenList } from "./api-tokens-section";
import type { ApiToken, ApiTokenList, ApiTokenPolicy } from "./model";

const MAX_LIFETIME_DAYS = 3650;

// The API token policy (authentication.manage) and every user's tokens
// (api_tokens.manage), on the Authentication page.
export function TokenAdministration() {
  const { can } = useWorkspace();
  return (
    <>
      {can("authentication.manage") && <TokenPolicy />}
      {can("api_tokens.manage") && <EveryonesTokens />}
    </>
  );
}

function TokenPolicy() {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(
    async () => {
      const settings = await expectJson<{ api_tokens_enabled: boolean; api_token_max_lifetime_days: number | null }>(
        await authorizedFetch("/v1/settings/authentication"),
        "The API token policy could not be loaded.",
      );
      return { enabled: settings.api_tokens_enabled, max_lifetime_days: settings.api_token_max_lifetime_days } satisfies ApiTokenPolicy;
    },
    [authorizedFetch],
  );
  const { data: saved, error: loadError, reload, setData } = useResource(load, "The API token policy could not be loaded.");
  const [source, setSource] = useState<ApiTokenPolicy | null>(null);
  const [enabled, setEnabled] = useState(true);
  const [limited, setLimited] = useState(true);
  const [days, setDays] = useState("");
  const [busy, setBusy] = useState(false);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;
  const [notice, setNotice] = useState<string | null>(null);

  // Loaded or saved settings replace the form's values.
  if (saved !== source) {
    setSource(saved);
    if (saved) {
      setEnabled(saved.enabled);
      setLimited(saved.max_lifetime_days !== null);
      setDays(String(saved.max_lifetime_days ?? 365));
    }
  }

  const daysValue = Number(days);
  const daysValid = !limited || (Number.isInteger(daysValue) && daysValue >= 1 && daysValue <= MAX_LIFETIME_DAYS);
  const maxDays = limited ? daysValue : null;
  const changed = saved !== null && (enabled !== saved.enabled || maxDays !== saved.max_lifetime_days);
  const shortens = saved !== null && maxDays !== null && (saved.max_lifetime_days === null || maxDays < saved.max_lifetime_days);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!saved || !daysValid) return;
    if (!enabled && saved.enabled && !window.confirm("Turn off API tokens? Every script and integration using one stops working until they're turned on again.")) return;
    if (shortens && !window.confirm(`Limit tokens to ${maxDays} days? Existing tokens that would last longer expire ${maxDays} days after they were made.`)) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const updated = await expectJson<{ api_tokens_enabled: boolean; api_token_max_lifetime_days: number | null }>(
        await authorizedFetch("/v1/settings/authentication", jsonBody({ api_tokens_enabled: enabled, api_token_max_lifetime_days: maxDays }, "PATCH")),
        "The API token policy could not be saved.",
      );
      setData(() => ({ enabled: updated.api_tokens_enabled, max_lifetime_days: updated.api_token_max_lifetime_days }));
      setNotice("API token policy saved.");
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The API token policy could not be saved."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="management-panel">
      <div className="management-heading"><h2>API token policy</h2><p>Personal tokens let scripts and integrations call the API as their user.</p></div>
      {!saved ? (
        error
          ? <div className="form-stack"><p role="alert" className="form-error">{error}</p><div><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></div></div>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <form className="form-stack form-narrow" onSubmit={(event) => void submit(event)}>
          <label className="checkbox-row">
            <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
            <span><strong>Allow API tokens</strong>Off refuses every token until turned on again; users can still see and revoke theirs.</span>
          </label>
          <label className="checkbox-row">
            <input type="checkbox" checked={limited} onChange={(event) => setLimited(event.target.checked)} />
            <span><strong>Limit how long tokens last</strong>Without a limit, users may make tokens that never expire.</span>
          </label>
          {limited && (
            <>
              <label htmlFor="api-token-max-days">Longest lifetime (days)
                <input id="api-token-max-days" type="number" inputMode="numeric" min={1} max={MAX_LIFETIME_DAYS} step={1} required value={days} onChange={(event) => setDays(event.target.value)} aria-invalid={!daysValid} aria-describedby="api-token-max-days-help" />
              </label>
              <small id="api-token-max-days-help" className="field-help">1 to {MAX_LIFETIME_DAYS} days. Lowering it shortens existing tokens too.</small>
            </>
          )}
          {error && <p role="alert" className="form-error">{error}</p>}
          {notice && <p role="status" className="form-notice">{notice}</p>}
          <div><button className="primary-button" disabled={busy || !changed || !daysValid}>{busy && <LoaderCircle size={16} className="spin" />} Save API token policy</button></div>
        </form>
      )}
    </section>
  );
}

function EveryonesTokens() {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(
    async () => expectJson<ApiTokenList>(await authorizedFetch("/v1/api-tokens?all=true"), "API tokens could not be loaded."),
    [authorizedFetch],
  );
  const { data, error: loadError, reload, setData } = useResource(load, "API tokens could not be loaded.");
  const [query, setQuery] = useState("");
  const [busyId, setBusyId] = useState<string | null>(null);
  const [actionError, setError] = useState<string | null>(null);
  const error = actionError ?? loadError;

  const revoke = async (token: ApiToken) => {
    if (!window.confirm(`Revoke “${token.name}”, ${token.user.email}'s token? Anything using it stops working at once.`)) return;
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

  const needle = query.trim().toLowerCase();
  const tokens = (data?.tokens ?? []).filter((token) => !needle
    || token.name.toLowerCase().includes(needle)
    || token.user.email.toLowerCase().includes(needle)
    || token.user.display_name.toLowerCase().includes(needle));

  return (
    <section className="management-panel">
      <div className="management-heading"><h2>Everyone&apos;s API tokens</h2><p>Revoke any user&apos;s token, for example when it leaks or its owner leaves.</p></div>
      {!data ? (
        error
          ? <div className="form-stack"><p role="alert" className="form-error">{error}</p><div><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={15} /> Try again</button></div></div>
          : <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> Loading…</p>
      ) : (
        <div className="form-stack">
          {data.tokens.length > 0 && (
            <label htmlFor="api-token-search" className="form-narrow">Find a token
              <input id="api-token-search" type="search" placeholder="Name or user" value={query} onChange={(event) => setQuery(event.target.value)} />
            </label>
          )}
          {tokens.length > 0
            ? <TokenList tokens={tokens} busyId={busyId} onRevoke={(token) => void revoke(token)} showUser />
            : <p className="field-help">{data.tokens.length ? "No tokens match." : "Nobody has an API token."}</p>}
          {error && <p role="alert" className="form-error">{error}</p>}
        </div>
      )}
    </section>
  );
}

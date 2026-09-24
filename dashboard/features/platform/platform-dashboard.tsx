"use client";

import { LoginRequiredError, useAuth } from "../auth/auth-provider";
import { Building2, LoaderCircle, LogOut, Network, Plus, RefreshCw, ShieldCheck, X } from "lucide-react";
import { FormEvent, useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { errorMessage, normalizeServer } from "../../lib/http";
import { useRuntimeConfig } from "../../app/providers";
import { errorAfterFailure, ownerAccessAfterFailure, PlatformRequestError, type OwnerAccess } from "./platform-access";

type PlatformCompany = {
  id: string;
  name: string;
  slug: string | null;
  workos_organization_id: string | null;
  status: "provisioning" | "awaiting_admin" | "active" | "suspended" | "failed";
  initial_admin_email: string | null;
  provisioning_error: string | null;
  created_at: number;
  updated_at: number | null;
};

export function PlatformDashboard() {
  const { serverUrl } = useRuntimeConfig();
  const { isLoading: isAuthLoading, user, signIn, signOut, getAccessToken } = useAuth();
  const [companies, setCompanies] = useState<PlatformCompany[]>([]);
  const [hasOwnerAccess, setHasOwnerAccess] = useState<OwnerAccess>(null);
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [adminEmail, setAdminEmail] = useState("");
  const [isLoading, setIsLoading] = useState(false);
  const [isCreating, setIsCreating] = useState(false);
  const [pendingCompany, setPendingCompany] = useState<string | null>(null);
  const [domainDrafts, setDomainDrafts] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);

  const authorizedFetch = useCallback(async (path: string, init: RequestInit = {}) => {
    let token: string;
    try {
      token = await getAccessToken();
    } catch (tokenError) {
      if (tokenError instanceof LoginRequiredError) throw new PlatformRequestError("Your administrator session has expired.", 401);
      throw tokenError;
    }
    if (!token) throw new PlatformRequestError("Your administrator session has expired.", 401);
    return fetch(`${normalizeServer(serverUrl)}${path}`, {
      ...init,
      headers: { ...init.headers, Authorization: `Bearer ${token}` },
    });
  }, [getAccessToken, serverUrl]);

  // Leaves any current error in place: callers that start a fresh attempt
  // clear it first, and a reload after a failed action must keep its error.
  const loadCompanies = useCallback(async () => {
    if (!user) return;
    setIsLoading(true);
    try {
      const response = await authorizedFetch("/v1/platform/companies");
      if (!response.ok) {
        throw new PlatformRequestError(await errorMessage(response, "Companies could not be loaded."), response.status);
      }
      const data = (await response.json()) as { companies: PlatformCompany[] };
      setHasOwnerAccess(true);
      setCompanies(data.companies);
    } catch (requestError) {
      setHasOwnerAccess((current) => ownerAccessAfterFailure(current, requestError));
      const message = requestError instanceof Error ? requestError.message : "Companies could not be loaded.";
      setError((current) => errorAfterFailure(current, requestError, message));
    } finally {
      setIsLoading(false);
    }
  }, [authorizedFetch, user]);

  useEffect(() => {
    if (isAuthLoading || !user) return;
    const timeout = window.setTimeout(() => void loadCompanies(), 0);
    return () => window.clearTimeout(timeout);
  }, [isAuthLoading, loadCompanies, user]);

  const createCompany = async (event: FormEvent) => {
    event.preventDefault();
    setIsCreating(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/platform/companies", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name, slug, admin_email: adminEmail }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "The company could not be created."));
      setName("");
      setSlug("");
      setAdminEmail("");
      await loadCompanies();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The company could not be created.");
      await loadCompanies();
    } finally {
      setIsCreating(false);
    }
  };

  const mutateCompany = async (company: PlatformCompany, action: "retry" | "suspend" | "activate") => {
    if (action === "suspend" && !window.confirm(`Suspend ${company.name}? Its dashboard and Agents will immediately lose control-plane access.`)) return;
    setPendingCompany(company.id);
    setError(null);
    try {
      const response = await authorizedFetch(`/v1/platform/companies/${encodeURIComponent(company.id)}/${action}`, { method: "POST" });
      if (!response.ok) throw new Error(await errorMessage(response, `The company could not be ${action === "retry" ? "provisioned" : action === "suspend" ? "suspended" : "activated"}.`));
      await loadCompanies();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The company could not be updated.");
    } finally {
      setPendingCompany(null);
    }
  };

  const assignDomain = async (company: PlatformCompany) => {
    const companySlug = domainDrafts[company.id]?.trim();
    if (!companySlug) return;
    setPendingCompany(company.id);
    setError(null);
    try {
      const response = await authorizedFetch(`/v1/platform/companies/${encodeURIComponent(company.id)}/domain`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ slug: companySlug }),
      });
      if (!response.ok) throw new Error(await errorMessage(response, "The company domain could not be assigned."));
      setDomainDrafts((current) => ({ ...current, [company.id]: "" }));
      await loadCompanies();
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "The company domain could not be assigned.");
    } finally {
      setPendingCompany(null);
    }
  };

  if (isAuthLoading) {
    return <main className="platform-auth"><LoaderCircle className="spin" /><span>Checking administrator access</span></main>;
  }

  if (!user) {
    return (
      <main className="platform-auth">
        <section className="signed-out-card">
          <div className="modal-icon"><ShieldCheck size={22} /></div>
          <p className="eyebrow">MeshRMM administration</p>
          <h1>Platform owner sign-in</h1>
          <p>This area is restricted to the configured MeshRMM platform owner.</p>
          <button className="primary-button" onClick={() => void signIn({ state: { returnTo: "/" } })}><ShieldCheck size={16} /> Sign in securely</button>
        </section>
      </main>
    );
  }

  const retryAccessCheck = () => {
    setHasOwnerAccess(null);
    setError(null);
    void loadCompanies();
  };

  if (hasOwnerAccess === null && error) {
    return (
      <main className="platform-auth">
        <section className="signed-out-card">
          <div className="modal-icon"><ShieldCheck size={22} /></div>
          <p className="eyebrow">Unavailable</p>
          <h1>Platform administration could not load</h1>
          <p role="alert">{error}</p>
          <div className="login-actions">
            <button className="primary-button" onClick={retryAccessCheck} disabled={isLoading}><RefreshCw size={16} className={isLoading ? "spin" : ""} /> Try again</button>
            <button className="secondary-button" onClick={() => void signOut({ returnTo: "https://meshrmm.com" })}><LogOut size={15} /> Sign out</button>
          </div>
        </section>
      </main>
    );
  }

  if (hasOwnerAccess === null) {
    return <main className="platform-auth"><LoaderCircle className="spin" /><span>Verifying platform owner access</span></main>;
  }

  if (!hasOwnerAccess) {
    return (
      <main className="platform-auth">
        <section className="signed-out-card">
          <div className="modal-icon"><ShieldCheck size={22} /></div>
          <p className="eyebrow">Restricted</p>
          <h1>Platform owner access required</h1>
          <p role="alert">{error ?? "This account does not have access to platform administration."}</p>
          <div className="login-actions">
            <button className="primary-button" onClick={retryAccessCheck}><RefreshCw size={16} /> Retry access check</button>
            <button className="secondary-button" onClick={() => void signOut({ returnTo: "https://meshrmm.com" })}><LogOut size={15} /> Sign out</button>
          </div>
        </section>
      </main>
    );
  }

  return (
    <main className="platform-page">
      <header className="platform-header">
        <Link className="marketing-brand" href="/"><span className="brand-mark"><Network size={19} /></span>Mesh<span>RMM</span></Link>
        <div><span>{user.email}</span><button className="secondary-button" onClick={() => void signOut({ returnTo: "https://meshrmm.com" })}><LogOut size={15} /> Sign out</button></div>
      </header>
      <div className="platform-content">
        <section className="page-heading">
          <div><p className="eyebrow">Workspace management</p><h1>Companies</h1><p>Create company workspaces and invite their administrators.</p></div>
          <button className="secondary-button" onClick={() => { setError(null); void loadCompanies(); }} disabled={isLoading}><RefreshCw size={16} className={isLoading ? "spin" : ""} /> Refresh</button>
        </section>

        {error && <div className="error-banner" role="alert"><X size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label="Dismiss"><X size={16} /></button></div>}

        <section className="platform-grid">
          <form className="platform-create-card" onSubmit={createCompany}>
            <div className="management-heading"><h2>Create company</h2><p>The slug is permanent after creation.</p></div>
            <label>Company name<input required maxLength={120} value={name} onChange={(event) => setName(event.target.value)} placeholder="Acme Support" /></label>
            <label>Immutable slug<div className="slug-input"><input required minLength={2} maxLength={63} pattern="[a-z0-9](?:[a-z0-9-]*[a-z0-9])?" value={slug} onChange={(event) => setSlug(event.target.value.toLowerCase().replace(/[^a-z0-9-]/g, ""))} placeholder="acme" /><span>.meshrmm.com</span></div></label>
            <label>Company admin email<input required type="email" maxLength={254} value={adminEmail} onChange={(event) => setAdminEmail(event.target.value)} placeholder="admin@acme.com" /></label>
            <button className="primary-button" disabled={isCreating}>{isCreating ? <LoaderCircle size={16} className="spin" /> : <Plus size={16} />} Create and invite admin</button>
          </form>

          <section className="company-list-card">
            <div className="management-heading"><h2>Provisioned companies</h2><p>{companies.length} company workspace{companies.length === 1 ? "" : "s"}</p></div>
            {isLoading && companies.length === 0 ? <div className="company-empty"><LoaderCircle className="spin" /> Loading companies</div> : companies.length === 0 ? <div className="company-empty"><Building2 /> No companies have been created.</div> : (
              <div className="company-list">
                {companies.map((company) => (
                  <article className="company-row" key={company.id}>
                    <div className="workspace-avatar">{company.name.slice(0, 2).toUpperCase()}</div>
                    <div className="company-row-main">
                      <div><strong>{company.name}</strong><span className={`company-status status-${company.status}`}>{company.status.replace("_", " ")}</span></div>
                      {company.slug ? <a href={`https://${company.slug}.meshrmm.com`}>{company.slug}.meshrmm.com</a> : (
                        <div className="legacy-domain-form">
                          <input aria-label={`Domain slug for ${company.name}`} value={domainDrafts[company.id] ?? ""} onChange={(event) => setDomainDrafts((current) => ({ ...current, [company.id]: event.target.value.toLowerCase().replace(/[^a-z0-9-]/g, "") }))} placeholder="assign-slug" />
                          <span>.meshrmm.com</span>
                          <button className="secondary-button" disabled={pendingCompany === company.id || !(domainDrafts[company.id]?.trim())} onClick={() => void assignDomain(company)}>Assign once</button>
                        </div>
                      )}
                      <small>{company.initial_admin_email}</small>
                      {company.provisioning_error && <p>{company.provisioning_error}</p>}
                    </div>
                    <div className="company-row-actions">
                      {(company.status === "failed" || company.status === "provisioning") && <button className="secondary-button" disabled={pendingCompany === company.id} onClick={() => void mutateCompany(company, "retry")}>Retry</button>}
                      {company.status === "suspended" && <button className="secondary-button" disabled={pendingCompany === company.id} onClick={() => void mutateCompany(company, "activate")}>Activate</button>}
                      {!(["suspended", "provisioning"] as string[]).includes(company.status) && <button className="danger-button" disabled={pendingCompany === company.id} onClick={() => void mutateCompany(company, "suspend")}>Suspend</button>}
                    </div>
                  </article>
                ))}
              </div>
            )}
          </section>
        </section>
      </div>
    </main>
  );
}

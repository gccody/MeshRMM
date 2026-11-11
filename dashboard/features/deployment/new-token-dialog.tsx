import { CircleAlert, Download, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, jsonBody } from "../../lib/http";
import { formatDateTime } from "../../lib/format";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Company } from "../agents/types";
import { CopyField } from "../authentication/copy-field";
import { INSTALLER_ASSETS, type WindowsPlatform, downloadEnrolledInstaller, linuxInstallCommand, macInstallCommand, windowsQuietCommand } from "../enrollment/installer";
import { useWorkspace } from "../workspace/workspace-context";
import { type CreatedDeploymentToken, DEFAULT_LIFETIME_DAYS, LIFETIME_CHOICES, MAX_NAME_LENGTH, newTokenProblem, parseLimit } from "./model";

const WINDOWS: readonly WindowsPlatform[] = ["windows-x64", "windows-arm64"];

// Makes a deployment token, then shows what installs the Agent with it on
// each platform, once.
export function NewDeploymentTokenDialog({ companies, initialCompanyId, returnFocus, onCreated, onClose }: {
  companies: readonly Company[];
  initialCompanyId: string;
  returnFocus: RefObject<HTMLElement | null>;
  onCreated: (created: CreatedDeploymentToken) => void;
  onClose: () => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const [name, setName] = useState("");
  const [companyId, setCompanyId] = useState(initialCompanyId);
  const [lifetime, setLifetime] = useState(String(DEFAULT_LIFETIME_DAYS));
  const [limit, setLimit] = useState("");
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<CreatedDeploymentToken | null>(null);
  const problem = newTokenProblem(name, companyId, limit);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setTouched(true);
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const body = { name: name.trim(), company_id: companyId, expires_in_days: Number(lifetime), max_uses: parseLimit(limit) };
      const token = await expectJson<CreatedDeploymentToken>(await authorizedFetch("/v1/deployment-tokens", jsonBody(body)), "The deployment token could not be made.");
      setCreated(token);
      onCreated(token);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The deployment token could not be made."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal api-token-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{created ? `“${created.token.name}” is ready` : "New deployment token"}</h2>
      {created ? <CreatedToken created={created} onClose={onClose} /> : (
        <form onSubmit={(event) => void submit(event)}>
          <p>One installer or command for every computer in a rollout: each one that runs it joins the company. Use it with Group Policy, Intune, Jamf or a script.</p>
          <fieldset className="script-editor-fields" disabled={busy}>
            <label htmlFor="deployment-token-name">Name
              <input id="deployment-token-name" required maxLength={MAX_NAME_LENGTH} placeholder="Acme laptops, spring rollout" value={name} onChange={(event) => setName(event.target.value)} />
            </label>
            <label htmlFor="deployment-token-company">Company
              <select id="deployment-token-company" required value={companyId} onChange={(event) => setCompanyId(event.target.value)}>
                <option value="">Choose a company</option>
                {companies.map((company) => <option key={company.id} value={company.id}>{company.name}</option>)}
              </select>
            </label>
            <label htmlFor="deployment-token-lifetime">Expires after
              <select id="deployment-token-lifetime" value={lifetime} onChange={(event) => setLifetime(event.target.value)}>
                {LIFETIME_CHOICES.map((days) => <option key={days} value={String(days)}>{days === 1 ? "1 day" : `${days} days`}</option>)}
              </select>
            </label>
            <label htmlFor="deployment-token-limit">Most devices it may enroll
              <input id="deployment-token-limit" inputMode="numeric" placeholder="No limit" value={limit} onChange={(event) => setLimit(event.target.value)} />
            </label>
            <small className="field-help">Revoke it, or let it expire or run out, and it enrolls nothing more. Devices it enrolled stay.</small>
          </fieldset>
          {touched && problem && <p role="alert" className="installer-error">{problem}</p>}
          {error && <p role="alert" className="installer-error">{error}</p>}
          <div className="connection-reason-actions">
            <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
            <button className="primary-button" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} Make token</button>
          </div>
        </form>
      )}
    </ModalDialog>
  );
}

// The installers and commands that carry the new token, while it's in hand.
function CreatedToken({ created, onClose }: { created: CreatedDeploymentToken; onClose: () => void }) {
  const { token, bootstrap } = created;
  const [downloading, setDownloading] = useState<WindowsPlatform | null>(null);
  const [error, setError] = useState<string | null>(null);

  const download = async (platform: WindowsPlatform) => {
    setDownloading(platform);
    setError(null);
    try {
      await downloadEnrolledInstaller(platform, bootstrap);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "The Agent installer could not be created.");
    } finally {
      setDownloading(null);
    }
  };

  return (
    <div className="form-stack">
      <p className="form-notice" role="status"><CircleAlert size={15} aria-hidden="true" /> Download or copy what you need now; it won&apos;t be shown again. Anyone who has it can enroll computers into {token.company_name} until {formatDateTime(token.expires_at)}, unless you revoke it first.</p>
      <div className="deployment-downloads">
        {WINDOWS.map((platform) => (
          <button key={platform} type="button" className="secondary-button" disabled={downloading !== null} onClick={() => void download(platform)}>
            {downloading === platform ? <LoaderCircle size={15} className="spin" /> : <Download size={15} />} {INSTALLER_ASSETS[platform].label} installer
          </button>
        ))}
      </div>
      {error && <p role="alert" className="installer-error">{error}</p>}
      <CopyField label="Windows, unattended" value={windowsQuietCommand("windows-x64")} />
      <p className="field-help">Run it as SYSTEM or an administrator, as Group Policy startup scripts and Intune do (use the ARM64 installer&apos;s name on ARM64). It shows no window, exits with 0 once installed, and leaves an Agent that is already installed alone.</p>
      <CopyField label="macOS command" value={macInstallCommand(bootstrap.server, bootstrap, { sudo: false })} />
      <CopyField label="Linux command" value={linuxInstallCommand(bootstrap.server, bootstrap, { sudo: false })} />
      <p className="field-help">They run as root, as Jamf and most MDMs run scripts; put <code>sudo</code> before <code>/bin/sh</code> or <code>sh</code> to run one in a terminal.</p>
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </div>
  );
}

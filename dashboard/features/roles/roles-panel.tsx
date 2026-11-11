import { CircleAlert, LoaderCircle, Plus, RefreshCw, Trash2, X } from "lucide-react";
import { type FormEvent, type RefObject, useCallback, useId, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import type { Company } from "../agents/types";
import type { Permission } from "../auth/types";
import { ADMINISTRATOR_ROLE_ID, type PermissionInfo, type Role, sortRoles } from "../users/model";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import { companyScopeLabel, grantableOnSomeCompanies, groupPermissions, savedPermissions } from "./permissions";

type Dialog = { role: Role | null };

const companyCollator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });

// Roles are named sets of permissions and the companies they reach; users
// hold each permission on the devices of the companies of the roles that
// grant it.
export function RolesPanel() {
  const { authorizedFetch } = useWorkspace();
  const [actionError, setActionError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [roles, permissions, companies] = await Promise.all([
      authorizedFetch("/v1/roles").then((response) => expectJson<Role[]>(response, "Roles could not be loaded.")),
      authorizedFetch("/v1/permissions").then((response) => expectJson<PermissionInfo[]>(response, "Permissions could not be loaded.")),
      // Without devices.view there are no companies to choose from; roles
      // still load, and keep the companies they have.
      authorizedFetch("/v1/companies")
        .then((response) => expectJson<Company[]>(response, "Companies could not be loaded."))
        .catch((error: unknown) => {
          if (error instanceof AuthenticationRequired) throw error;
          return null;
        }),
    ]);
    const sorted = companies ? [...companies].sort((left, right) => companyCollator.compare(left.name, right.name)) : null;
    return { roles: sortRoles(roles), permissions, companies: sorted };
  }, [authorizedFetch]);
  const { data, error: loadError, reload, setData } = useResource(load, "Roles could not be loaded.");
  const setRoles = (update: (roles: Role[]) => Role[]) => setData((current) => ({ ...current, roles: update(current.roles) }));
  const companyNames = useMemo(() => new Map((data?.companies ?? []).map((company) => [company.id, company.name])), [data?.companies]);

  const remove = async (role: Role) => {
    const members = role.member_count === 1 ? "Its 1 member loses" : `Its ${role.member_count} members lose`;
    if (!window.confirm(`Delete the role “${role.name}”?${role.member_count ? ` ${members} its permissions.` : ""}`)) return;
    setBusyId(role.id);
    setActionError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/roles/${encodeURIComponent(role.id)}`, { method: "DELETE" }), "The role could not be deleted.");
      setRoles((current) => current.filter((candidate) => candidate.id !== role.id));
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The role could not be deleted."));
    } finally {
      setBusyId(null);
    }
  };

  if (!data) {
    return loadError
      ? <section className="management-panel account-status"><p role="alert">{loadError}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></section>
      : <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading roles…</p></section>;
  }
  const { roles, permissions, companies } = data;

  return (
    <>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <HeaderActions>
        <button type="button" className="primary-button" onClick={(event) => { opener.current = event.currentTarget; setDialog({ role: null }); }} aria-haspopup="dialog"><Plus size={16} /> New role</button>
      </HeaderActions>
      <section className="agent-panel">
        <table className="data-table" aria-label="Roles">
          <thead><tr><th scope="col">Role</th><th scope="col">Permissions</th><th scope="col">Companies</th><th scope="col">Members</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
          <tbody>
            {roles.map((role) => {
              const administrator = role.id === ADMINISTRATOR_ROLE_ID;
              return (
                <tr key={role.id}>
                  <td><strong>{role.name}{role.builtin && <span className="badge badge-muted">Built-in</span>}</strong><span>{role.description}</span></td>
                  <td>{administrator ? "All" : `${role.permissions.length} of ${permissions.length}`}</td>
                  <td>{companyScopeLabel(role, companyNames)}</td>
                  <td>{role.member_count}</td>
                  <td className="row-actions">
                    <button type="button" className="secondary-button" onClick={(event) => { opener.current = event.currentTarget; setDialog({ role }); }} aria-haspopup="dialog" aria-label={`${administrator ? "View" : "Edit"} ${role.name}`}>{administrator ? "View" : "Edit"}</button>
                    {!role.builtin && <button type="button" className="agent-delete-button" disabled={busyId === role.id} onClick={() => void remove(role)} aria-label={`Delete ${role.name}`} title="Delete role">{busyId === role.id ? <LoaderCircle size={16} className="spin" /> : <Trash2 size={16} />}</button>}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </section>

      {dialog && (
        <RoleEditor
          role={dialog.role}
          permissions={permissions}
          companies={companies}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onSaved={(saved) => {
            setRoles((current) => sortRoles([...current.filter((candidate) => candidate.id !== saved.id), saved]));
            setDialog(null);
          }}
        />
      )}
    </>
  );
}

function RoleEditor({ role, permissions, companies, returnFocus, onClose, onSaved }: {
  // The role to edit, or null for a new one.
  role: Role | null;
  permissions: PermissionInfo[];
  // Every company, or null when they couldn't be loaded.
  companies: Company[] | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (role: Role) => void;
}) {
  const { account, authorizedFetch, refreshAccount } = useWorkspace();
  const titleId = useId();
  const administrator = role?.id === ADMINISTRATOR_ROLE_ID;
  const [name, setName] = useState(role?.name ?? "");
  const [description, setDescription] = useState(role?.description ?? "");
  const [granted, setGranted] = useState<Set<Permission>>(() => new Set(administrator ? permissions.map((permission) => permission.name) : role?.permissions ?? []));
  const [allCompanies, setAllCompanies] = useState(role?.all_companies ?? true);
  const [chosenCompanies, setChosenCompanies] = useState<Set<string>>(() => new Set(role?.company_ids ?? []));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const groups = groupPermissions(permissions);
  const limited = !allCompanies;
  // Companies the role reaches that this page can't name, such as when the
  // list couldn't load: kept as they are.
  const unnamed = [...chosenCompanies].filter((id) => !companies?.some((company) => company.id === id));

  const toggle = (permission: Permission, checked: boolean) => {
    setGranted((current) => {
      const next = new Set(current);
      if (checked) next.add(permission);
      else next.delete(permission);
      return next;
    });
  };

  const toggleCompany = (id: string, checked: boolean) => {
    setChosenCompanies((current) => {
      const next = new Set(current);
      if (checked) next.add(id);
      else next.delete(id);
      return next;
    });
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (administrator) return;
    if (limited && chosenCompanies.size === 0) {
      setError("Choose at least one company, or every company.");
      return;
    }
    setBusy(true);
    setError(null);
    const body = {
      name: name.trim(),
      description: description.trim(),
      permissions: savedPermissions(granted, permissions, allCompanies),
      all_companies: allCompanies,
      company_ids: allCompanies ? [] : [...chosenCompanies],
    };
    try {
      const response = role
        ? await authorizedFetch(`/v1/roles/${encodeURIComponent(role.id)}`, jsonBody(body, "PATCH"))
        : await authorizedFetch("/v1/roles", jsonBody(body));
      const saved = await expectJson<Role>(response, "The role could not be saved.");
      // Changing a role of one's own changes what this website may show.
      if (role && account.roles.some((held) => held.id === role.id)) await refreshAccount();
      onSaved(saved);
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The role could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{role ? role.name : "New role"}</h2>
      {administrator && <p>Administrators can do everything in every company, including what later releases add. This role can&apos;t be changed.</p>}
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={administrator || busy}>
          <label htmlFor="role-name">Name<input id="role-name" required maxLength={80} value={name} onChange={(event) => setName(event.target.value)} /></label>
          <label htmlFor="role-description">Description (optional)<input id="role-description" maxLength={500} value={description} onChange={(event) => setDescription(event.target.value)} /></label>
          <fieldset className="choice-list permission-group">
            <legend>Companies</legend>
            <label><input type="radio" name="role-companies" checked={allCompanies} onChange={() => setAllCompanies(true)} /><span><strong>Every company</strong>Its permissions apply to every device, including devices in no company.</span></label>
            <label><input type="radio" name="role-companies" checked={limited} disabled={!companies?.length && !limited} onChange={() => setAllCompanies(false)} /><span><strong>Only some companies</strong>{companies === null ? "The companies could not be loaded." : companies.length ? "Its permissions apply only to the chosen companies' devices. Permissions that act on the whole server can't be granted." : "There are no companies yet."}</span></label>
          </fieldset>
          {limited && (
            <fieldset className="choice-list picker-list" aria-describedby={`${titleId}-companies-help`}>
              <legend className="sr-only">Chosen companies</legend>
              <p id={`${titleId}-companies-help`} className="field-help">Its members see and act on only these companies&apos; devices, deployment tokens, alerts, updates and jobs. Devices in no company are out of its reach.</p>
              {(companies ?? []).map((company) => (
                <label key={company.id}>
                  <input type="checkbox" checked={chosenCompanies.has(company.id)} onChange={(event) => toggleCompany(company.id, event.target.checked)} />
                  <span>{company.name}</span>
                </label>
              ))}
              {unnamed.length > 0 && <p className="field-help">{unnamed.length === 1 ? "1 more company" : `${unnamed.length} more companies`} this page can&apos;t show stay chosen.</p>}
            </fieldset>
          )}
          {groups.map(({ label, items }) => (
            <fieldset key={label} className="choice-list permission-group">
              <legend>{label}</legend>
              {items.map((permission) => {
                const blocked = limited && !grantableOnSomeCompanies(permission);
                return (
                  <label key={permission.name}>
                    <input type="checkbox" checked={granted.has(permission.name) && !blocked} disabled={blocked} onChange={(event) => toggle(permission.name, event.target.checked)} />
                    <span>
                      <strong><code>{permission.name}</code>{permission.reach === "instance" && <span className="badge badge-muted" title="Acts on what the whole server shares, so only roles for every company can grant it.">Every company only</span>}</strong>
                      {permission.description}
                    </span>
                  </label>
                );
              })}
            </fieldset>
          ))}
        </fieldset>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>{administrator ? "Close" : "Cancel"}</button>
          {!administrator && <button className="primary-button" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />} {role ? "Save role" : "Create role"}</button>}
        </div>
      </form>
    </ModalDialog>
  );
}

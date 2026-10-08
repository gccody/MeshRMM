import { CircleAlert, LoaderCircle, MailPlus, RefreshCw, Search, ShieldCheck, Trash2, UserRound, X } from "lucide-react";
import { type FormEvent, type RefObject, useCallback, useId, useMemo, useRef, useState } from "react";
import { AuthenticationRequired, errorText, expectJson, expectOk, jsonBody } from "../../lib/http";
import { formatDateTime, formatRelative } from "../../lib/format";
import { ModalDialog } from "../../lib/modal-dialog";
import { useResource } from "../../lib/use-resource";
import { useSession } from "../auth/session";
import { useWorkspace } from "../workspace/workspace-context";
import { type CreatedInvitation, type GroupRoleRef, type InvitationView, type ResetLink, type Role, type UserView, groupRoleLabel, matchesUser, sortRoles, sortUsers } from "./model";
import { OneTimeLink } from "./one-time-link";

type Dialog =
  | { kind: "invite" }
  | { kind: "invited"; created: CreatedInvitation }
  | { kind: "user"; user: UserView };

// The people who can sign in, and the invitations not yet accepted.
export function UsersPanel() {
  const { authorizedFetch } = useWorkspace();
  const [actionError, setActionError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const opener = useRef<HTMLElement | null>(null);

  const load = useCallback(async () => {
    const [users, invitations, roles] = await Promise.all([
      authorizedFetch("/v1/users").then((response) => expectJson<UserView[]>(response, "Users could not be loaded.")),
      authorizedFetch("/v1/invitations").then((response) => expectJson<InvitationView[]>(response, "Invitations could not be loaded.")),
      authorizedFetch("/v1/roles").then((response) => expectJson<Role[]>(response, "Roles could not be loaded.")),
    ]);
    return { users: sortUsers(users), invitations, roles: sortRoles(roles) };
  }, [authorizedFetch]);
  const { data, error: loadError, reload, setData } = useResource(load, "Users could not be loaded.");
  const users = data?.users ?? null;
  const invitations = data?.invitations ?? null;
  const roles = data?.roles ?? null;
  const setUsers = (update: (users: UserView[]) => UserView[]) => setData((current) => ({ ...current, users: update(current.users) }));
  const setInvitations = (update: (invitations: InvitationView[]) => InvitationView[]) => setData((current) => ({ ...current, invitations: update(current.invitations) }));

  const open = (next: Dialog, element: HTMLElement) => {
    opener.current = element;
    setActionError(null);
    setDialog(next);
  };

  const replaceUser = (user: UserView | null, id: string) =>
    setUsers((current) => sortUsers(user ? current.map((candidate) => (candidate.id === id ? user : candidate)) : current.filter((candidate) => candidate.id !== id)));

  const renew = async (invitation: InvitationView, element: HTMLElement) => {
    setBusyId(invitation.id);
    setActionError(null);
    try {
      const created = await expectJson<CreatedInvitation>(
        await authorizedFetch(`/v1/invitations/${encodeURIComponent(invitation.id)}/renew`, { method: "POST" }),
        "The invitation could not be renewed.",
      );
      setInvitations((current) => current.map((candidate) => (candidate.id === invitation.id ? created.invitation : candidate)));
      open({ kind: "invited", created }, element);
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The invitation could not be renewed."));
    } finally {
      setBusyId(null);
    }
  };

  const revoke = async (invitation: InvitationView) => {
    if (!window.confirm(`Revoke the invitation for ${invitation.email}? Its link stops working.`)) return;
    setBusyId(invitation.id);
    setActionError(null);
    try {
      await expectOk(await authorizedFetch(`/v1/invitations/${encodeURIComponent(invitation.id)}`, { method: "DELETE" }), "The invitation could not be revoked.");
      setInvitations((current) => current.filter((candidate) => candidate.id !== invitation.id));
    } catch (error) {
      if (!(error instanceof AuthenticationRequired)) setActionError(errorText(error, "The invitation could not be revoked."));
    } finally {
      setBusyId(null);
    }
  };

  const visibleUsers = useMemo(() => users?.filter((user) => matchesUser(user, query)) ?? [], [users, query]);

  if (!users || !invitations || !roles) {
    return loadError
      ? <section className="management-panel account-status"><p role="alert">{loadError}</p><button type="button" className="secondary-button" onClick={reload}><RefreshCw size={16} /> Try again</button></section>
      : <section className="management-panel account-status"><p role="status"><LoaderCircle size={16} className="spin" /> Loading users…</p></section>;
  }

  return (
    <>
      {actionError && (
        <div className="error-banner" role="alert">
          <CircleAlert size={17} aria-hidden="true" /><span>{actionError}</span>
          <button onClick={() => setActionError(null)} aria-label="Dismiss"><X size={16} /></button>
        </div>
      )}
      <section className="agent-panel">
        <div className="panel-header">
          <div><h2>Users</h2><span>{users.length === 1 ? "1 user" : `${users.length} users`}</span></div>
          <div className="heading-actions">
            <button type="button" className="primary-button" onClick={(event) => open({ kind: "invite" }, event.currentTarget)} aria-haspopup="dialog"><MailPlus size={16} /> Invite user</button>
          </div>
        </div>
        <div className="table-toolbar">
          <label className="agent-search"><Search size={18} /><input aria-label="Search users" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search by name, email or role" /></label>
        </div>
        <table className="data-table" aria-label="Users">
          <thead><tr><th scope="col">User</th><th scope="col">Roles</th><th scope="col">Two-factor</th><th scope="col">Last sign-in</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
          <tbody>
            {visibleUsers.map((user) => (
              <tr key={user.id} className={user.disabled ? "row-disabled" : undefined}>
                <td><strong>{user.display_name}</strong><span>{user.email}</span>{user.disabled && <span className="badge badge-muted">Disabled</span>}<IdentityBadges user={user} /></td>
                <td>
                  {user.roles.length || user.group_roles.length ? user.roles.map((role) => role.name).join(", ") : <span className="muted-text">No roles</span>}
                  {user.group_roles.length > 0 && <GroupRoles roles={user.group_roles} />}
                </td>
                <td>{user.two_factor_enabled ? <span className="badge badge-good"><ShieldCheck size={13} aria-hidden="true" /> On</span> : <span className="muted-text">Off</span>}</td>
                <td>{user.last_sign_in_at === null ? <span className="muted-text">Never</span> : <span title={formatDateTime(user.last_sign_in_at)}>{formatRelative(user.last_sign_in_at)}</span>}</td>
                <td className="row-actions"><button type="button" className="secondary-button" onClick={(event) => open({ kind: "user", user }, event.currentTarget)} aria-haspopup="dialog" aria-label={`Manage ${user.display_name}`}>Manage</button></td>
              </tr>
            ))}
          </tbody>
        </table>
        {!visibleUsers.length && <div className="empty-state"><UserRound size={28} /><strong>No matching users</strong><span>Try another name, email or role.</span></div>}
      </section>

      {invitations.length > 0 && (
        <section className="agent-panel">
          <div className="panel-header"><div><h2>Pending invitations</h2><span>{invitations.length === 1 ? "1 invitation" : `${invitations.length} invitations`}</span></div></div>
          <table className="data-table" aria-label="Pending invitations">
            <thead><tr><th scope="col">Email</th><th scope="col">Roles</th><th scope="col">Expires</th><th scope="col"><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {invitations.map((invitation) => (
                <tr key={invitation.id}>
                  <td><strong>{invitation.email}</strong></td>
                  <td>{invitation.roles.map((role) => role.name).join(", ") || <span className="muted-text">No roles</span>}</td>
                  <td>{invitation.expired ? <span className="badge badge-warn">Expired</span> : formatDateTime(invitation.expires_at)}</td>
                  <td className="row-actions">
                    <button type="button" className="secondary-button" disabled={busyId === invitation.id} onClick={(event) => void renew(invitation, event.currentTarget)} title="Send a new link; the old one stops working">{busyId === invitation.id ? <LoaderCircle size={15} className="spin" /> : <RefreshCw size={15} />} New link</button>
                    <button type="button" className="agent-delete-button" disabled={busyId === invitation.id} onClick={() => void revoke(invitation)} aria-label={`Revoke the invitation for ${invitation.email}`} title="Revoke"><Trash2 size={16} /></button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}

      {dialog?.kind === "invite" && (
        <InviteDialog
          roles={roles}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onInvited={(created) => {
            setInvitations((current) => [created.invitation, ...current]);
            setDialog({ kind: "invited", created });
          }}
        />
      )}
      {dialog?.kind === "invited" && <InvitedDialog created={dialog.created} returnFocus={opener} onClose={() => setDialog(null)} />}
      {dialog?.kind === "user" && (
        <UserDialog
          user={dialog.user}
          roles={roles}
          returnFocus={opener}
          onClose={() => setDialog(null)}
          onChanged={(user, id) => {
            replaceUser(user, id);
            if (!user) setDialog(null);
            else setDialog({ kind: "user", user });
          }}
        />
      )}
    </>
  );
}

// Where the account's identity comes from, besides this server.
function IdentityBadges({ user }: { user: Pick<UserView, "sso_linked" | "scim_managed"> }) {
  return (
    <>
      {user.sso_linked && <span className="badge" title="Signs in with single sign-on">SSO</span>}
      {user.scim_managed && <span className="badge" title="Managed by your identity provider">SCIM</span>}
    </>
  );
}

// Roles from identity provider groups, which only the provider changes.
function GroupRoles({ roles }: { roles: GroupRoleRef[] }) {
  const ssoName = useSession().instance?.sign_in.sso?.name;
  return (
    <ul className="role-chips" aria-label="Roles from identity provider groups">
      {roles.map((role) => <li key={`${role.source}:${role.group}:${role.id}`}>{groupRoleLabel(role, ssoName)}</li>)}
    </ul>
  );
}

function RoleChoices({ roles, selected, onChange, disabled = false }: {
  roles: Role[];
  selected: ReadonlySet<string>;
  onChange: (selected: Set<string>) => void;
  disabled?: boolean;
}) {
  const toggle = (id: string, checked: boolean) => {
    const next = new Set(selected);
    if (checked) next.add(id);
    else next.delete(id);
    onChange(next);
  };
  return (
    <fieldset className="choice-list" disabled={disabled}>
      <legend>Roles</legend>
      {roles.map((role) => (
        <label key={role.id}>
          <input type="checkbox" checked={selected.has(role.id)} onChange={(event) => toggle(role.id, event.target.checked)} />
          <span><strong>{role.name}</strong>{role.description}</span>
        </label>
      ))}
    </fieldset>
  );
}

function InviteDialog({ roles, returnFocus, onClose, onInvited }: {
  roles: Role[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onInvited: (created: CreatedInvitation) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const [email, setEmail] = useState("");
  const [selected, setSelected] = useState<Set<string>>(() => new Set(roles.some((role) => role.id === "technician") ? ["technician"] : []));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const response = await authorizedFetch("/v1/invitations", jsonBody({ email: email.trim(), role_ids: [...selected] }));
      onInvited(await expectJson<CreatedInvitation>(response, "The invitation could not be created."));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The invitation could not be created."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><MailPlus size={22} /></div>
      <h2 id={titleId}>Invite a user</h2>
      <p>They get a link to choose their name and password. It works once, for 7 days.</p>
      <form onSubmit={(event) => void submit(event)}>
        <label htmlFor="invite-email">Email<input id="invite-email" type="email" required value={email} onChange={(event) => setEmail(event.target.value)} /></label>
        <RoleChoices roles={roles} selected={selected} onChange={setSelected} />
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <MailPlus size={16} />} Invite</button>
        </div>
      </form>
    </ModalDialog>
  );
}

function InvitedDialog({ created, returnFocus, onClose }: { created: CreatedInvitation; returnFocus: RefObject<HTMLElement | null>; onClose: () => void }) {
  const titleId = useId();
  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><MailPlus size={22} /></div>
      <h2 id={titleId}>{created.emailed ? "Invitation sent" : "Invitation link"}</h2>
      <OneTimeLink delivery={created} recipient={created.invitation.email} expiresAt={created.invitation.expires_at} />
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </ModalDialog>
  );
}

function UserDialog({ user, roles, returnFocus, onClose, onChanged }: {
  user: UserView;
  roles: Role[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  // The changed user, or null once deleted.
  onChanged: (user: UserView | null, id: string) => void;
}) {
  const { account, authorizedFetch, refreshAccount } = useWorkspace();
  const titleId = useId();
  const self = user.id === account.user.id;
  const [name, setName] = useState(user.display_name);
  const [selected, setSelected] = useState<Set<string>>(() => new Set(user.roles.map((role) => role.id)));
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reset, setReset] = useState<ResetLink | null>(null);
  const userPath = `/v1/users/${encodeURIComponent(user.id)}`;
  const rolesChanged = selected.size !== user.roles.length || user.roles.some((role) => !selected.has(role.id));
  const changed = name.trim() !== user.display_name || rolesChanged;

  const run = async (action: string, work: () => Promise<void>) => {
    setBusy(action);
    setError(null);
    setNotice(null);
    try {
      await work();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "That didn't work. Try again."));
    } finally {
      setBusy(null);
    }
  };

  const update = (body: Record<string, unknown>, success: string) => run("save", async () => {
    const updated = await expectJson<UserView>(await authorizedFetch(userPath, jsonBody(body, "PATCH")), "The user could not be saved.");
    onChanged(updated, user.id);
    setNotice(success);
    // Changing one's own roles changes what this page may show.
    if (self) await refreshAccount();
  });

  const save = (event: FormEvent) => {
    event.preventDefault();
    const body: Record<string, unknown> = {};
    if (name.trim() !== user.display_name) body.display_name = name.trim();
    if (rolesChanged) body.role_ids = [...selected];
    void update(body, "Saved.");
  };

  const post = (path: string, action: string, success: string) => run(action, async () => {
    await expectOk(await authorizedFetch(`${userPath}/${path}`, { method: "POST" }), "That didn't work. Try again.");
    setNotice(success);
    if (path === "reset-two-factor") onChanged({ ...user, two_factor_enabled: false, passkeys: 0 }, user.id);
    if (path === "unlink-sso") onChanged({ ...user, sso_linked: false }, user.id);
  });

  const makeResetLink = () => run("reset", async () => {
    setReset(await expectJson<ResetLink>(await authorizedFetch(`${userPath}/password-reset`, { method: "POST" }), "The reset link could not be made."));
  });

  const remove = () => {
    if (!window.confirm(`Delete ${user.display_name} (${user.email})? They are signed out everywhere and can't sign in again. The audit log keeps their actions.`)) return;
    void run("delete", async () => {
      await expectOk(await authorizedFetch(userPath, { method: "DELETE" }), "The user could not be deleted.");
      onChanged(null, user.id);
    });
  };

  return (
    <ModalDialog className="settings-modal user-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <div className="modal-icon"><UserRound size={22} /></div>
      <p className="eyebrow">{user.disabled ? "Disabled user" : self ? "You" : "User"}</p>
      <h2 id={titleId}>{user.display_name}</h2>
      <p>{user.email} · joined {formatDateTime(user.created_at)} · {user.has_password ? "has a password" : "no password"}{user.passkeys > 0 && ` · ${user.passkeys === 1 ? "1 passkey" : `${user.passkeys} passkeys`}`}<IdentityBadges user={user} /></p>
      <form onSubmit={save}>
        <fieldset className="script-editor-fields" disabled={busy !== null}>
          <label htmlFor="user-name">Name<input id="user-name" required value={name} onChange={(event) => setName(event.target.value)} /></label>
          <RoleChoices roles={roles} selected={selected} onChange={setSelected} />
          {user.group_roles.length > 0 && (
            <div className="group-roles">
              <span>From identity provider groups</span>
              <GroupRoles roles={user.group_roles} />
              <small className="field-help">These roles can only be changed at the identity provider, by changing the user&apos;s groups.</small>
            </div>
          )}
        </fieldset>
        {error && <p role="alert" className="installer-error">{error}</p>}
        {notice && <p role="status" className="form-notice">{notice}</p>}
        {reset && <OneTimeLink delivery={reset} recipient={user.email} expiresAt={reset.expires_at} />}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Close</button>
          <button className="primary-button" disabled={busy !== null || !changed}>{busy === "save" && <LoaderCircle size={16} className="spin" />} Save</button>
        </div>
      </form>
      {!self && (
        <div className="user-actions">
          <h3>Access</h3>
          <div className="form-actions">
            <button type="button" className="secondary-button" disabled={busy !== null || user.disabled} onClick={() => void makeResetLink()}>Password reset link</button>
            <button type="button" className="secondary-button" disabled={busy !== null || !user.two_factor_enabled} onClick={() => {
              if (window.confirm(`Remove ${user.display_name}'s authenticator app, passkeys and recovery codes? They sign in with only their password until they set up two-factor authentication again.`)) void post("reset-two-factor", "two-factor", "Two-factor authentication was reset.");
            }}>Reset two-factor</button>
            {user.sso_linked && (
              <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => {
                if (window.confirm(`Unlink ${user.display_name}'s SSO identity? Their next single sign-on links the account again by email address, to whichever identity has it then.`)) void post("unlink-sso", "unlink-sso", "The SSO identity was unlinked.");
              }}>Unlink SSO identity</button>
            )}
            <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => void post("sign-out", "sign-out", "Signed out everywhere.")}>Sign out everywhere</button>
            <button type="button" className="secondary-button" disabled={busy !== null} onClick={() => void update({ disabled: !user.disabled }, user.disabled ? "Enabled." : "Disabled. They were signed out everywhere.")}>{user.disabled ? "Enable" : "Disable"}</button>
            <button type="button" className="danger-button" disabled={busy !== null} onClick={remove}><Trash2 size={15} /> Delete</button>
          </div>
        </div>
      )}
    </ModalDialog>
  );
}

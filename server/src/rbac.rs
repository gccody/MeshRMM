//! Permissions and the roles that grant them.
//!
//! A role is a named set of permissions and the companies they reach: every
//! company (the default), or chosen ones. A user holds a permission on a
//! device when one of their roles grants it and reaches the device's
//! company; [`crate::access::Access`] works that out. A device in no company
//! is reached only by roles that reach every company. The built-in
//! Administrator role holds every permission implicitly, including ones
//! added by later releases, so it can't be emptied, and it always reaches
//! every company.
use std::{collections::BTreeSet, fmt, str::FromStr};

use sea_query::{Cond, Expr, ExprTrait, LockType, Order, Query};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    access::{Access, CompanySet},
    db::{
        self, Executor,
        tables::{
            EffectiveUserRoles, OidcProvider, RoleCompanies, RolePermissions, Roles, Settings,
            UserPasskeys, Users,
        },
    },
};

/// The ID (and `builtin` value) of the role with every permission.
pub const ADMINISTRATOR_ROLE_ID: &str = "administrator";
/// The ID (and `builtin` value) of the editable default role.
pub const TECHNICIAN_ROLE_ID: &str = "technician";

/// What a permission acts on, and so whether a role limited to some
/// companies can grant it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    /// Devices, or what belongs to a company (a deployment token): it is
    /// judged for each device or company, and a role limited to some
    /// companies grants it on their devices only.
    Company,
    /// Something the whole instance shares (users, roles, settings, the
    /// toolbox's shared items, the application library, policies, alert
    /// rules, companies and tags themselves, the audit log): only roles
    /// that reach every company grant it.
    Instance,
}

macro_rules! permissions {
    ($($variant:ident = $name:literal, $reach:ident: $description:literal,)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Permission {
            $($variant,)+
        }

        impl Permission {
            pub const ALL: &[Permission] = &[$(Permission::$variant,)+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Permission::$variant => $name,)+
                }
            }

            pub fn description(self) -> &'static str {
                match self {
                    $(Permission::$variant => $description,)+
                }
            }

            pub fn reach(self) -> Reach {
                match self {
                    $(Permission::$variant => Reach::$reach,)+
                }
            }
        }

        impl FromStr for Permission {
            type Err = UnknownPermission;

            fn from_str(name: &str) -> Result<Self, Self::Err> {
                match name {
                    $($name => Ok(Permission::$variant),)+
                    _ => Err(UnknownPermission(name.to_owned())),
                }
            }
        }
    };
}

// Each permission's name, its [`Reach`] and what it allows. `Company`
// permissions are judged per device (or, for deployment tokens, per
// company), so a role limited to some companies grants them on those
// companies' devices only. `Instance` permissions act on what the whole
// instance shares, so only roles that reach every company grant them; one
// on a limited role would let its holder act beyond its companies (change
// who holds which role, edit a shared script every company runs, or read
// every company's audit trail).
//
// `jobs.run_wide` is `Company`: from a limited role it runs jobs on its own
// companies, or on tags resolved within them, but never on every device.
// `applications.view` is `Company` though it also shows the library, which
// every holder may read; the packages it shows are those of devices in reach.
// `alerts.view` likewise shows every rule but only the alerts of devices in
// reach.
// `custom_fields.manage` is `Instance`: field definitions (and which scripts may
// use them) are shared by every company. Values are set per device under
// `devices.edit` and per company under `companies.manage`, and a secret is
// revealed under `custom_fields.reveal`, which is `Company`. The `tools.*`
// permissions act on one device each, so they are `Company`.
// `reports.view` is `Company`: from a limited role it makes reports only of
// its companies' devices and sees only the reports it made. Schedules
// (`reports.manage`, `Instance`) run on any device and email anyone, so only
// roles over every company manage them.
permissions! {
    DevicesView = "devices.view", Company: "See devices, their status and inventory.",
    DevicesEdit = "devices.edit", Company: "Add and remove devices' tags, edit their notes, and set their custom field values, one device or many.",
    DevicesEnroll = "devices.enroll", Company: "Create Agent installers and deployment tokens to enroll devices, and see and revoke deployment tokens.",
    DevicesDelete = "devices.delete", Company: "Delete devices and uninstall their Agents.",
    DevicesRotateCredentials = "devices.rotate_credentials", Company: "Rotate a device's Agent credential.",
    DevicesWake = "devices.wake", Company: "Wake offline devices with Wake-on-LAN, through online devices on their network.",
    DevicesMaintenance = "devices.maintenance", Company: "Put devices in maintenance, which silences their alerts and holds back their scheduled automation, and end it.",
    CompaniesManage = "companies.manage", Instance: "Create, rename and delete companies, move devices between them, and set companies' custom field values, which their devices inherit.",
    CustomFieldsManage = "custom_fields.manage", Instance: "Create, change, reorder and delete custom fields, including which fields toolbox scripts may read and write.",
    CustomFieldsReveal = "custom_fields.reveal", Company: "Reveal the values of secret custom fields. Each reveal is recorded in the audit log.",
    TagsManage = "tags.manage", Instance: "Create, rename, recolor and delete tags.",
    SessionsConnect = "sessions.connect", Company: "Start remote sessions.",
    SessionsConnectBackground = "sessions.connect_background", Company: "Start remote sessions in the background, without the user seeing them.",
    SessionsCloseAny = "sessions.close_any", Company: "End other users' remote sessions.",
    TerminalConnect = "terminal.connect", Company: "Open terminals on devices as the signed-in user.",
    TerminalConnectSystem = "terminal.connect_system", Company: "Open terminals on devices as SYSTEM or root, the Agent's own account.",
    ScriptsRun = "scripts.run", Company: "Run toolbox scripts on devices.",
    ScriptsManageShared = "scripts.manage_shared", Instance: "Create, edit and delete shared toolbox scripts.",
    FilesDeliver = "files.deliver", Company: "Deliver toolbox files to devices.",
    FilesManageShared = "files.manage_shared", Instance: "Upload, edit and delete shared toolbox files.",
    FilesBrowse = "files.browse", Company: "Browse devices' folders and download their files in the website's file browser.",
    FilesManage = "files.manage", Company: "Upload files to devices, and create, rename, move and delete their files and folders, in the website's file browser.",
    FilesSystem = "files.system", Company: "Use the file browser as SYSTEM or root, the Agent's own account, which reaches every file on a device; without it, only as the signed-in user.",
    ToolsProcessesView = "tools.processes.view", Company: "See the processes running on devices, with their users and command lines, in the website's remote tools.",
    ToolsProcessesManage = "tools.processes.manage", Company: "End processes and process trees on devices, as SYSTEM or root, in the website's remote tools.",
    ToolsServicesView = "tools.services.view", Company: "See devices' services, their state, how they start and the accounts they run as, in the website's remote tools.",
    ToolsServicesManage = "tools.services.manage", Company: "Start, stop and restart devices' services and change how they start, in the website's remote tools.",
    ToolsRegistryView = "tools.registry.view", Company: "Browse Windows devices' registries, their keys and values, in the website's remote tools.",
    ToolsRegistryManage = "tools.registry.manage", Company: "Create, change, rename and delete keys and values in Windows devices' registries, in the website's remote tools.",
    JobsRunWide = "jobs.run_wide", Company: "Run scripts and send files on whole companies or on every device at once.",
    AutomationManage = "automation.manage", Instance: "Create, edit and delete automation policies, which run shared scripts, scan for or install updates and wake devices on a schedule, or run on newly enrolled devices.",
    UpdatesView = "updates.view", Company: "See devices' pending operating system updates, scans and update history, and ask devices to scan.",
    UpdatesInstall = "updates.install", Company: "Install operating system updates on devices and restart them to finish.",
    ApplicationsView = "applications.view", Company: "See the application library, the third-party packages installed and outdated on devices and their application history, and ask devices to scan.",
    ApplicationsDeploy = "applications.deploy", Company: "Install, update and uninstall third-party applications on devices.",
    ApplicationsManage = "applications.manage", Instance: "Add, edit and delete applications in the library, including custom installers, which run as SYSTEM or root.",
    AlertsView = "alerts.view", Company: "See alerts and the rules that raise them.",
    AlertsRespond = "alerts.respond", Company: "Acknowledge and resolve alerts.",
    AlertsManage = "alerts.manage", Instance: "Create, edit and delete alert rules and notification channels.",
    ReportsView = "reports.view", Company: "See report templates, schedules and generated reports, and run and download reports.",
    ReportsManage = "reports.manage", Instance: "Create, edit, run and delete report schedules and choose who they are emailed to, and delete generated reports.",
    UsersManage = "users.manage", Instance: "Invite, disable and remove users and assign their roles.",
    RolesManage = "roles.manage", Instance: "Create, edit and delete roles.",
    SettingsManage = "settings.manage", Instance: "Change the instance name, remote session policy and how new Agent releases roll out to devices.",
    AuthenticationManage = "authentication.manage", Instance: "Change the sign-in policy: two-factor requirement, password length, session lifetime and whether API tokens work.",
    ApiTokensManage = "api_tokens.manage", Instance: "See and revoke every user's API tokens.",
    AuditView = "audit.view", Instance: "Read the audit log.",
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission {0:?}")]
pub struct UnknownPermission(pub String);

impl fmt::Display for Permission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

pub type Permissions = BTreeSet<Permission>;

pub fn all_permissions() -> Permissions {
    Permission::ALL.iter().copied().collect()
}

/// A role as users and the API see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `"administrator"` or `"technician"` for the built-in roles.
    pub builtin: Option<String>,
    pub permissions: Permissions,
    /// Whether the permissions reach every company's devices, and devices
    /// in no company.
    pub all_companies: bool,
    /// The companies they reach otherwise, sorted; empty with
    /// `all_companies`.
    pub company_ids: BTreeSet<String>,
}

impl Role {
    pub fn is_administrator(&self) -> bool {
        self.builtin.as_deref() == Some(ADMINISTRATOR_ROLE_ID)
    }

    /// The companies the role's permissions reach.
    pub fn companies(&self) -> CompanySet {
        if self.all_companies {
            CompanySet::All
        } else {
            CompanySet::Only(self.company_ids.clone())
        }
    }

    /// The permissions the role grants: on a role limited to some
    /// companies, only those of [`Reach::Company`]. The API refuses to save
    /// others on one, so this only matters for rows written otherwise.
    pub fn grants(&self) -> impl Iterator<Item = Permission> + '_ {
        self.permissions
            .iter()
            .copied()
            .filter(|permission| self.all_companies || permission.reach() == Reach::Company)
    }
}

#[derive(sqlx::FromRow)]
struct RoleRow {
    id: String,
    name: String,
    description: String,
    builtin: Option<String>,
    all_companies: bool,
}

/// Loads roles with their permissions and companies: every role, or only
/// `ids`.
pub async fn load_roles(
    executor: &mut impl Executor,
    ids: Option<&[String]>,
) -> db::Result<Vec<Role>> {
    let mut select = Query::select();
    select
        .columns([
            Roles::Id,
            Roles::Name,
            Roles::Description,
            Roles::Builtin,
            Roles::AllCompanies,
        ])
        .from(Roles::Table)
        .order_by(Roles::Name, Order::Asc);
    let mut grants = Query::select();
    grants
        .columns([RolePermissions::RoleId, RolePermissions::Permission])
        .from(RolePermissions::Table);
    let mut reached = Query::select();
    reached
        .columns([RoleCompanies::RoleId, RoleCompanies::CompanyId])
        .from(RoleCompanies::Table);
    if let Some(ids) = ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        select.and_where(Expr::col(Roles::Id).is_in(ids.iter().cloned()));
        grants.and_where(Expr::col(RolePermissions::RoleId).is_in(ids.iter().cloned()));
        reached.and_where(Expr::col(RoleCompanies::RoleId).is_in(ids.iter().cloned()));
    }
    let rows: Vec<RoleRow> = executor.fetch_all(&select).await?;
    let grants: Vec<(String, String)> = executor.fetch_all(&grants).await?;
    let reached: Vec<(String, String)> = executor.fetch_all(&reached).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let administrator = row.builtin.as_deref() == Some(ADMINISTRATOR_ROLE_ID);
            let permissions = if administrator {
                all_permissions()
            } else {
                grants
                    .iter()
                    .filter(|(role_id, _)| *role_id == row.id)
                    // A permission a later release removed grants nothing.
                    .filter_map(|(_, name)| name.parse().ok())
                    .collect()
            };
            let all_companies = administrator || row.all_companies;
            let company_ids = if all_companies {
                BTreeSet::new()
            } else {
                reached
                    .iter()
                    .filter(|(role_id, _)| *role_id == row.id)
                    .map(|(_, company_id)| company_id.clone())
                    .collect()
            };
            Role {
                id: row.id,
                name: row.name,
                description: row.description,
                builtin: row.builtin,
                permissions,
                all_companies,
                company_ids,
            }
        })
        .collect())
}

/// The roles a user holds, directly or through an identity provider group.
pub async fn user_roles(executor: &mut impl Executor, user_id: &str) -> db::Result<Vec<Role>> {
    let ids: Vec<(String,)> = executor
        .fetch_all(
            &Query::select()
                .column(EffectiveUserRoles::RoleId)
                .from(EffectiveUserRoles::Table)
                .and_where(Expr::col(EffectiveUserRoles::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    let ids = ids.into_iter().map(|(id,)| id).collect::<Vec<_>>();
    load_roles(executor, Some(&ids)).await
}

/// Who is left to administer MeshRMM, as [`remaining_administrators`]
/// counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Administrators {
    /// An enabled administrator can sign in.
    CanSignIn,
    /// Enabled administrators remain, but none of them can sign in: none
    /// has a password or a passkey, and SSO is off or links none of them.
    NoneCanSignIn,
    /// No enabled user holds the Administrator role.
    None,
}

/// Whether an administrator who can sign in remains: an enabled user who
/// holds the Administrator role, assigned to them or through an identity
/// provider's group, and has a way to sign in that works with the current
/// settings: a password, a passkey, or an SSO identity linked to the
/// account while SSO is turned on. An account SSO or SCIM created has
/// neither of the first two, so it counts only while SSO is on and once it
/// has signed in with it.
///
/// Every change that can take the role from someone, disable or delete
/// them, take away a way they sign in, or turn SSO off calls this inside
/// its transaction after making the change, and keeps the change only if
/// such an administrator remains.
pub async fn remaining_administrators(executor: &mut impl Executor) -> db::Result<Administrators> {
    // Two transactions removing different administrators would each still
    // count the other. Locking one shared row first makes the second wait
    // and then count what the first committed. (SQLite's write lock already
    // serializes them; sea-query leaves FOR UPDATE out there.)
    let _: Option<(i64,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Settings::Id)
                .from(Settings::Table)
                .and_where(Expr::col(Settings::Id).eq(1))
                .lock(LockType::Update)
                .to_owned(),
        )
        .await?;
    let sso: Option<(bool,)> = executor
        .fetch_optional(
            &Query::select()
                .column(OidcProvider::Enabled)
                .from(OidcProvider::Table)
                .and_where(Expr::col(OidcProvider::Id).eq(1))
                .to_owned(),
        )
        .await?;
    let mut can_sign_in = Cond::any()
        .add(Expr::col((Users::Table, Users::PasswordHash)).is_not_null())
        .add(Expr::exists(
            Query::select()
                .expr(Expr::val(1))
                .from(UserPasskeys::Table)
                .and_where(
                    Expr::col((UserPasskeys::Table, UserPasskeys::UserId))
                        .equals((Users::Table, Users::Id)),
                )
                .to_owned(),
        ));
    if sso.is_some_and(|(enabled,)| enabled) {
        can_sign_in = can_sign_in.add(Expr::col((Users::Table, Users::OidcSubject)).is_not_null());
    }
    let signs_in: Option<(String,)> = executor
        .fetch_optional(&enabled_administrators().cond_where(can_sign_in).to_owned())
        .await?;
    if signs_in.is_some() {
        return Ok(Administrators::CanSignIn);
    }
    // Only to say why the change is refused.
    let enabled: Option<(String,)> = executor.fetch_optional(&enabled_administrators()).await?;
    Ok(if enabled.is_some() {
        Administrators::NoneCanSignIn
    } else {
        Administrators::None
    })
}

/// Whether an administrator who can sign in remains; see
/// [`remaining_administrators`].
pub async fn administrator_remains(executor: &mut impl Executor) -> db::Result<bool> {
    Ok(remaining_administrators(executor).await? == Administrators::CanSignIn)
}

/// The ID of one enabled user holding the Administrator role.
fn enabled_administrators() -> sea_query::SelectStatement {
    Query::select()
        .column((Users::Table, Users::Id))
        .from(Users::Table)
        .inner_join(
            EffectiveUserRoles::Table,
            Expr::col((EffectiveUserRoles::Table, EffectiveUserRoles::UserId))
                .equals((Users::Table, Users::Id)),
        )
        .and_where(
            Expr::col((EffectiveUserRoles::Table, EffectiveUserRoles::RoleId))
                .eq(ADMINISTRATOR_ROLE_ID),
        )
        .and_where(Expr::col((Users::Table, Users::Disabled)).eq(false))
        .limit(1)
        .to_owned()
}

/// What `roles` grant, and where; see [`Access`].
pub fn access_of(roles: &[Role]) -> Access {
    Access::of(roles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_names_round_trip_and_are_unique() {
        let names = Permission::ALL
            .iter()
            .map(|permission| permission.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), Permission::ALL.len());
        for permission in Permission::ALL {
            assert_eq!(permission.as_str().parse::<Permission>(), Ok(*permission));
            assert!(!permission.description().is_empty());
        }
        assert!("devices.everything".parse::<Permission>().is_err());
    }

    #[test]
    fn permissions_serialize_as_their_names() {
        assert_eq!(
            serde_json::to_string(&Permission::SessionsConnectBackground).unwrap(),
            r#""sessions.connect_background""#
        );
        assert_eq!(
            serde_json::from_str::<Permission>(r#""audit.view""#).unwrap(),
            Permission::AuditView
        );
        assert!(serde_json::from_str::<Permission>(r#""audit.edit""#).is_err());
    }

    #[test]
    fn every_permission_has_a_reach() {
        let instance = Permission::ALL
            .iter()
            .filter(|permission| permission.reach() == Reach::Instance)
            .map(|permission| permission.as_str())
            .collect::<BTreeSet<_>>();
        assert!(instance.contains("users.manage"));
        assert!(instance.contains("audit.view"));
        assert!(!instance.contains("devices.view"));
        assert!(!instance.contains("jobs.run_wide"));
    }
}

//! Permissions and the roles that grant them.
//!
//! A role is a named set of permissions; a user holds the union of their
//! roles' permissions. The built-in Administrator role holds every permission
//! implicitly, including ones added by later releases, so it can't be emptied.
use std::{collections::BTreeSet, fmt, str::FromStr};

use sea_query::{Expr, ExprTrait, Order, Query};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::db::{
    self, Executor,
    tables::{EffectiveUserRoles, RolePermissions, Roles},
};

/// The ID (and `builtin` value) of the role with every permission.
pub const ADMINISTRATOR_ROLE_ID: &str = "administrator";
/// The ID (and `builtin` value) of the editable default role.
pub const TECHNICIAN_ROLE_ID: &str = "technician";

macro_rules! permissions {
    ($($variant:ident = $name:literal: $description:literal,)+) => {
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

permissions! {
    DevicesView = "devices.view": "See devices, their status and inventory.",
    DevicesEnroll = "devices.enroll": "Create Agent installers to enroll devices.",
    DevicesDelete = "devices.delete": "Delete devices and uninstall their Agents.",
    DevicesRotateCredentials = "devices.rotate_credentials": "Rotate a device's Agent credential.",
    SessionsConnect = "sessions.connect": "Start remote sessions.",
    SessionsConnectBackground = "sessions.connect_background": "Start remote sessions in the background, without the user seeing them.",
    SessionsCloseAny = "sessions.close_any": "End other users' remote sessions.",
    ScriptsRun = "scripts.run": "Run toolbox scripts on devices.",
    ScriptsManageShared = "scripts.manage_shared": "Create, edit and delete shared toolbox scripts.",
    FilesDeliver = "files.deliver": "Deliver toolbox files to devices.",
    FilesManageShared = "files.manage_shared": "Upload, edit and delete shared toolbox files.",
    UsersManage = "users.manage": "Invite, disable and remove users and assign their roles.",
    RolesManage = "roles.manage": "Create, edit and delete roles.",
    SettingsManage = "settings.manage": "Change the instance name and remote session policy.",
    AuthenticationManage = "authentication.manage": "Change the sign-in policy: two-factor requirement, password length and session lifetime.",
    AuditView = "audit.view": "Read the audit log.",
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
}

impl Role {
    pub fn is_administrator(&self) -> bool {
        self.builtin.as_deref() == Some(ADMINISTRATOR_ROLE_ID)
    }
}

#[derive(sqlx::FromRow)]
struct RoleRow {
    id: String,
    name: String,
    description: String,
    builtin: Option<String>,
}

/// Loads roles with their permissions: every role, or only `ids`.
pub async fn load_roles(
    executor: &mut impl Executor,
    ids: Option<&[String]>,
) -> db::Result<Vec<Role>> {
    let mut select = Query::select();
    select
        .columns([Roles::Id, Roles::Name, Roles::Description, Roles::Builtin])
        .from(Roles::Table)
        .order_by(Roles::Name, Order::Asc);
    let mut grants = Query::select();
    grants
        .columns([RolePermissions::RoleId, RolePermissions::Permission])
        .from(RolePermissions::Table);
    if let Some(ids) = ids {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        select.and_where(Expr::col(Roles::Id).is_in(ids.iter().cloned()));
        grants.and_where(Expr::col(RolePermissions::RoleId).is_in(ids.iter().cloned()));
    }
    let rows: Vec<RoleRow> = executor.fetch_all(&select).await?;
    let grants: Vec<(String, String)> = executor.fetch_all(&grants).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let permissions = if row.builtin.as_deref() == Some(ADMINISTRATOR_ROLE_ID) {
                all_permissions()
            } else {
                grants
                    .iter()
                    .filter(|(role_id, _)| *role_id == row.id)
                    // A permission a later release removed grants nothing.
                    .filter_map(|(_, name)| name.parse().ok())
                    .collect()
            };
            Role {
                id: row.id,
                name: row.name,
                description: row.description,
                builtin: row.builtin,
                permissions,
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

/// Every permission any of `roles` grants.
pub fn permissions_of(roles: &[Role]) -> Permissions {
    roles
        .iter()
        .flat_map(|role| role.permissions.iter().copied())
        .collect()
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
    fn a_users_permissions_are_the_union_of_their_roles() {
        let role = |id: &str, builtin: Option<&str>, permissions: &[Permission]| Role {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            builtin: builtin.map(str::to_owned),
            permissions: permissions.iter().copied().collect(),
        };
        let roles = [
            role("a", None, &[Permission::DevicesView]),
            role(
                "b",
                None,
                &[Permission::DevicesView, Permission::ScriptsRun],
            ),
        ];
        assert_eq!(
            permissions_of(&roles),
            [Permission::DevicesView, Permission::ScriptsRun].into()
        );
        assert!(permissions_of(&[]).is_empty());
        assert!(role("administrator", Some(ADMINISTRATOR_ROLE_ID), &[]).is_administrator());
    }
}

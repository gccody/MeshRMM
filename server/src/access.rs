//! Where a user's permissions apply: which companies' devices each one
//! reaches.
//!
//! A role grants its permissions on the devices of the companies it
//! reaches: every company (and devices in none), or chosen ones. A user
//! holds permission P on a device when one of their roles grants P *and*
//! reaches the device's company. [`Access`] keeps, for each permission, the
//! union of the companies of the roles that grant it, so role B's
//! permissions never leak into role A's companies. Permissions of
//! [`Reach::Instance`] come only from roles that reach every company.
//!
//! Routes ask through [`Authorized`]:
//!
//! - [`Authorized::require_device`] for one device: it answers 404 for a
//!   device out of reach exactly as for a missing one, so a device's
//!   existence doesn't leak.
//! - [`Authorized::require_company`] for one company.
//! - [`Authorized::devices_condition`] and
//!   [`Authorized::companies_condition`] for list queries: a SQL condition
//!   on a device ID or company ID column, `None` when the permission reaches
//!   every company, so lists cost one query however many devices they have.
//! - [`Authorized::companies`] for filtering in memory, as the realtime
//!   socket does.
use std::collections::{BTreeMap, BTreeSet};

use sea_query::{Expr, ExprTrait, IntoColumnRef, Query, SimpleExpr};

use crate::{
    auth::Authorized,
    db::{Executor, tables::Agents},
    http::ApiError,
    rbac::{Permission, Permissions, Reach, Role},
};

/// The companies a permission reaches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CompanySet {
    /// Every company, and devices in no company.
    #[default]
    All,
    /// Only these companies' devices.
    Only(BTreeSet<String>),
}

impl CompanySet {
    /// No company at all: what a permission the user lacks reaches.
    pub fn none() -> Self {
        Self::Only(BTreeSet::new())
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// Whether it reaches a device or token in `company_id`; `None` for no
    /// company, which only [`CompanySet::All`] reaches.
    pub fn covers(&self, company_id: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => company_id.is_some_and(|id| ids.contains(id)),
        }
    }

    /// Whether it reaches every company `other` does.
    pub fn includes(&self, other: &CompanySet) -> bool {
        match (self, other) {
            (Self::All, _) => true,
            (Self::Only(_), Self::All) => false,
            (Self::Only(mine), Self::Only(theirs)) => theirs.is_subset(mine),
        }
    }

    /// Adds the companies `other` reaches.
    pub fn add(&mut self, other: &CompanySet) {
        match (&mut *self, other) {
            (Self::All, _) => {}
            (Self::Only(_), Self::All) => *self = Self::All,
            (Self::Only(mine), Self::Only(theirs)) => mine.extend(theirs.iter().cloned()),
        }
    }

    /// The companies both reach.
    pub fn intersection(&self, other: &CompanySet) -> CompanySet {
        match (self, other) {
            (Self::All, other) | (other, Self::All) => other.clone(),
            (Self::Only(mine), Self::Only(theirs)) => {
                Self::Only(mine.intersection(theirs).cloned().collect())
            }
        }
    }

    /// A condition that holds where `column`, a company ID (NULL for no
    /// company), is one of these companies; `None` for every company.
    pub fn company_condition(&self, column: impl IntoColumnRef) -> Option<SimpleExpr> {
        match self {
            Self::All => None,
            Self::Only(ids) if ids.is_empty() => Some(nothing()),
            Self::Only(ids) => Some(Expr::col(column).is_in(ids.iter().cloned())),
        }
    }

    /// A condition that holds where `column`, a device ID, names a device
    /// in one of these companies; `None` for every company. It reads the
    /// device's company through `agents`' primary key, so queries already
    /// joined to `agents` should use [`Self::company_condition`] on
    /// `agents.company_id` instead.
    pub fn device_condition(&self, column: impl IntoColumnRef) -> Option<SimpleExpr> {
        let companies = self.company_condition((Agents::Table, Agents::CompanyId))?;
        Some(
            Expr::col(column).in_subquery(
                Query::select()
                    .column((Agents::Table, Agents::Id))
                    .from(Agents::Table)
                    .and_where(companies)
                    .to_owned(),
            ),
        )
    }
}

/// A condition that holds for no row.
fn nothing() -> SimpleExpr {
    Expr::cust("1 = 0")
}

/// What a user may do, and on which companies' devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    grants: BTreeMap<Permission, CompanySet>,
}

impl Access {
    /// What `roles` grant together: each permission reaches the companies
    /// of the roles that grant it.
    pub fn of(roles: &[Role]) -> Self {
        let mut grants: BTreeMap<Permission, CompanySet> = BTreeMap::new();
        for role in roles {
            let companies = role.companies();
            for permission in role.grants() {
                grants
                    .entry(permission)
                    .and_modify(|reached| reached.add(&companies))
                    .or_insert_with(|| companies.clone());
            }
        }
        Self { grants }
    }

    /// Only `permissions` of it, as an API token limited to them has.
    pub fn limited_to(&self, permissions: &Permissions) -> Self {
        Self {
            grants: self
                .grants
                .iter()
                .filter(|(permission, _)| permissions.contains(permission))
                .map(|(permission, companies)| (*permission, companies.clone()))
                .collect(),
        }
    }

    /// Every permission held somewhere.
    pub fn permissions(&self) -> Permissions {
        self.grants.keys().copied().collect()
    }

    pub fn holds(&self, permission: Permission) -> bool {
        self.grants.contains_key(&permission)
    }

    /// The companies `permission` reaches; none if it isn't held.
    pub fn companies(&self, permission: Permission) -> CompanySet {
        self.grants
            .get(&permission)
            .cloned()
            .unwrap_or_else(CompanySet::none)
    }

    /// The companies any of `permissions` reaches.
    pub fn companies_of_any(&self, permissions: &[Permission]) -> CompanySet {
        let mut companies = CompanySet::none();
        for permission in permissions {
            if let Some(reached) = self.grants.get(permission) {
                companies.add(reached);
            }
        }
        companies
    }

    /// Whether `permission` reaches every company.
    pub fn everywhere(&self, permission: Permission) -> bool {
        self.grants.get(&permission).is_some_and(CompanySet::is_all)
    }

    /// Whether `permission` reaches a device or token in `company_id`.
    pub fn covers(&self, permission: Permission, company_id: Option<&str>) -> bool {
        self.grants
            .get(&permission)
            .is_some_and(|companies| companies.covers(company_id))
    }

    /// The permissions held only in some companies.
    pub fn limited(&self) -> Vec<Permission> {
        self.grants
            .iter()
            .filter(|(_, companies)| !companies.is_all())
            .map(|(permission, _)| *permission)
            .collect()
    }

    /// Whether this holds `permission` on every company `companies` names.
    pub fn grants_on(&self, permission: Permission, companies: &CompanySet) -> bool {
        self.grants
            .get(&permission)
            .is_some_and(|reached| reached.includes(companies))
    }

    /// Checks that `permission` reaches the enrolled, undeleted device
    /// `device_id`; 404 for one that is missing or out of reach alike. See
    /// [`Authorized::require_device`], which also answers 403 for a
    /// permission the caller lacks.
    pub async fn require_device(
        &self,
        executor: &mut impl Executor,
        permission: Permission,
        device_id: &str,
    ) -> Result<(), ApiError> {
        match device_company(executor, device_id).await? {
            Some(company_id) if self.covers(permission, company_id.as_deref()) => Ok(()),
            _ => Err(device_not_found()),
        }
    }
}

/// The company of the enrolled, undeleted device `device_id`: `None` when
/// there is no such device, `Some(None)` when it is in no company.
pub async fn device_company(
    executor: &mut impl Executor,
    device_id: &str,
) -> crate::db::Result<Option<Option<String>>> {
    let row: Option<(Option<String>,)> = executor
        .fetch_optional(
            &Query::select()
                .column(Agents::CompanyId)
                .from(Agents::Table)
                .and_where(Expr::col(Agents::Id).eq(device_id))
                .and_where(Expr::col(Agents::DeletionRequestedAt).is_null())
                .to_owned(),
        )
        .await?;
    Ok(row.map(|(company_id,)| company_id))
}

/// The answer for a device that is missing, deleted, or out of reach.
pub fn device_not_found() -> ApiError {
    ApiError::not_found("Device not found")
}

/// The answer for a company that is missing or out of reach.
pub fn company_not_found() -> ApiError {
    ApiError::not_found("no such company")
}

impl Authorized {
    /// The companies `permission` reaches for this caller; none if they
    /// lack it.
    pub fn companies(&self, permission: Permission) -> CompanySet {
        self.access.companies(permission)
    }

    /// Whether `permission` reaches every company, and devices in none.
    pub fn everywhere(&self, permission: Permission) -> bool {
        self.access.everywhere(permission)
    }

    /// Whether `permission` reaches a device or token in `company_id`.
    pub fn covers(&self, permission: Permission, company_id: Option<&str>) -> bool {
        self.access.covers(permission, company_id)
    }

    /// Refuses a caller who holds `permission` only in some companies, for
    /// what reaches every device at once.
    pub fn require_everywhere(&self, permission: Permission) -> Result<(), ApiError> {
        self.require(permission)?;
        if self.everywhere(permission) {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!(
                "you have the {permission} permission only in some companies"
            ))
            .with_code("permission_denied"))
        }
    }

    /// Checks that the caller holds `permission` (403 otherwise) on the
    /// enrolled, undeleted device `device_id`, an ID already parsed. A
    /// device out of reach answers 404 exactly as a missing one does.
    pub async fn require_device(
        &self,
        executor: &mut impl Executor,
        permission: Permission,
        device_id: &str,
    ) -> Result<(), ApiError> {
        self.require(permission)?;
        self.access
            .require_device(executor, permission, device_id)
            .await
    }

    /// Checks that the caller holds `permission` (403 otherwise) in the
    /// company `company_id`; one out of reach answers 404 as a missing one
    /// does. It doesn't check that the company exists.
    pub fn require_company(
        &self,
        permission: Permission,
        company_id: &str,
    ) -> Result<(), ApiError> {
        self.require(permission)?;
        if self.covers(permission, Some(company_id)) {
            Ok(())
        } else {
            Err(company_not_found())
        }
    }

    /// A condition that holds where `column`, a device ID, names a device
    /// `permission` reaches; `None` when it reaches every company. For a
    /// query joined to `agents`, [`Self::companies_condition`] on
    /// `agents.company_id` is cheaper.
    pub fn devices_condition(
        &self,
        permission: Permission,
        column: impl IntoColumnRef,
    ) -> Option<SimpleExpr> {
        self.companies(permission).device_condition(column)
    }

    /// A condition that holds where `column`, a company ID (NULL for no
    /// company), names a company `permission` reaches; `None` when it
    /// reaches every company.
    pub fn companies_condition(
        &self,
        permission: Permission,
        column: impl IntoColumnRef,
    ) -> Option<SimpleExpr> {
        self.companies(permission).company_condition(column)
    }
}

/// Why a role can't hold these permissions on these companies: the
/// [`Reach::Instance`] ones among them when it reaches only some companies.
pub fn instance_permissions_on_limited_role(
    permissions: &Permissions,
    companies: &CompanySet,
) -> Vec<Permission> {
    if companies.is_all() {
        return Vec::new();
    }
    permissions
        .iter()
        .copied()
        .filter(|permission| permission.reach() == Reach::Instance)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac::ADMINISTRATOR_ROLE_ID;

    fn role(permissions: &[Permission], companies: Option<&[&str]>) -> Role {
        Role {
            id: "r".into(),
            name: "r".into(),
            description: String::new(),
            builtin: None,
            permissions: permissions.iter().copied().collect(),
            all_companies: companies.is_none(),
            company_ids: companies
                .unwrap_or_default()
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
        }
    }

    fn only(ids: &[&str]) -> CompanySet {
        CompanySet::Only(ids.iter().map(|id| (*id).to_owned()).collect())
    }

    #[test]
    fn each_permission_reaches_only_the_companies_of_the_roles_granting_it() {
        // Role A runs scripts in Acme; role B views devices in Globex.
        let access = Access::of(&[
            role(&[Permission::ScriptsRun], Some(&["acme"])),
            role(&[Permission::DevicesView], Some(&["globex"])),
        ]);
        assert!(access.covers(Permission::ScriptsRun, Some("acme")));
        assert!(!access.covers(Permission::ScriptsRun, Some("globex")));
        assert!(access.covers(Permission::DevicesView, Some("globex")));
        assert!(!access.covers(Permission::DevicesView, Some("acme")));
        assert!(!access.covers(Permission::DevicesView, None));
        assert_eq!(
            access.permissions(),
            [Permission::DevicesView, Permission::ScriptsRun].into()
        );
        assert_eq!(
            access.companies_of_any(&[Permission::DevicesView, Permission::ScriptsRun]),
            only(&["acme", "globex"])
        );
    }

    #[test]
    fn a_role_for_every_company_reaches_devices_in_none() {
        let access = Access::of(&[
            role(&[Permission::DevicesView], Some(&["acme"])),
            role(&[Permission::DevicesView], None),
        ]);
        assert!(access.everywhere(Permission::DevicesView));
        assert!(access.covers(Permission::DevicesView, None));
        assert!(access.limited().is_empty());
    }

    #[test]
    fn instance_permissions_come_only_from_roles_for_every_company() {
        let access = Access::of(&[role(
            &[Permission::UsersManage, Permission::DevicesView],
            Some(&["acme"]),
        )]);
        assert!(!access.holds(Permission::UsersManage));
        assert!(access.holds(Permission::DevicesView));
        assert_eq!(access.limited(), vec![Permission::DevicesView]);
        assert_eq!(
            instance_permissions_on_limited_role(
                &[Permission::UsersManage, Permission::DevicesView].into(),
                &only(&["acme"])
            ),
            vec![Permission::UsersManage]
        );
        assert!(
            instance_permissions_on_limited_role(
                &[Permission::UsersManage].into(),
                &CompanySet::All
            )
            .is_empty()
        );
    }

    #[test]
    fn the_administrator_reaches_every_company() {
        let administrator = Role {
            builtin: Some(ADMINISTRATOR_ROLE_ID.into()),
            ..role(Permission::ALL, None)
        };
        let access = Access::of(&[administrator]);
        for permission in Permission::ALL {
            assert!(access.everywhere(*permission));
        }
    }

    #[test]
    fn a_token_keeps_its_users_reach() {
        let access = Access::of(&[role(
            &[Permission::DevicesView, Permission::ScriptsRun],
            Some(&["acme"]),
        )]);
        let token = access.limited_to(&[Permission::DevicesView].into());
        assert!(token.covers(Permission::DevicesView, Some("acme")));
        assert!(!token.covers(Permission::DevicesView, Some("globex")));
        assert!(!token.holds(Permission::ScriptsRun));
    }

    #[test]
    fn companies_combine() {
        let mut companies = only(&["a"]);
        companies.add(&only(&["b"]));
        assert_eq!(companies, only(&["a", "b"]));
        assert!(companies.includes(&only(&["a"])));
        assert!(!companies.includes(&CompanySet::All));
        assert!(CompanySet::All.includes(&companies));
        assert_eq!(companies.intersection(&only(&["b", "c"])), only(&["b"]));
        assert_eq!(CompanySet::All.intersection(&only(&["c"])), only(&["c"]));
        companies.add(&CompanySet::All);
        assert!(companies.is_all());
        assert!(!CompanySet::none().covers(Some("a")));
    }
}

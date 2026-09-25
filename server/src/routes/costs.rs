//! The platform cost report: what each company cost on Cloudflare and WorkOS
//! in a calendar month, priced at paid-plan list rates before any included
//! allowance. docs/cost-tracking.md explains each source and allocation.
use crate::usage::{
    OBJECT_OWNER_PREFIX, PLATFORM_OWNER, SLUG_OWNER_PREFIX, civil_date, unix_ms_from_civil,
};
use crate::*;
use serde::de::{DeserializeOwned, Deserializer};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Worker script names from server/wrangler.jsonc and dashboard/wrangler.jsonc.
const API_SCRIPT: &str = "pulsermm-server";
const DASHBOARD_SCRIPT: &str = "meshrmm-dashboard";
/// The Analytics Engine dataset both Workers' `USAGE` bindings write to.
const USAGE_DATASET: &str = "meshrmm_usage";
/// When the rates below were last checked against the providers' pricing pages.
const PRICING_AS_OF: &str = "2026-09-25";
/// Analytics Engine keeps three months of data, so older reports would be incomplete.
const REPORT_MONTHS: i64 = 3;
/// Cloudflare bills incoming WebSocket messages to a Durable Object at 20:1.
const WEBSOCKET_MESSAGES_PER_REQUEST: f64 = 20.0;
const BYTES_PER_GB: f64 = 1e9;
/// Remote session owners are kept as long as their usage can be reported.
const SESSION_OWNER_RETENTION_MS: i64 = 120 * 86_400_000;
/// WorkOS bills SSO and Directory Sync connections in graduated volume tiers:
/// (connections up to and including, USD per connection per month).
const WORKOS_CONNECTION_TIERS: &[(f64, f64)] = &[
    (15.0, 125.0),
    (30.0, 100.0),
    (50.0, 80.0),
    (f64::INFINITY, 65.0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Item {
    WorkersRequests,
    WorkersCpu,
    WorkersLogs,
    D1RowsRead,
    D1RowsWritten,
    D1Storage,
    DurableObjectRequests,
    DurableObjectDuration,
    DurableObjectRowsRead,
    DurableObjectRowsWritten,
    DurableObjectStorage,
    TurnEgress,
    AnalyticsEngine,
    WorkersPaidPlan,
    WorkosActiveUsers,
    WorkosSso,
    WorkosDirectorySync,
}

/// How a quantity was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Basis {
    /// Counted for the company itself.
    Metered,
    /// A shared total divided in proportion to the company's metered usage.
    Allocated,
    /// Derived from a related count, because the provider reports no exact figure.
    Estimated,
    /// A flat platform fee.
    Fixed,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Rate {
    provider: &'static str,
    label: &'static str,
    /// The unit quantities are counted in.
    unit: &'static str,
    /// Price of `per` units, which the dashboard shows as `usd / per_label`.
    usd: f64,
    per: f64,
    per_label: &'static str,
    basis: Basis,
}

impl Item {
    /// List prices from developers.cloudflare.com/workers/platform/pricing,
    /// developers.cloudflare.com/realtime/turn, and workos.com/pricing.
    #[rustfmt::skip]
    fn rate(self, prices: &TierPrices) -> Rate {
        use Basis::*;
        const M: f64 = 1e6;
        let (label, unit, usd, per, per_label, basis) = match self {
            Self::WorkersRequests => ("Workers requests", "requests", 0.30, M, "1M requests", Metered),
            Self::WorkersCpu => ("Workers CPU time", "CPU ms", 0.02, M, "1M CPU ms", Allocated),
            Self::WorkersLogs => ("Workers Logs events", "events", 0.60, M, "1M events", Estimated),
            Self::D1RowsRead => ("D1 rows read", "rows", 0.001, M, "1M rows", Metered),
            Self::D1RowsWritten => ("D1 rows written", "rows", 1.00, M, "1M rows", Metered),
            Self::D1Storage => ("D1 storage", "GB-months", 0.75, 1.0, "GB-month", Allocated),
            Self::DurableObjectRequests => ("Durable Objects requests", "requests", 0.15, M, "1M requests", Metered),
            Self::DurableObjectDuration => ("Durable Objects duration", "GB-s", 12.50, M, "1M GB-s", Metered),
            Self::DurableObjectRowsRead => ("Durable Objects rows read", "rows", 0.001, M, "1M rows", Metered),
            Self::DurableObjectRowsWritten => ("Durable Objects rows written", "rows", 1.00, M, "1M rows", Metered),
            Self::DurableObjectStorage => ("Durable Objects storage", "GB-months", 0.20, 1.0, "GB-month", Allocated),
            Self::TurnEgress => ("TURN relay egress", "GB", 0.05, 1.0, "GB", Metered),
            Self::AnalyticsEngine => ("Analytics Engine data points", "data points", 0.25, M, "1M data points", Metered),
            Self::WorkersPaidPlan => ("Workers Paid plan", "months", 5.00, 1.0, "month", Fixed),
            Self::WorkosActiveUsers => ("AuthKit monthly active users", "users", 2_500.0, M, "1M users", Metered),
            Self::WorkosSso => ("SSO connections", "connections", prices.sso, 1.0, "connection", Metered),
            Self::WorkosDirectorySync => ("Directory Sync connections", "connections", prices.directory_sync, 1.0, "connection", Metered),
        };
        let provider = match self {
            Self::WorkosActiveUsers | Self::WorkosSso | Self::WorkosDirectorySync => "workos",
            _ => "cloudflare",
        };
        Rate { provider, label, unit, usd, per, per_label, basis }
    }
}

/// The average price per connection at the account's current volume.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TierPrices {
    sso: f64,
    directory_sync: f64,
}

fn average_tier_price(connections: f64) -> f64 {
    if connections <= 0.0 {
        return WORKOS_CONNECTION_TIERS[0].1;
    }
    let mut remaining = connections;
    let mut floor = 0.0;
    let mut total = 0.0;
    for &(ceiling, price) in WORKOS_CONNECTION_TIERS {
        let in_tier = remaining.min(ceiling - floor);
        total += in_tier * price;
        remaining -= in_tier;
        floor = ceiling;
        if remaining <= 0.0 {
            break;
        }
    }
    total / connections
}

#[derive(Debug, Deserialize)]
struct CompanyRow {
    id: String,
    name: String,
    slug: Option<String>,
    workos_organization_id: Option<String>,
}

/// One owner and source from the Analytics Engine usage dataset.
#[derive(Debug, Deserialize)]
struct MeteredRow {
    owner: String,
    source: String,
    #[serde(deserialize_with = "number")]
    data_points: f64,
    #[serde(deserialize_with = "number")]
    requests: f64,
    #[serde(deserialize_with = "number")]
    invocations: f64,
    #[serde(deserialize_with = "number")]
    d1_rows_read: f64,
    #[serde(deserialize_with = "number")]
    d1_rows_written: f64,
}

/// Analytics Engine returns some aggregates as JSON strings.
fn number<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Float(f64),
        Text(String),
    }
    match Number::deserialize(deserializer)? {
        Number::Float(value) => Ok(value),
        Number::Text(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
struct Group<D, S> {
    dimensions: D,
    #[serde(alias = "max")]
    sum: S,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptDimensions {
    script_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptSum {
    requests: f64,
    cpu_time_us: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectDimensions {
    namespace_id: String,
    object_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Requests {
    requests: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectPeriodicSum {
    duration: f64,
    rows_read: f64,
    rows_written: f64,
    inbound_websocket_msg_count: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamespaceDimensions {
    namespace_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredBytes {
    stored_bytes: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnDimensions {
    custom_identifier: String,
    #[serde(default)]
    key_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EgressBytes {
    egress_bytes: f64,
}

#[derive(Debug, Default, Deserialize)]
struct InvocationAnalytics {
    workers: Vec<Group<ScriptDimensions, ScriptSum>>,
    objects: Vec<Group<ObjectDimensions, Requests>>,
    turn: Vec<Group<TurnDimensions, EgressBytes>>,
}

#[derive(Debug, Default, Deserialize)]
struct ObjectAnalytics {
    periodic: Vec<Group<ObjectDimensions, ObjectPeriodicSum>>,
    storage: Vec<Group<NamespaceDimensions, StoredBytes>>,
}

#[derive(Debug, Default)]
struct CloudflareUsage {
    metered: Vec<MeteredRow>,
    invocations: InvocationAnalytics,
    objects: ObjectAnalytics,
}

/// Active SSO and Directory Sync connections per WorkOS organization.
#[derive(Debug, Default)]
struct WorkOsUsage {
    sso: HashMap<String, f64>,
    directory_sync: HashMap<String, f64>,
}

/// Everything the report is computed from, gathered before any pricing.
#[derive(Debug, Default)]
struct Inputs {
    /// The share of the month's storage billed so far: 1 for past months.
    month_fraction: f64,
    companies: Vec<CompanyRow>,
    /// Durable Object names that are not company IDs, by owning company.
    object_owners: HashMap<String, String>,
    active_users: HashMap<String, f64>,
    /// Rows each company holds in D1, which apportion the database's size.
    d1_rows: HashMap<String, f64>,
    d1_size_bytes: f64,
    cloudflare: Option<CloudflareUsage>,
    workos: Option<WorkOsUsage>,
}

/// Quantities per owner and item. `None` is the platform itself.
#[derive(Default)]
struct Ledger(BTreeMap<Option<String>, BTreeMap<Item, f64>>);

impl Ledger {
    fn add(&mut self, owner: Option<&str>, item: Item, quantity: f64) {
        if quantity.is_finite() && quantity > 0.0 {
            *self
                .0
                .entry(owner.map(str::to_owned))
                .or_default()
                .entry(item)
                .or_default() += quantity;
        }
    }
}

/// Maps the owners usage is recorded under to company IDs.
struct Owners<'a> {
    companies: HashMap<&'a str, &'a CompanyRow>,
    slugs: HashMap<&'a str, &'a str>,
    organizations: HashMap<&'a str, &'a str>,
    object_owners: &'a HashMap<String, String>,
    object_names: HashMap<&'a str, &'a str>,
}

impl<'a> Owners<'a> {
    fn new(inputs: &'a Inputs) -> Self {
        let mut object_names = HashMap::new();
        if let Some(cloudflare) = &inputs.cloudflare {
            let dimensions = cloudflare
                .invocations
                .objects
                .iter()
                .map(|group| &group.dimensions)
                .chain(
                    cloudflare
                        .objects
                        .periodic
                        .iter()
                        .map(|group| &group.dimensions),
                );
            for dimensions in dimensions {
                if let Some(name) = dimensions.name.as_deref().filter(|name| !name.is_empty()) {
                    object_names.insert(dimensions.object_id.as_str(), name);
                }
            }
        }
        Self {
            companies: inputs
                .companies
                .iter()
                .map(|company| (company.id.as_str(), company))
                .collect(),
            slugs: inputs
                .companies
                .iter()
                .filter_map(|company| Some((company.slug.as_deref()?, company.id.as_str())))
                .collect(),
            organizations: inputs
                .companies
                .iter()
                .filter_map(|company| {
                    Some((
                        company.workos_organization_id.as_deref()?,
                        company.id.as_str(),
                    ))
                })
                .collect(),
            object_owners: &inputs.object_owners,
            object_names,
        }
    }

    fn company(&self, id: &str) -> Option<&'a str> {
        self.companies.get(id).map(|company| company.id.as_str())
    }

    /// The company a Durable Object belongs to. Company presence objects are
    /// named after the company; Agent and session objects are recorded in D1.
    fn object(&self, object_id: &str) -> Option<&'a str> {
        let name = self.object_names.get(object_id)?;
        self.company(name).or_else(|| {
            self.object_owners
                .get(*name)
                .and_then(|company_id| self.company(company_id))
        })
    }

    /// The company an Analytics Engine owner index refers to.
    fn metered(&self, owner: &str) -> Option<&'a str> {
        if owner == PLATFORM_OWNER {
            None
        } else if let Some(slug) = owner.strip_prefix(SLUG_OWNER_PREFIX) {
            self.slugs.get(slug.to_ascii_lowercase().as_str()).copied()
        } else if let Some(object_id) = owner.strip_prefix(OBJECT_OWNER_PREFIX) {
            self.object(object_id)
        } else {
            self.company(owner)
        }
    }
}

fn price(ledger: &mut Ledger, inputs: &Inputs) {
    let owners = Owners::new(inputs);
    ledger.add(None, Item::WorkersPaidPlan, 1.0);

    for (company_id, users) in &inputs.active_users {
        ledger.add(owners.company(company_id), Item::WorkosActiveUsers, *users);
    }

    let d1_rows: f64 = inputs.d1_rows.values().sum();
    let d1_gb_months = inputs.d1_size_bytes / BYTES_PER_GB * inputs.month_fraction;
    if d1_rows > 0.0 {
        for (company_id, rows) in &inputs.d1_rows {
            ledger.add(
                owners.company(company_id),
                Item::D1Storage,
                d1_gb_months * rows / d1_rows,
            );
        }
    } else {
        ledger.add(None, Item::D1Storage, d1_gb_months);
    }

    if let Some(workos) = &inputs.workos {
        for (connections, item) in [
            (&workos.sso, Item::WorkosSso),
            (&workos.directory_sync, Item::WorkosDirectorySync),
        ] {
            for (organization_id, count) in connections {
                let owner = owners.organizations.get(organization_id.as_str()).copied();
                ledger.add(owner, item, *count);
            }
        }
    }

    if let Some(cloudflare) = &inputs.cloudflare {
        price_cloudflare(ledger, &owners, cloudflare, inputs.month_fraction);
    }
}

fn price_cloudflare(
    ledger: &mut Ledger,
    owners: &Owners<'_>,
    cloudflare: &CloudflareUsage,
    month_fraction: f64,
) {
    // Invocations per script and owner apportion each script's CPU time.
    let mut invocations: HashMap<&str, BTreeMap<Option<&str>, f64>> = HashMap::new();
    for row in &cloudflare.metered {
        let owner = owners.metered(&row.owner);
        ledger.add(owner, Item::WorkersRequests, row.requests);
        ledger.add(owner, Item::D1RowsRead, row.d1_rows_read);
        ledger.add(owner, Item::D1RowsWritten, row.d1_rows_written);
        ledger.add(owner, Item::AnalyticsEngine, row.data_points);
        ledger.add(owner, Item::WorkersLogs, row.invocations);
        let script = match row.source.as_str() {
            "api" => API_SCRIPT,
            "dashboard" => DASHBOARD_SCRIPT,
            _ => continue,
        };
        *invocations
            .entry(script)
            .or_default()
            .entry(owner)
            .or_default() += row.invocations;
    }
    for script in &cloudflare.invocations.workers {
        let cpu_ms = script.sum.cpu_time_us / 1_000.0;
        let by_owner = invocations.get(script.dimensions.script_name.as_str());
        let metered: f64 = by_owner
            .map(|owners| owners.values().sum())
            .unwrap_or_default();
        // Invocations from before metering began stay with the platform.
        let total = script.sum.requests.max(metered);
        let mut allocated = 0.0;
        if total > 0.0 {
            for (owner, count) in by_owner.into_iter().flatten() {
                let share = cpu_ms * count / total;
                ledger.add(*owner, Item::WorkersCpu, share);
                allocated += share;
            }
        }
        ledger.add(None, Item::WorkersCpu, cpu_ms - allocated);
    }

    // Objects per namespace and owner apportion each namespace's storage.
    let mut namespace_objects: BTreeMap<&str, BTreeMap<Option<&str>, BTreeSet<&str>>> =
        BTreeMap::new();
    for group in &cloudflare.invocations.objects {
        let dimensions = &group.dimensions;
        let owner = owners.object(&dimensions.object_id);
        let requests = if dimensions.kind.as_deref() == Some("hibernation") {
            group.sum.requests / WEBSOCKET_MESSAGES_PER_REQUEST
        } else {
            group.sum.requests
        };
        ledger.add(owner, Item::DurableObjectRequests, requests);
        ledger.add(owner, Item::WorkersLogs, group.sum.requests);
        namespace_objects
            .entry(&dimensions.namespace_id)
            .or_default()
            .entry(owner)
            .or_default()
            .insert(&dimensions.object_id);
    }
    for group in &cloudflare.objects.periodic {
        let dimensions = &group.dimensions;
        let owner = owners.object(&dimensions.object_id);
        let usage = &group.sum;
        ledger.add(owner, Item::DurableObjectDuration, usage.duration);
        ledger.add(owner, Item::DurableObjectRowsRead, usage.rows_read);
        ledger.add(owner, Item::DurableObjectRowsWritten, usage.rows_written);
        ledger.add(
            owner,
            Item::DurableObjectRequests,
            usage.inbound_websocket_msg_count / WEBSOCKET_MESSAGES_PER_REQUEST,
        );
        namespace_objects
            .entry(&dimensions.namespace_id)
            .or_default()
            .entry(owner)
            .or_default()
            .insert(&dimensions.object_id);
    }
    for group in &cloudflare.objects.storage {
        let gb_months = group.sum.stored_bytes / BYTES_PER_GB * month_fraction;
        let objects = namespace_objects.get(group.dimensions.namespace_id.as_str());
        let total: usize = objects
            .map(|owners| owners.values().map(BTreeSet::len).sum())
            .unwrap_or_default();
        if total == 0 {
            ledger.add(None, Item::DurableObjectStorage, gb_months);
            continue;
        }
        for (owner, ids) in objects.into_iter().flatten() {
            let share = gb_months * ids.len() as f64 / total as f64;
            ledger.add(*owner, Item::DurableObjectStorage, share);
        }
    }

    for group in &cloudflare.invocations.turn {
        let owner = owners.company(&group.dimensions.custom_identifier);
        ledger.add(
            owner,
            Item::TurnEgress,
            group.sum.egress_bytes / BYTES_PER_GB,
        );
    }
}

#[derive(Debug, Serialize)]
struct LineItem {
    item: Item,
    #[serde(flatten)]
    rate: Rate,
    quantity: f64,
    cost_usd: f64,
}

#[derive(Debug, Default, Serialize)]
struct CostSummary {
    cloudflare_usd: f64,
    workos_usd: f64,
    total_usd: f64,
    line_items: Vec<LineItem>,
}

impl CostSummary {
    fn new(items: Option<&BTreeMap<Item, f64>>, prices: &TierPrices) -> Self {
        let mut summary = Self::default();
        for (item, quantity) in items.into_iter().flatten() {
            let rate = item.rate(prices);
            let cost_usd = quantity * rate.usd / rate.per;
            match rate.provider {
                "workos" => summary.workos_usd += cost_usd,
                _ => summary.cloudflare_usd += cost_usd,
            }
            summary.total_usd += cost_usd;
            summary.line_items.push(LineItem {
                item: *item,
                rate,
                quantity: *quantity,
                cost_usd,
            });
        }
        summary
    }
}

#[derive(Debug, Serialize)]
struct CompanyCost {
    company_id: String,
    name: String,
    #[serde(flatten)]
    summary: CostSummary,
}

#[derive(Debug, Serialize)]
struct CostReport {
    month: String,
    start_date: String,
    end_date: String,
    complete: bool,
    pricing_as_of: &'static str,
    total_usd: f64,
    companies: Vec<CompanyCost>,
    platform: CostSummary,
    warnings: Vec<String>,
}

fn build_report(inputs: &Inputs, period: &Period, warnings: Vec<String>) -> CostReport {
    let mut ledger = Ledger::default();
    price(&mut ledger, inputs);
    let connection_total =
        |item: Item| -> f64 { ledger.0.values().filter_map(|items| items.get(&item)).sum() };
    let prices = TierPrices {
        sso: average_tier_price(connection_total(Item::WorkosSso)),
        directory_sync: average_tier_price(connection_total(Item::WorkosDirectorySync)),
    };
    let mut companies: Vec<_> = inputs
        .companies
        .iter()
        .map(|company| CompanyCost {
            company_id: company.id.clone(),
            name: company.name.clone(),
            summary: CostSummary::new(ledger.0.get(&Some(company.id.clone())), &prices),
        })
        .collect();
    companies.sort_by(|left, right| right.summary.total_usd.total_cmp(&left.summary.total_usd));
    let platform = CostSummary::new(ledger.0.get(&None), &prices);
    CostReport {
        month: period.month.clone(),
        start_date: period.start_date.clone(),
        end_date: period.end_date.clone(),
        complete: period.complete,
        pricing_as_of: PRICING_AS_OF,
        total_usd: companies
            .iter()
            .map(|company| company.summary.total_usd)
            .sum::<f64>()
            + platform.total_usd,
        companies,
        platform,
        warnings,
    }
}

/// The part of a UTC calendar month a report covers.
#[derive(Debug, PartialEq)]
struct Period {
    month: String,
    /// First and last UTC dates with usage, inclusive.
    start_date: String,
    end_date: String,
    /// The first moment after the month, as an Analytics Engine timestamp.
    next_month_start: String,
    complete: bool,
    month_fraction: f64,
}

fn report_period(
    requested: Option<&str>,
    now_ms: u64,
) -> std::result::Result<Period, &'static str> {
    let (current_year, current_month, today) = civil_date(now_ms);
    let (year, month) = match requested {
        None => (current_year, current_month),
        Some(value) => {
            let (year, month) = value.split_once('-').ok_or("month must be YYYY-MM")?;
            let year: i64 = year.parse().map_err(|_| "month must be YYYY-MM")?;
            let month: u32 = month.parse().map_err(|_| "month must be YYYY-MM")?;
            if year < 2000 || !(1..=12).contains(&month) {
                return Err("month must be YYYY-MM");
            }
            (year, month)
        }
    };
    let age = (current_year - year) * 12 + i64::from(current_month) - i64::from(month);
    if age < 0 {
        return Err("costs are not available for future months");
    }
    if age >= REPORT_MONTHS {
        return Err("costs are available for the current and previous two months");
    }
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let start_ms = unix_ms_from_civil(year, month, 1);
    let end_ms = unix_ms_from_civil(next_year, next_month, 1);
    let complete = age > 0;
    let last_day = if complete {
        civil_date((end_ms - 1) as u64).2
    } else {
        today
    };
    let elapsed_ms = if complete {
        end_ms - start_ms
    } else {
        now_ms as i64 - start_ms
    };
    Ok(Period {
        month: format!("{year:04}-{month:02}"),
        start_date: format!("{year:04}-{month:02}-01"),
        end_date: format!("{year:04}-{month:02}-{last_day:02}"),
        next_month_start: format!("{next_year:04}-{next_month:02}-01 00:00:00"),
        complete,
        month_fraction: elapsed_ms as f64 / (end_ms - start_ms) as f64,
    })
}

pub(crate) async fn platform_costs(request: &Request, environment: &Env) -> Result<Response> {
    if let Err(error) = authorize_platform_owner(request, environment).await {
        return workos_auth_error(error);
    }
    let requested = request
        .url()?
        .query_pairs()
        .find_map(|(key, value)| (key == "month").then(|| value.into_owned()));
    let now_ms = Date::now().as_millis();
    let period = match report_period(requested.as_deref(), now_ms) {
        Ok(period) => period,
        Err(message) => return api_error(400, message),
    };
    let db = environment.d1("DB")?;
    let mut inputs = load_d1_inputs(&db, &period, now_ms).await?;
    let mut warnings = Vec::new();

    let (cloudflare, workos) = futures_util::future::join(
        load_cloudflare_usage(environment, &period),
        load_workos_usage(environment),
    )
    .await;
    match cloudflare {
        Ok(usage) => {
            if usage.metered.is_empty() {
                warnings.push("No Workers requests or D1 rows have been metered for this month yet. They are counted from when metering was deployed.".into());
            }
            inputs.cloudflare = Some(usage);
        }
        Err(error) => {
            console_error!("event=platform_costs_cloudflare_failed error={}", error);
            warnings.push(format!("Cloudflare usage is unavailable: {error}"));
        }
    }
    match workos {
        Ok(usage) => inputs.workos = Some(usage),
        Err(error) => {
            console_error!("event=platform_costs_workos_failed error={}", error);
            warnings.push(format!("WorkOS connections are unavailable: {error}"));
        }
    }
    Response::from_json(&build_report(&inputs, &period, warnings))
}

async fn load_d1_inputs(db: &D1Database, period: &Period, now_ms: u64) -> Result<Inputs> {
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let (oldest_year, oldest_month, _) = civil_date(now_ms.saturating_sub(400 * 86_400_000));
    metered_batch(
        db,
        vec![
            query!(
                db,
                "DELETE FROM usage_object_owners WHERE kind = 'remote_session' AND created_at < ?1",
                now - SESSION_OWNER_RETENTION_MS
            )?,
            query!(
                db,
                "DELETE FROM company_active_users WHERE month < ?1",
                format!("{oldest_year:04}-{oldest_month:02}")
            )?,
        ],
    )
    .await?;

    let companies = query!(
        db,
        "SELECT id, name, slug, workos_organization_id FROM companies"
    )
    .metered_all()
    .await?
    .results::<CompanyRow>()?;

    #[derive(Deserialize)]
    struct ObjectOwner {
        object_name: String,
        company_id: String,
    }
    let object_owners = query!(
        db,
        "SELECT object_name, company_id FROM usage_object_owners"
    )
    .metered_all()
    .await?
    .results::<ObjectOwner>()?
    .into_iter()
    .map(|owner| (owner.object_name, owner.company_id))
    .collect();

    #[derive(Deserialize)]
    struct CompanyCount {
        company_id: String,
        count: f64,
    }
    let active_users = query!(
        db,
        "SELECT company_id, COUNT(*) AS count FROM company_active_users WHERE month = ?1 GROUP BY company_id",
        period.month
    )?
    .metered_all()
    .await?
    .results::<CompanyCount>()?
    .into_iter()
    .map(|row| (row.company_id, row.count))
    .collect();

    // The tables that grow with a company's use, which apportion D1 storage.
    let rows = query!(
        db,
        "SELECT company_id, SUM(count) AS count FROM (SELECT company_id, COUNT(*) AS count FROM agents GROUP BY company_id UNION ALL SELECT company_id, COUNT(*) FROM audit_events GROUP BY company_id UNION ALL SELECT company_id, COUNT(*) FROM agent_install_tokens GROUP BY company_id UNION ALL SELECT company_id, COUNT(*) FROM company_active_users GROUP BY company_id UNION ALL SELECT company_id, COUNT(*) FROM usage_object_owners GROUP BY company_id) GROUP BY company_id"
    )
    .metered_all()
    .await?;
    let d1_size_bytes = rows
        .meta()?
        .and_then(|meta| meta.size_after)
        .unwrap_or_default() as f64;
    let d1_rows = rows
        .results::<CompanyCount>()?
        .into_iter()
        .map(|row| (row.company_id, row.count))
        .collect();

    Ok(Inputs {
        month_fraction: period.month_fraction,
        companies,
        object_owners,
        active_users,
        d1_rows,
        d1_size_bytes,
        cloudflare: None,
        workos: None,
    })
}

async fn load_cloudflare_usage(environment: &Env, period: &Period) -> Result<CloudflareUsage> {
    let account = environment
        .var("CLOUDFLARE_ACCOUNT_ID")
        .map_err(|_| Error::RustError("CLOUDFLARE_ACCOUNT_ID is not configured".into()))?
        .to_string();
    let token = environment
        .secret("CLOUDFLARE_ANALYTICS_API_TOKEN")
        .map_err(|_| {
            Error::RustError("the CLOUDFLARE_ANALYTICS_API_TOKEN secret is not set".into())
        })?
        .to_string();
    validate_identifier(&account, "Cloudflare account ID")?;
    let turn_key = environment
        .secret("TURN_KEY_ID")
        .map(|key| key.to_string())
        .ok();

    let metered_sql = format!(
        "SELECT index1 AS owner, blob1 AS source, SUM(_sample_interval) AS data_points, SUM(_sample_interval * double1) AS requests, SUM(_sample_interval * double2) AS invocations, SUM(_sample_interval * double3) AS d1_rows_read, SUM(_sample_interval * double4) AS d1_rows_written FROM {USAGE_DATASET} WHERE timestamp >= toDateTime('{} 00:00:00') AND timestamp < toDateTime('{}') GROUP BY owner, source LIMIT 10000",
        period.start_date, period.next_month_start
    );
    let invocation_query = "query Usage($account: string!, $start: Date!, $end: Date!, $apiScript: string!, $scripts: [string!]) { viewer { accounts(filter: {accountTag: $account}) { workers: workersInvocationsAdaptive(limit: 100, filter: {date_geq: $start, date_leq: $end, scriptName_in: $scripts}) { dimensions { scriptName } sum { requests cpuTimeUs } } objects: durableObjectsInvocationsAdaptiveGroups(limit: 10000, filter: {date_geq: $start, date_leq: $end, scriptName: $apiScript}) { dimensions { namespaceId objectId name type } sum { requests } } turn: callsTurnUsageAdaptiveGroups(limit: 10000, filter: {date_geq: $start, date_leq: $end}) { dimensions { keyId customIdentifier } sum { egressBytes } } } } }";
    let variables = serde_json::json!({
        "account": account,
        "start": period.start_date,
        "end": period.end_date,
        "apiScript": API_SCRIPT,
        "scripts": [API_SCRIPT, DASHBOARD_SCRIPT],
    });
    let (metered, invocations) = futures_util::future::join(
        analytics_engine_sql::<MeteredRow>(&account, &token, metered_sql),
        cloudflare_graphql::<InvocationAnalytics>(&token, invocation_query, variables),
    )
    .await;
    let mut invocations = invocations?;
    // Other TURN keys on the account belong to other applications.
    if let Some(turn_key) = turn_key {
        invocations
            .turn
            .retain(|group| group.dimensions.key_id.as_deref() == Some(turn_key.as_str()));
    }

    let namespaces: BTreeSet<&str> = invocations
        .objects
        .iter()
        .map(|group| group.dimensions.namespace_id.as_str())
        .collect();
    let objects = if namespaces.is_empty() {
        ObjectAnalytics::default()
    } else {
        let object_query = "query Objects($account: string!, $start: Date!, $end: Date!, $namespaces: [string!]) { viewer { accounts(filter: {accountTag: $account}) { periodic: durableObjectsPeriodicGroups(limit: 10000, filter: {date_geq: $start, date_leq: $end, namespaceId_in: $namespaces}) { dimensions { namespaceId objectId name } sum { duration rowsRead rowsWritten inboundWebsocketMsgCount } } storage: durableObjectsSqlStorageGroups(limit: 100, filter: {date_geq: $start, date_leq: $end, namespaceId_in: $namespaces}) { dimensions { namespaceId } max { storedBytes } } } } }";
        let variables = serde_json::json!({
            "account": account,
            "start": period.start_date,
            "end": period.end_date,
            "namespaces": namespaces,
        });
        cloudflare_graphql::<ObjectAnalytics>(&token, object_query, variables).await?
    };
    Ok(CloudflareUsage {
        metered: metered?,
        invocations,
        objects,
    })
}

async fn analytics_engine_sql<T: DeserializeOwned>(
    account: &str,
    token: &str,
    sql: String,
) -> Result<Vec<T>> {
    #[derive(Deserialize)]
    struct SqlResponse<T> {
        data: Vec<T>,
    }
    let mut response = cloudflare_api(
        &format!("https://api.cloudflare.com/client/v4/accounts/{account}/analytics_engine/sql"),
        token,
        sql,
        "text/plain",
    )
    .await?;
    if !(200..300).contains(&response.status_code()) {
        let status = response.status_code();
        let detail = response.text().await.unwrap_or_default();
        return Err(Error::RustError(format!(
            "Analytics Engine returned HTTP {status}: {}",
            detail.chars().take(300).collect::<String>()
        )));
    }
    Ok(response.json::<SqlResponse<T>>().await?.data)
}

async fn cloudflare_graphql<T: DeserializeOwned + Default>(
    token: &str,
    query: &str,
    variables: serde_json::Value,
) -> Result<T> {
    #[derive(Deserialize)]
    struct GraphQlError {
        message: String,
    }
    #[derive(Deserialize)]
    struct Viewer<T> {
        accounts: Vec<T>,
    }
    #[derive(Deserialize)]
    struct Data<T> {
        viewer: Viewer<T>,
    }
    #[derive(Deserialize)]
    struct GraphQlResponse<T> {
        data: Option<Data<T>>,
        errors: Option<Vec<GraphQlError>>,
    }
    let body =
        serde_json::to_string(&serde_json::json!({ "query": query, "variables": variables }))?;
    let mut response = cloudflare_api(
        "https://api.cloudflare.com/client/v4/graphql",
        token,
        body,
        "application/json",
    )
    .await?;
    if !(200..300).contains(&response.status_code()) {
        return Err(Error::RustError(format!(
            "Cloudflare GraphQL returned HTTP {}",
            response.status_code()
        )));
    }
    let response: GraphQlResponse<T> = response.json().await?;
    if let Some(error) = response.errors.into_iter().flatten().next() {
        return Err(Error::RustError(format!(
            "Cloudflare GraphQL: {}",
            error.message
        )));
    }
    Ok(response
        .data
        .and_then(|data| data.viewer.accounts.into_iter().next())
        .unwrap_or_default())
}

async fn cloudflare_api(
    url: &str,
    token: &str,
    body: String,
    content_type: &str,
) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {token}"))?;
    headers.set("Content-Type", content_type)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.into()));
    Fetch::Request(Request::new_with_init(url, &init)?)
        .send()
        .await
}

async fn load_workos_usage(environment: &Env) -> Result<WorkOsUsage> {
    let (sso, directory_sync) = futures_util::future::join(
        count_workos_connections(environment, "/connections", &["active"]),
        count_workos_connections(environment, "/directories", &["active", "linked"]),
    )
    .await;
    Ok(WorkOsUsage {
        sso: sso?,
        directory_sync: directory_sync?,
    })
}

/// Counts the billable connections of each organization, across all pages.
async fn count_workos_connections(
    environment: &Env,
    path: &str,
    billable_states: &[&str],
) -> Result<HashMap<String, f64>> {
    #[derive(Deserialize)]
    struct Connection {
        #[serde(default)]
        organization_id: Option<String>,
        #[serde(default)]
        state: String,
    }
    #[derive(Deserialize)]
    struct Metadata {
        after: Option<String>,
    }
    #[derive(Deserialize)]
    struct Page {
        data: Vec<Connection>,
        list_metadata: Metadata,
    }
    /// Bounds the subrequests one report makes.
    const MAX_PAGES: usize = 20;
    let mut counts = HashMap::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("limit", "100");
        if let Some(cursor) = &after {
            query.append_pair("after", cursor);
        }
        let mut response = super::platform::workos_request(
            environment,
            Method::Get,
            &format!("{path}?{}", query.finish()),
            None,
        )
        .await?;
        if !(200..300).contains(&response.status_code()) {
            return Err(Error::RustError(format!(
                "WorkOS {path} returned HTTP {}",
                response.status_code()
            )));
        }
        let page: Page = response.json().await?;
        for connection in page.data {
            if let Some(organization_id) = connection.organization_id
                && billable_states.contains(&connection.state.as_str())
            {
                *counts.entry(organization_id).or_default() += 1.0;
            }
        }
        after = page.list_metadata.after;
        if after.is_none() {
            return Ok(counts);
        }
    }
    Err(Error::RustError(format!(
        "WorkOS {path} has more than {MAX_PAGES} pages"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn company(id: &str, slug: &str, organization: &str) -> CompanyRow {
        CompanyRow {
            id: id.into(),
            name: id.to_uppercase(),
            slug: Some(slug.into()),
            workos_organization_id: Some(organization.into()),
        }
    }

    fn metered(owner: &str, source: &str, requests: f64, invocations: f64) -> MeteredRow {
        MeteredRow {
            owner: owner.into(),
            source: source.into(),
            data_points: invocations,
            requests,
            invocations,
            d1_rows_read: 10.0,
            d1_rows_written: 2.0,
        }
    }

    fn object(
        object_id: &str,
        name: &str,
        kind: &str,
        requests: f64,
    ) -> Group<ObjectDimensions, Requests> {
        Group {
            dimensions: ObjectDimensions {
                namespace_id: "ns".into(),
                object_id: object_id.into(),
                name: Some(name.into()),
                kind: Some(kind.into()),
            },
            sum: Requests { requests },
        }
    }

    fn quantity(report: &CostReport, company_id: &str, item: Item) -> f64 {
        report
            .companies
            .iter()
            .find(|company| company.company_id == company_id)
            .and_then(|company| {
                company
                    .summary
                    .line_items
                    .iter()
                    .find(|line| line.item == item)
            })
            .map(|line| line.quantity)
            .unwrap_or_default()
    }

    fn platform_quantity(report: &CostReport, item: Item) -> f64 {
        report
            .platform
            .line_items
            .iter()
            .find(|line| line.item == item)
            .map(|line| line.quantity)
            .unwrap_or_default()
    }

    fn period() -> Period {
        report_period(Some("2026-08"), 1_790_362_666_000).unwrap()
    }

    #[test]
    fn periods_cover_the_month_so_far_or_all_of_a_past_month() {
        let now = 1_790_362_666_000; // 2026-09-25T18:57:46Z
        let current = report_period(None, now).unwrap();
        assert_eq!(current.month, "2026-09");
        assert_eq!(
            (current.start_date.as_str(), current.end_date.as_str()),
            ("2026-09-01", "2026-09-25")
        );
        assert_eq!(current.next_month_start, "2026-10-01 00:00:00");
        assert!(!current.complete);
        assert!((0.8..0.83).contains(&current.month_fraction));
        let past = report_period(Some("2026-08"), now).unwrap();
        assert_eq!(past.end_date, "2026-08-31");
        assert!(past.complete);
        assert_eq!(past.month_fraction, 1.0);
        let december = report_period(Some("2025-12"), 1_767_225_600_000).unwrap();
        assert_eq!(december.next_month_start, "2026-01-01 00:00:00");
        assert!(report_period(Some("2026-10"), now).is_err());
        assert!(report_period(Some("2026-06"), now).is_err());
        assert!(report_period(Some("2026-13"), now).is_err());
        assert!(report_period(Some("26-09"), now).is_err());
    }

    #[test]
    fn workos_connections_are_priced_at_graduated_tier_averages() {
        assert_eq!(average_tier_price(0.0), 125.0);
        assert_eq!(average_tier_price(15.0), 125.0);
        assert_eq!(
            average_tier_price(30.0),
            (15.0 * 125.0 + 15.0 * 100.0) / 30.0
        );
        assert_eq!(
            average_tier_price(60.0),
            (15.0 * 125.0 + 15.0 * 100.0 + 20.0 * 80.0 + 10.0 * 65.0) / 60.0
        );
    }

    #[test]
    fn analytics_engine_numbers_may_be_strings() {
        let row: MeteredRow = serde_json::from_value(serde_json::json!({
            "owner": "a", "source": "api", "data_points": "3", "requests": 2.0,
            "invocations": "3", "d1_rows_read": 4, "d1_rows_written": "0"
        }))
        .unwrap();
        assert_eq!(
            (row.data_points, row.requests, row.d1_rows_read),
            (3.0, 2.0, 4.0)
        );
    }

    #[test]
    fn usage_is_charged_to_the_company_that_caused_it() {
        let inputs = Inputs {
            month_fraction: 1.0,
            companies: vec![company("a", "acme", "org_a"), company("b", "beta", "org_b")],
            object_owners: HashMap::from([
                ("device-a".into(), "a".into()),
                ("session-b".into(), "b".into()),
            ]),
            active_users: HashMap::from([("a".into(), 3.0)]),
            d1_rows: HashMap::from([("a".into(), 30.0), ("b".into(), 10.0)]),
            d1_size_bytes: 4e9,
            cloudflare: Some(CloudflareUsage {
                metered: vec![
                    metered("a", "api", 5.0, 6.0),
                    metered("slug:beta", "dashboard", 4.0, 4.0),
                    metered("do:obj-b", "do", 0.0, 0.0),
                    metered(PLATFORM_OWNER, "dashboard", 7.0, 7.0),
                    metered("slug:unknown", "api", 1.0, 1.0),
                ],
                invocations: InvocationAnalytics {
                    workers: vec![Group {
                        dimensions: ScriptDimensions {
                            script_name: API_SCRIPT.into(),
                        },
                        sum: ScriptSum {
                            requests: 10.0,
                            cpu_time_us: 10_000.0,
                        },
                    }],
                    objects: vec![
                        object("obj-a", "device-a", "http", 4.0),
                        object("obj-a", "device-a", "hibernation", 40.0),
                        object("obj-b", "session-b", "alarm", 1.0),
                        object("obj-p", "b", "http", 2.0),
                        object("obj-x", "someone-else", "http", 8.0),
                    ],
                    turn: vec![
                        Group {
                            dimensions: TurnDimensions {
                                custom_identifier: "a".into(),
                                key_id: None,
                            },
                            sum: EgressBytes { egress_bytes: 2e9 },
                        },
                        Group {
                            dimensions: TurnDimensions {
                                custom_identifier: String::new(),
                                key_id: None,
                            },
                            sum: EgressBytes { egress_bytes: 1e9 },
                        },
                    ],
                },
                objects: ObjectAnalytics {
                    periodic: vec![Group {
                        dimensions: ObjectDimensions {
                            namespace_id: "ns".into(),
                            object_id: "obj-a".into(),
                            name: None,
                            kind: None,
                        },
                        sum: ObjectPeriodicSum {
                            duration: 100.0,
                            rows_read: 7.0,
                            rows_written: 3.0,
                            inbound_websocket_msg_count: 20.0,
                        },
                    }],
                    storage: vec![Group {
                        dimensions: NamespaceDimensions {
                            namespace_id: "ns".into(),
                        },
                        sum: StoredBytes { stored_bytes: 5e9 },
                    }],
                },
            }),
            workos: Some(WorkOsUsage {
                sso: HashMap::from([("org_b".into(), 2.0), ("org_gone".into(), 1.0)]),
                directory_sync: HashMap::new(),
            }),
        };
        let report = build_report(&inputs, &period(), Vec::new());

        assert_eq!(quantity(&report, "a", Item::WorkersRequests), 5.0);
        assert_eq!(quantity(&report, "b", Item::WorkersRequests), 4.0);
        assert_eq!(platform_quantity(&report, Item::WorkersRequests), 8.0);
        // D1 used inside session-b's object is charged to b.
        assert_eq!(quantity(&report, "b", Item::D1RowsRead), 20.0);
        // CPU follows API invocations; the remainder stays with the platform.
        assert_eq!(quantity(&report, "a", Item::WorkersCpu), 6.0);
        assert_eq!(platform_quantity(&report, Item::WorkersCpu), 4.0);
        // Hibernated WebSocket messages and inbound messages bill at 20:1.
        assert_eq!(
            quantity(&report, "a", Item::DurableObjectRequests),
            4.0 + 2.0 + 1.0
        );
        assert_eq!(quantity(&report, "b", Item::DurableObjectRequests), 3.0);
        assert_eq!(platform_quantity(&report, Item::DurableObjectRequests), 8.0);
        assert_eq!(quantity(&report, "a", Item::DurableObjectDuration), 100.0);
        // Storage follows object counts: a has 1 of 4 objects in the namespace.
        assert_eq!(
            quantity(&report, "a", Item::DurableObjectStorage),
            5.0 / 4.0
        );
        assert_eq!(
            quantity(&report, "b", Item::DurableObjectStorage),
            5.0 / 2.0
        );
        assert_eq!(quantity(&report, "a", Item::D1Storage), 3.0);
        assert_eq!(quantity(&report, "a", Item::TurnEgress), 2.0);
        assert_eq!(platform_quantity(&report, Item::TurnEgress), 1.0);
        assert_eq!(quantity(&report, "a", Item::WorkosActiveUsers), 3.0);
        assert_eq!(quantity(&report, "b", Item::WorkosSso), 2.0);
        assert_eq!(platform_quantity(&report, Item::WorkosSso), 1.0);
        assert_eq!(platform_quantity(&report, Item::WorkersPaidPlan), 1.0);

        let b = report
            .companies
            .iter()
            .find(|company| company.company_id == "b")
            .unwrap();
        assert_eq!(b.summary.workos_usd, 250.0);
        let total: f64 = report
            .companies
            .iter()
            .map(|company| company.summary.total_usd)
            .sum();
        assert!((report.total_usd - total - report.platform.total_usd).abs() < 1e-9);
        assert_eq!(
            report.companies[0].company_id, "b",
            "the costliest company is listed first"
        );
    }
}

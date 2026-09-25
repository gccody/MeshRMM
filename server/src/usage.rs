//! Per-company usage metering for the platform cost report.
//!
//! Every Worker invocation and Durable Object event runs inside a meter
//! scope. D1 statements executed through [`MeteredStatement`] or
//! [`metered_batch`] add their billed row counts to the current scope, and the
//! scope is written to Workers Analytics Engine when the invocation finishes.
//! Cloudflare's own analytics cover what can be attributed without help:
//! Durable Objects by object name and TURN traffic by custom identifier.
use serde::de::DeserializeOwned;
use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Mutex;
use std::task::{Context as TaskContext, Poll};
use worker::{
    AnalyticsEngineDataPointBuilder, D1Database, D1PreparedStatement, D1Result, Env, Error, Result,
    State, console_warn, query,
};

/// The Analytics Engine binding both Workers write usage to.
pub(crate) const USAGE_BINDING: &str = "USAGE";
/// Usage no company can be charged for: the admin console, marketing pages,
/// and requests that fail before a company is known.
pub(crate) const PLATFORM_OWNER: &str = "_platform";
/// Usage written by the dashboard Worker for a company hostname it has not
/// resolved to a company ID. The cost report maps the slug to its company.
pub(crate) const SLUG_OWNER_PREFIX: &str = "slug:";
/// D1 usage inside a Durable Object, keyed by the object's hex ID. The cost
/// report maps the ID to the object's name, and the name to its company.
pub(crate) const OBJECT_OWNER_PREFIX: &str = "do:";

/// What one invocation used. Written as one Analytics Engine data point:
/// index1 is the owner, blob1 the source, and double1..4 the fields in order.
#[derive(Debug, Default)]
struct Usage {
    company_id: Option<String>,
    fallback_owner: Option<String>,
    billable_requests: f64,
    invocations: f64,
    d1_rows_read: f64,
    d1_rows_written: f64,
}

impl Usage {
    fn owner(&self) -> &str {
        self.company_id
            .as_deref()
            .or(self.fallback_owner.as_deref())
            .unwrap_or(PLATFORM_OWNER)
    }
}

/// Where an invocation ran, which decides how its usage is recorded.
pub(crate) enum Source<'a> {
    /// A request to the API Worker. Requests that arrive through the dashboard
    /// Worker's service binding are not billed again, so they only count as
    /// invocations, which apportion the Worker's CPU time.
    Api {
        billable: bool,
        tenant_slug: Option<&'a str>,
    },
    /// A Durable Object event. Cloudflare reports the object's requests and
    /// duration itself, so only D1 usage is recorded, and only when there is some.
    DurableObject { object_id: String },
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<RefCell<Usage>>>> = const { RefCell::new(None) };
}

/// Makes `usage` the current scope whenever `inner` is polled. Requests in one
/// isolate interleave at await points, so the scope is set per poll.
struct Scoped<F> {
    usage: Rc<RefCell<Usage>>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<F::Output> {
        let previous = CURRENT.with(|current| current.replace(Some(self.usage.clone())));
        let result = self.inner.as_mut().poll(cx);
        CURRENT.with(|current| *current.borrow_mut() = previous);
        result
    }
}

/// Runs `future` as one metered invocation and records what it used.
pub(crate) async fn metered<F: Future>(
    environment: &Env,
    source: Source<'_>,
    future: F,
) -> F::Output {
    let mut usage = Usage::default();
    let label = match &source {
        Source::Api {
            billable,
            tenant_slug,
        } => {
            usage.invocations = 1.0;
            usage.billable_requests = if *billable { 1.0 } else { 0.0 };
            usage.fallback_owner = tenant_slug.map(|slug| format!("{SLUG_OWNER_PREFIX}{slug}"));
            "api"
        }
        Source::DurableObject { object_id } => {
            usage.fallback_owner = Some(format!("{OBJECT_OWNER_PREFIX}{object_id}"));
            "do"
        }
    };
    let usage = Rc::new(RefCell::new(usage));
    let output = Scoped {
        usage: usage.clone(),
        inner: Box::pin(future),
    }
    .await;
    let usage = usage.take();
    let used_d1 = usage.d1_rows_read > 0.0 || usage.d1_rows_written > 0.0;
    if matches!(source, Source::Api { .. }) || used_d1 {
        write(environment, label, &usage);
    }
    output
}

/// Runs one Durable Object event as a metered invocation.
pub(crate) async fn metered_object<F: Future>(
    state: &State,
    environment: &Env,
    future: F,
) -> F::Output {
    let source = Source::DurableObject {
        object_id: state.id().to_string(),
    };
    metered(environment, source, future).await
}

fn write(environment: &Env, source: &str, usage: &Usage) {
    // Local development and tests run without the binding.
    let Ok(dataset) = environment.analytics_engine(USAGE_BINDING) else {
        return;
    };
    let point = AnalyticsEngineDataPointBuilder::new()
        .indexes([usage.owner()])
        .add_blob(source)
        .doubles([
            usage.billable_requests,
            usage.invocations,
            usage.d1_rows_read,
            usage.d1_rows_written,
        ])
        .build();
    if let Err(error) = dataset.write_data_point(&point) {
        console_warn!("event=usage_write_failed error={}", error);
    }
}

fn with_current(update: impl FnOnce(&mut Usage)) {
    CURRENT.with(|current| {
        if let Some(usage) = current.borrow().as_ref() {
            update(&mut usage.borrow_mut());
        }
    });
}

/// Charges the current invocation to `company_id`. The first company wins,
/// so a request cannot move its usage to another company afterwards.
pub(crate) fn attribute_company(company_id: &str) {
    with_current(|usage| {
        if usage.company_id.is_none() {
            usage.company_id = Some(company_id.to_owned());
        }
    });
}

fn record(result: &D1Result) {
    let Ok(Some(meta)) = result.meta() else {
        return;
    };
    with_current(|usage| {
        usage.d1_rows_read += meta.rows_read.unwrap_or_default() as f64;
        usage.d1_rows_written += meta.rows_written.unwrap_or_default() as f64;
    });
}

/// D1 statements that record the rows they read and write. D1 bills by rows,
/// and only full results carry the counts, so `metered_first` runs the
/// statement and picks the first row itself.
pub(crate) trait MeteredStatement {
    async fn metered_first<T: DeserializeOwned>(&self, column: Option<&str>) -> Result<Option<T>>;
    async fn metered_run(&self) -> Result<D1Result>;
    async fn metered_all(&self) -> Result<D1Result>;
}

impl MeteredStatement for D1PreparedStatement {
    async fn metered_first<T: DeserializeOwned>(&self, column: Option<&str>) -> Result<Option<T>> {
        let result = self.metered_run().await?;
        let Some(column) = column else {
            return Ok(result.results::<T>()?.into_iter().next());
        };
        let Some(mut row) = result
            .results::<serde_json::Map<String, serde_json::Value>>()?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let value = row
            .remove(column)
            .ok_or_else(|| Error::RustError(format!("D1 result has no column {column}")))?;
        Ok(Some(serde_json::from_value(value)?))
    }

    async fn metered_run(&self) -> Result<D1Result> {
        let result = self.run().await?;
        record(&result);
        Ok(result)
    }

    async fn metered_all(&self) -> Result<D1Result> {
        let result = self.all().await?;
        record(&result);
        Ok(result)
    }
}

pub(crate) async fn metered_batch(
    db: &D1Database,
    statements: Vec<D1PreparedStatement>,
) -> Result<Vec<D1Result>> {
    let results = db.batch(statements).await?;
    results.iter().for_each(record);
    Ok(results)
}

/// Users already recorded as active by this isolate, as `month|company|user`.
static RECORDED_USERS: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Bounds the cache; clearing it only costs one ignored insert per user.
const MAX_RECORDED_USERS: usize = 10_000;

/// Records `user_id` as a monthly active user of `company_id`, which is how
/// WorkOS counts AuthKit users. A failure is logged and never blocks the request.
pub(crate) async fn record_active_user(
    db: &D1Database,
    company_id: &str,
    user_id: &str,
    now_ms: u64,
) {
    let month = month_key(now_ms);
    let key = format!("{month}|{company_id}|{user_id}");
    let cached = RECORDED_USERS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|users| users.contains(&key));
    if cached {
        return;
    }
    let inserted = async {
        query!(
            db,
            "INSERT OR IGNORE INTO company_active_users (company_id, month, user_id, first_seen_at) VALUES (?1, ?2, ?3, ?4)",
            company_id,
            month,
            user_id,
            i64::try_from(now_ms).unwrap_or(i64::MAX)
        )?
        .metered_run()
        .await
    }
    .await;
    match inserted {
        Ok(_) => {
            let mut users = RECORDED_USERS
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let users = users.get_or_insert_with(HashSet::new);
            if users.len() >= MAX_RECORDED_USERS {
                users.clear();
            }
            users.insert(key);
        }
        Err(error) => console_warn!("event=active_user_record_failed error={}", error),
    }
}

/// The UTC calendar date of a Unix timestamp in milliseconds.
pub(crate) fn civil_date(unix_ms: u64) -> (i64, u32, u32) {
    // Howard Hinnant's days-to-civil algorithm.
    let days = (unix_ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Milliseconds from the Unix epoch to the start of a UTC calendar date.
pub(crate) fn unix_ms_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era - 719_468) * 86_400_000
}

/// The UTC month of a Unix timestamp in milliseconds, as `YYYY-MM`.
pub(crate) fn month_key(unix_ms: u64) -> String {
    let (year, month, _) = civil_date(unix_ms);
    format!("{year:04}-{month:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_conversions_round_trip() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        // 2026-09-25T18:57:46Z
        assert_eq!(civil_date(1_790_362_666_000), (2026, 9, 25));
        assert_eq!(month_key(1_790_362_666_000), "2026-09");
        for (year, month, day) in [(1970, 1, 1), (2024, 2, 29), (2026, 12, 31), (2027, 1, 1)] {
            let ms = unix_ms_from_civil(year, month, day);
            assert_eq!(civil_date(ms as u64), (year, month, day));
            assert_eq!(civil_date(ms as u64 + 86_399_999), (year, month, day));
        }
        assert_eq!(
            unix_ms_from_civil(2026, 10, 1) - unix_ms_from_civil(2026, 9, 1),
            30 * 86_400_000
        );
    }

    #[test]
    fn usage_is_owned_by_its_company_then_its_hostname() {
        let mut usage = Usage {
            fallback_owner: Some("slug:acme".into()),
            ..Usage::default()
        };
        assert_eq!(usage.owner(), "slug:acme");
        usage.company_id = Some("company".into());
        assert_eq!(usage.owner(), "company");
        assert_eq!(Usage::default().owner(), PLATFORM_OWNER);
    }
}

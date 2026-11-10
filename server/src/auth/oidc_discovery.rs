//! The SSO provider's discovery document and signing keys, kept between
//! sign-ins. Fetching them for every sign-in would let anyone make the server
//! send two requests to the provider per request, slow every sign-in down,
//! and fail sign-ins whenever the provider's discovery endpoint blips.
//!
//! There is one provider, so this keeps one entry, for the provider's
//! current settings: an edit to them (a new `updated_at`) starts afresh.
//! Concurrent sign-ins that find nothing usable share one fetch.
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::anyhow;
use openidconnect::core::CoreProviderMetadata;

use super::oidc::{self, Provider};

/// How long the cache trusts what it fetched, and how hard it may press the
/// provider.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// How long a fetched document is used before it is fetched again.
    pub lifetime: Duration,
    /// How much longer an expired document is used while fetching it again
    /// fails.
    pub grace: Duration,
    /// How long a failed fetch is remembered, so a stampede of sign-ins
    /// against a provider that is down makes one request, not one each.
    pub retry_after_failure: Duration,
    /// How often an ID token signed with an unknown key may fetch the keys
    /// again.
    pub key_refetch_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_secs(60 * 60),
            grace: Duration::from_secs(6 * 60 * 60),
            retry_after_failure: Duration::from_secs(5),
            key_refetch_interval: Duration::from_secs(60),
        }
    }
}

/// The provider's metadata and keys, as fetched for one version of its
/// settings.
#[derive(Debug)]
pub struct Discovered {
    pub issuer_url: String,
    pub provider_updated_at: i64,
    pub metadata: CoreProviderMetadata,
}

impl Discovered {
    fn is_for(&self, issuer_url: &str, updated_at: i64) -> bool {
        self.issuer_url == issuer_url && self.provider_updated_at == updated_at
    }
}

#[derive(Debug)]
struct Current {
    entry: Arc<Discovered>,
    /// When the discovery document was fetched.
    fetched_at: Instant,
    /// When the keys were last fetched, or tried, by themselves.
    keys_tried_at: Option<Instant>,
}

#[derive(Debug)]
struct Failure {
    issuer_url: String,
    updated_at: i64,
    at: Instant,
    message: String,
}

#[derive(Debug, Default)]
struct Inner {
    current: Option<Current>,
    failure: Option<Failure>,
}

#[derive(Debug, Default)]
pub struct DiscoveryCache {
    timings: Timings,
    inner: Mutex<Inner>,
    /// Held while fetching, so only one fetch runs at a time.
    fetching: tokio::sync::Mutex<()>,
}

impl DiscoveryCache {
    pub fn new(timings: Timings) -> Self {
        Self {
            timings,
            ..Self::default()
        }
    }

    /// The provider's metadata and keys: kept ones while they are fresh,
    /// otherwise fetched. If fetching fails, ones fetched before are used
    /// for the grace period.
    pub async fn get(&self, provider: &Provider) -> anyhow::Result<Arc<Discovered>> {
        if let Some(entry) = self.fresh(provider, Instant::now()) {
            return Ok(entry);
        }
        let _fetching = self.fetching.lock().await;
        let now = Instant::now();
        // Another sign-in may have fetched them while this one waited.
        if let Some(entry) = self.fresh(provider, now) {
            return Ok(entry);
        }
        let stale = self.stale(provider, now);
        if let Some(message) = self.recent_failure(provider, now) {
            return stale.ok_or_else(|| anyhow!(message));
        }
        match oidc::discover(&provider.issuer_url).await {
            Ok(metadata) => Ok(self.store(&provider.issuer_url, provider.updated_at, metadata)),
            Err(error) => {
                let message = format!("{error:#}");
                self.lock().failure = Some(Failure {
                    issuer_url: provider.issuer_url.clone(),
                    updated_at: provider.updated_at,
                    at: Instant::now(),
                    message: message.clone(),
                });
                let entry = stale.ok_or(error)?;
                tracing::warn!(
                    error = message,
                    "could not refresh the SSO provider's metadata; using what was fetched before"
                );
                Ok(entry)
            }
        }
    }

    /// Keeps metadata just fetched for these settings, e.g. when an
    /// administrator saved them.
    pub fn prime(&self, issuer_url: &str, updated_at: i64, metadata: CoreProviderMetadata) {
        self.store(issuer_url, updated_at, metadata);
    }

    /// Forgets everything, e.g. when the provider is removed.
    pub fn clear(&self) {
        *self.lock() = Inner::default();
    }

    /// Newer keys than `tried`'s, for an ID token that none of them
    /// verify: the kept ones if they were refreshed since, otherwise the
    /// keys fetched again, at most once per [`Timings::key_refetch_interval`]
    /// so tokens with made-up key IDs can't make the server hammer the
    /// provider. `None` if there are none.
    pub async fn refresh_keys(
        &self,
        provider: &Provider,
        tried: &Arc<Discovered>,
    ) -> Option<Arc<Discovered>> {
        let _fetching = self.fetching.lock().await;
        {
            let now = Instant::now();
            let mut inner = self.lock();
            let current = inner
                .current
                .as_mut()
                .filter(|current| {
                    current
                        .entry
                        .is_for(&provider.issuer_url, provider.updated_at)
                })
                .filter(|current| {
                    current
                        .entry
                        .is_for(&tried.issuer_url, tried.provider_updated_at)
                })?;
            if !Arc::ptr_eq(&current.entry, tried) {
                return Some(current.entry.clone());
            }
            if current.keys_tried_at.is_some_and(|at| {
                now.saturating_duration_since(at) < self.timings.key_refetch_interval
            }) {
                return None;
            }
            current.keys_tried_at = Some(now);
        }
        let keys = match oidc::fetch_keys(tried.metadata.jwks_uri()).await {
            Ok(keys) => keys,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not fetch the SSO provider's signing keys"
                );
                return None;
            }
        };
        let entry = Arc::new(Discovered {
            issuer_url: tried.issuer_url.clone(),
            provider_updated_at: tried.provider_updated_at,
            metadata: tried.metadata.clone().set_jwks(keys),
        });
        let mut inner = self.lock();
        // The settings may have been saved meanwhile; keep what that kept.
        if let Some(current) = inner
            .current
            .as_mut()
            .filter(|current| Arc::ptr_eq(&current.entry, tried))
        {
            current.entry = entry.clone();
        }
        Some(entry)
    }

    fn store(
        &self,
        issuer_url: &str,
        updated_at: i64,
        metadata: CoreProviderMetadata,
    ) -> Arc<Discovered> {
        let entry = Arc::new(Discovered {
            issuer_url: issuer_url.to_owned(),
            provider_updated_at: updated_at,
            metadata,
        });
        // Replacing the one entry drops any kept for earlier settings.
        *self.lock() = Inner {
            current: Some(Current {
                entry: entry.clone(),
                fetched_at: Instant::now(),
                keys_tried_at: None,
            }),
            failure: None,
        };
        entry
    }

    /// The kept entry for `provider` if it is younger than `max_age`.
    fn kept(
        &self,
        provider: &Provider,
        now: Instant,
        max_age: Duration,
    ) -> Option<Arc<Discovered>> {
        self.lock()
            .current
            .as_ref()
            .filter(|current| {
                current
                    .entry
                    .is_for(&provider.issuer_url, provider.updated_at)
            })
            .filter(|current| now.saturating_duration_since(current.fetched_at) < max_age)
            .map(|current| current.entry.clone())
    }

    fn fresh(&self, provider: &Provider, now: Instant) -> Option<Arc<Discovered>> {
        self.kept(provider, now, self.timings.lifetime)
    }

    fn stale(&self, provider: &Provider, now: Instant) -> Option<Arc<Discovered>> {
        self.kept(provider, now, self.timings.lifetime + self.timings.grace)
    }

    /// Why fetching for `provider` failed, if it failed a moment ago.
    fn recent_failure(&self, provider: &Provider, now: Instant) -> Option<String> {
        self.lock()
            .failure
            .as_ref()
            .filter(|failure| {
                failure.issuer_url == provider.issuer_url
                    && failure.updated_at == provider.updated_at
                    && now.saturating_duration_since(failure.at) < self.timings.retry_after_failure
            })
            .map(|failure| failure.message.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves only a cache behind.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

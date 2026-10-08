//! Signing in: passwords, second factors, passkeys, SSO, sessions and the
//! limits on guessing.
pub mod ceremonies;
pub mod limits;
pub mod oidc;
pub mod passkeys;
pub mod password;
pub mod second_factor;
pub mod session;

use std::time::Duration;

use webauthn_rs::prelude::{DiscoverableAuthentication, PasskeyRegistration};

pub use self::session::{Authorized, SignedIn};
use self::{
    ceremonies::Ceremonies,
    limits::{Challenges, RateLimiter, SetupGate},
};

const WINDOW: Duration = Duration::from_secs(15 * 60);
/// How long a correct password waits for its second factor.
const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);
const CHALLENGE_ATTEMPTS: u32 = 5;
/// How long a browser has to answer a passkey prompt.
const PASSKEY_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a sign-in may spend at the SSO provider.
const SSO_TTL: Duration = Duration::from_secs(10 * 60);

/// A passkey registration waiting for the browser's answer.
#[derive(Debug)]
pub struct PendingRegistration {
    pub user_id: String,
    pub state: PasskeyRegistration,
}

/// Sign-in state kept in memory.
#[derive(Debug)]
pub struct AuthState {
    /// Failed sign-ins per client address.
    pub sign_in_by_ip: RateLimiter,
    /// Failed sign-ins per account, however many addresses try.
    pub sign_in_by_account: RateLimiter,
    /// Wrong passwords re-entered to confirm an account change, per user.
    pub password_confirmations: RateLimiter,
    /// Setup, invitation and reset tokens tried per client address.
    pub token_attempts: RateLimiter,
    /// Password reset emails requested per client address.
    pub reset_requests: RateLimiter,
    /// Passkey sign-ins and SSO redirects started per client address.
    pub ceremony_starts: RateLimiter,
    pub challenges: Challenges,
    pub setup: SetupGate,
    pub passkey_registrations: Ceremonies<PendingRegistration>,
    pub passkey_sign_ins: Ceremonies<DiscoverableAuthentication>,
    pub sso_sign_ins: Ceremonies<oidc::Pending>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            sign_in_by_ip: RateLimiter::new(30, WINDOW),
            sign_in_by_account: RateLimiter::new(10, WINDOW),
            password_confirmations: RateLimiter::new(10, WINDOW),
            token_attempts: RateLimiter::new(30, WINDOW),
            reset_requests: RateLimiter::new(5, WINDOW),
            ceremony_starts: RateLimiter::new(60, WINDOW),
            challenges: Challenges::new(CHALLENGE_TTL, CHALLENGE_ATTEMPTS),
            setup: SetupGate::default(),
            passkey_registrations: Ceremonies::new(PASSKEY_TTL),
            passkey_sign_ins: Ceremonies::new(PASSKEY_TTL),
            sso_sign_ins: Ceremonies::new(SSO_TTL),
        }
    }
}

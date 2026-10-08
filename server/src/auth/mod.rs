//! Signing in: passwords, second factors, sessions and the limits on guessing.
pub mod limits;
pub mod password;
pub mod second_factor;
pub mod session;

use std::time::Duration;

use self::limits::{Challenges, RateLimiter, SetupGate};
pub use self::session::{Authorized, SignedIn};

const WINDOW: Duration = Duration::from_secs(15 * 60);
/// How long a correct password waits for its second factor.
const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);
const CHALLENGE_ATTEMPTS: u32 = 5;

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
    pub challenges: Challenges,
    pub setup: SetupGate,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            sign_in_by_ip: RateLimiter::new(30, WINDOW),
            sign_in_by_account: RateLimiter::new(10, WINDOW),
            password_confirmations: RateLimiter::new(10, WINDOW),
            token_attempts: RateLimiter::new(30, WINDOW),
            reset_requests: RateLimiter::new(5, WINDOW),
            challenges: Challenges::new(CHALLENGE_TTL, CHALLENGE_ATTEMPTS),
            setup: SetupGate::default(),
        }
    }
}

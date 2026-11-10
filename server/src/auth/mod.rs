//! Signing in: passwords, second factors, passkeys, SSO, sessions and the
//! limits on guessing.
pub mod accounts;
pub mod ceremonies;
pub mod devices;
pub mod limits;
pub mod oidc;
pub mod oidc_discovery;
pub mod passkeys;
pub mod password;
pub mod second_factor;
pub mod session;

use std::{net::IpAddr, time::Duration};

use webauthn_rs::prelude::{DiscoverableAuthentication, PasskeyRegistration};

pub use self::session::{Authorized, SignedIn};
use self::{
    ceremonies::Ceremonies,
    limits::{Challenges, RateLimiter, SetupGate, account_key, ip_key},
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

/// A passkey sign-in waiting for the browser's answer.
#[derive(Debug)]
pub struct PendingSignIn {
    /// The [`AuthState::ceremony_starts`] key the prompt was counted under,
    /// handed back once a passkey answers it.
    pub ip_key: String,
    pub state: DiscoverableAuthentication,
}

/// Sign-in state kept in memory.
#[derive(Debug)]
pub struct AuthState {
    /// Failed sign-ins per client address.
    pub sign_in_by_ip: RateLimiter,
    /// Wrong passwords per account and client address: what a guesser
    /// exhausts, without touching other addresses' attempts at the account.
    pub sign_in_by_pair: RateLimiter,
    /// Wrong passwords per account from a browser with its device cookie,
    /// instead of the pair and account-wide limits.
    pub sign_in_by_device: RateLimiter,
    /// Wrong passwords per account, however many addresses try: the bound on
    /// guessing from many addresses. Browsers with the account's device
    /// cookie are neither counted nor refused by it.
    pub sign_in_by_account: RateLimiter,
    /// Wrong second-factor codes per user, however many challenges they
    /// span. Only someone with the password gets that far.
    pub second_factor_by_user: RateLimiter,
    /// Wrong passwords re-entered to confirm an account change, per user.
    pub password_confirmations: RateLimiter,
    /// Setup, invitation and reset tokens tried per client address.
    pub token_attempts: RateLimiter,
    /// Password reset emails requested per client address.
    pub reset_requests: RateLimiter,
    /// Passkey sign-ins and SSO redirects started per client address: a cap
    /// on the anonymous prompts the server keeps. A passkey sign-in that a
    /// registered passkey answers hands its start back, as its prompt is
    /// gone, so people signing in from one office address don't use it up.
    pub ceremony_starts: RateLimiter,
    /// Unknown personal API tokens tried per client address.
    pub api_token_failures: RateLimiter,
    pub challenges: Challenges,
    pub setup: SetupGate,
    pub passkey_registrations: Ceremonies<PendingRegistration>,
    pub passkey_sign_ins: Ceremonies<PendingSignIn>,
    pub sso_sign_ins: Ceremonies<oidc::Pending>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            sign_in_by_ip: RateLimiter::new(30, WINDOW),
            sign_in_by_pair: RateLimiter::new(10, WINDOW),
            sign_in_by_device: RateLimiter::new(10, WINDOW),
            sign_in_by_account: RateLimiter::new(50, WINDOW),
            second_factor_by_user: RateLimiter::new(10, WINDOW),
            password_confirmations: RateLimiter::new(10, WINDOW),
            token_attempts: RateLimiter::new(30, WINDOW),
            reset_requests: RateLimiter::new(5, WINDOW),
            ceremony_starts: RateLimiter::new(60, WINDOW),
            api_token_failures: RateLimiter::new(30, WINDOW),
            challenges: Challenges::new(CHALLENGE_TTL, CHALLENGE_ATTEMPTS),
            setup: SetupGate::default(),
            passkey_registrations: Ceremonies::new(PASSKEY_TTL),
            passkey_sign_ins: Ceremonies::new(PASSKEY_TTL),
            sso_sign_ins: Ceremonies::new(SSO_TTL),
        }
    }
}

/// The limits a password attempt was counted under, to hand back if the
/// password is right.
#[derive(Debug)]
pub struct PasswordAttempt {
    ip: String,
    account: Option<Account>,
}

#[derive(Debug)]
struct Account {
    key: String,
    /// The account and the client address, or with a device cookie the
    /// account and the device.
    own: String,
    device: bool,
}

impl AuthState {
    /// Counts a password attempt from `ip` at the account with the normalised
    /// `email` (`None` if the address isn't valid, which no account has),
    /// from a browser whose device cookie for that account names `device`.
    /// Unknown accounts are counted like known ones, so a refusal doesn't
    /// tell them apart. Returns `Err(seconds)` if a limit is used up.
    pub fn count_password_attempt(
        &self,
        ip: IpAddr,
        email: Option<&str>,
        device: Option<&str>,
    ) -> Result<PasswordAttempt, u64> {
        let ip = ip_key(ip);
        self.sign_in_by_ip.hit(&ip)?;
        let Some(email) = email else {
            return Ok(PasswordAttempt { ip, account: None });
        };
        let key = account_key(email);
        let account = match device {
            Some(device) => Account {
                own: format!("{key}/{device}"),
                key,
                device: true,
            },
            None => Account {
                own: format!("{key}/{ip}"),
                key,
                device: false,
            },
        };
        let own = self.own_limit(&account);
        if let Err(seconds) = own.hit(&account.own) {
            self.sign_in_by_ip.refund(&ip);
            return Err(seconds);
        }
        if !account.device
            && let Err(seconds) = self.sign_in_by_account.hit(&account.key)
        {
            own.refund(&account.own);
            self.sign_in_by_ip.refund(&ip);
            return Err(seconds);
        }
        Ok(PasswordAttempt {
            ip,
            account: Some(account),
        })
    }

    /// A right password isn't a guess: hands back its counts and forgets
    /// the earlier failures from the same address or device, but not other
    /// addresses' failures at the account.
    pub fn password_accepted(&self, attempt: &PasswordAttempt) {
        self.sign_in_by_ip.refund(&attempt.ip);
        if let Some(account) = &attempt.account {
            self.own_limit(account).clear(&account.own);
            if !account.device {
                self.sign_in_by_account.refund(&account.key);
            }
        }
    }

    fn own_limit(&self, account: &Account) -> &RateLimiter {
        if account.device {
            &self.sign_in_by_device
        } else {
            &self.sign_in_by_pair
        }
    }
}

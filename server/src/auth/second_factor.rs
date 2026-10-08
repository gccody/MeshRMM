//! Authenticator-app codes (TOTP, RFC 6238) and one-time recovery codes.
use sea_query::{Expr, ExprTrait, Func, Query};
use subtle::ConstantTimeEq;
use totp_rs::{Algorithm, TOTP};

use crate::{
    db::{
        self, Executor,
        tables::{UserRecoveryCodes, UserTotp},
    },
    secrets::{InstanceKey, random_bytes, token_hash},
    time::now_ms,
    users::new_id,
};

const DIGITS: usize = 6;
const STEP_SECONDS: u64 = 30;
/// Codes from one step either side of now are accepted, for clock drift.
const SKEW_STEPS: u64 = 1;
/// 160 bits, the size RFC 4226 recommends for HMAC-SHA1.
pub const SECRET_BYTES: usize = 20;

pub const RECOVERY_CODE_COUNT: usize = 10;
/// Without 0/o, 1/l/i, which are easy to misread.
const RECOVERY_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const RECOVERY_CODE_LENGTH: usize = 10;

pub fn new_totp_secret() -> [u8; SECRET_BYTES] {
    random_bytes()
}

fn totp(secret: &[u8], issuer: &str, account: &str) -> TOTP {
    // The otpauth URI separates issuer and account with ':', so neither may
    // contain one.
    let clean = |text: &str| text.replace(':', " ");
    TOTP::new_unchecked(
        Algorithm::SHA1,
        DIGITS,
        // Skew is applied by `matching_step`, which also reports the step.
        0,
        STEP_SECONDS,
        secret.to_vec(),
        Some(clean(issuer)),
        clean(account),
    )
}

/// What an authenticator app needs: the secret in base32, for typing in,
/// and an `otpauth://` URI, for a QR code.
pub fn provisioning(secret: &[u8], issuer: &str, account: &str) -> (String, String) {
    let totp = totp(secret, issuer, account);
    (totp.get_secret_base32(), totp.get_url())
}

/// The time step `code` is valid for at `now_seconds`, if any. The caller
/// must reject steps at or before the last one used, so a code works once.
pub fn matching_step(secret: &[u8], code: &str, now_seconds: u64) -> Option<u64> {
    let code = code
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    if code.len() != DIGITS || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let totp = totp(secret, "", "");
    let current = now_seconds / STEP_SECONDS;
    (current.saturating_sub(SKEW_STEPS)..=current + SKEW_STEPS).find(|step| {
        totp.generate(step * STEP_SECONDS)
            .as_bytes()
            .ct_eq(code.as_bytes())
            .into()
    })
}

/// New recovery codes, formatted `xxxxx-xxxxx` (50 bits each).
pub fn new_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let code = random_bytes::<RECOVERY_CODE_LENGTH>()
                .iter()
                .map(|byte| RECOVERY_ALPHABET[usize::from(*byte) % RECOVERY_ALPHABET.len()] as char)
                .collect::<String>();
            format!("{}-{}", &code[..5], &code[5..])
        })
        .collect()
}

/// The form recovery codes are hashed in: lowercase, without separators.
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The encryption context of a user's TOTP secret.
pub fn totp_context(user_id: &str) -> String {
    format!("totp:{user_id}")
}

#[derive(sqlx::FromRow)]
struct TotpRow {
    secret_encrypted: Vec<u8>,
    confirmed_at: Option<i64>,
    last_used_step: Option<i64>,
}

/// Whether the authenticator was set up and confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotpState {
    Pending,
    Confirmed,
}

/// Checks a code against the user's authenticator in `state`, and uses it
/// up: each code works once, and so do earlier ones.
pub async fn verify_totp(
    executor: &mut impl Executor,
    key: &InstanceKey,
    user_id: &str,
    state: TotpState,
    code: &str,
) -> anyhow::Result<bool> {
    let row: Option<TotpRow> = executor
        .fetch_optional(
            &Query::select()
                .columns([
                    UserTotp::SecretEncrypted,
                    UserTotp::ConfirmedAt,
                    UserTotp::LastUsedStep,
                ])
                .from(UserTotp::Table)
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    let Some(row) = row.filter(|row| row.confirmed_at.is_some() == (state == TotpState::Confirmed))
    else {
        return Ok(false);
    };
    let secret = key.decrypt(&totp_context(user_id), &row.secret_encrypted)?;
    let now_seconds = u64::try_from(now_ms() / 1000).unwrap_or_default();
    let Some(step) = matching_step(&secret, code, now_seconds) else {
        return Ok(false);
    };
    let step = i64::try_from(step).unwrap_or(i64::MAX);
    if row.last_used_step.is_some_and(|last| step <= last) {
        return Ok(false);
    }
    // Conditional, so two requests with the same code can't both pass.
    let updated = executor
        .execute(
            &Query::update()
                .table(UserTotp::Table)
                .value(UserTotp::LastUsedStep, step)
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .and_where(
                    Expr::col(UserTotp::LastUsedStep)
                        .is_null()
                        .or(Expr::col(UserTotp::LastUsedStep).lt(step)),
                )
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Uses up one of the user's recovery codes, if `code` is one.
pub async fn use_recovery_code(
    executor: &mut impl Executor,
    user_id: &str,
    code: &str,
) -> db::Result<bool> {
    let normalized = normalize_recovery_code(code);
    if normalized.len() != RECOVERY_CODE_LENGTH {
        return Ok(false);
    }
    let updated = executor
        .execute(
            &Query::update()
                .table(UserRecoveryCodes::Table)
                .value(UserRecoveryCodes::UsedAt, now_ms())
                .and_where(Expr::col(UserRecoveryCodes::UserId).eq(user_id))
                .and_where(Expr::col(UserRecoveryCodes::CodeHash).eq(token_hash(&normalized)))
                .and_where(Expr::col(UserRecoveryCodes::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    Ok(updated == 1)
}

/// Replaces the user's recovery codes with new ones and returns them. Only
/// their hashes are stored, so this is the one time they can be shown.
pub async fn replace_recovery_codes(
    executor: &mut impl Executor,
    user_id: &str,
) -> db::Result<Vec<String>> {
    executor
        .execute(
            &Query::delete()
                .from_table(UserRecoveryCodes::Table)
                .and_where(Expr::col(UserRecoveryCodes::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    let codes = new_recovery_codes();
    let now = now_ms();
    let mut insert = Query::insert();
    insert.into_table(UserRecoveryCodes::Table).columns([
        UserRecoveryCodes::Id,
        UserRecoveryCodes::UserId,
        UserRecoveryCodes::CodeHash,
        UserRecoveryCodes::CreatedAt,
    ]);
    for code in &codes {
        insert.values_panic([
            new_id().into(),
            user_id.into(),
            token_hash(&normalize_recovery_code(code)).into(),
            now.into(),
        ]);
    }
    executor.execute(&insert).await?;
    Ok(codes)
}

/// How many unused recovery codes the user has left.
pub async fn recovery_codes_remaining(
    executor: &mut impl Executor,
    user_id: &str,
) -> db::Result<i64> {
    let (count,): (i64,) = executor
        .fetch_one(
            &Query::select()
                .expr(Func::count(Expr::col(UserRecoveryCodes::Id)))
                .from(UserRecoveryCodes::Table)
                .and_where(Expr::col(UserRecoveryCodes::UserId).eq(user_id))
                .and_where(Expr::col(UserRecoveryCodes::UsedAt).is_null())
                .to_owned(),
        )
        .await?;
    Ok(count)
}

/// Stores a new, unconfirmed authenticator secret for the user, replacing
/// any earlier unconfirmed one.
pub async fn store_pending_totp(
    executor: &mut impl Executor,
    key: &InstanceKey,
    user_id: &str,
    secret: &[u8],
) -> db::Result<()> {
    executor
        .execute(
            &Query::delete()
                .from_table(UserTotp::Table)
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    executor
        .execute(
            &Query::insert()
                .into_table(UserTotp::Table)
                .columns([
                    UserTotp::UserId,
                    UserTotp::SecretEncrypted,
                    UserTotp::CreatedAt,
                ])
                .values_panic([
                    user_id.into(),
                    key.encrypt(&totp_context(user_id), secret).into(),
                    now_ms().into(),
                ])
                .to_owned(),
        )
        .await?;
    Ok(())
}

/// Marks the user's pending authenticator as confirmed.
pub async fn confirm_totp(executor: &mut impl Executor, user_id: &str) -> db::Result<()> {
    executor
        .execute(
            &Query::update()
                .table(UserTotp::Table)
                .value(UserTotp::ConfirmedAt, now_ms())
                .and_where(Expr::col(UserTotp::UserId).eq(user_id))
                .to_owned(),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B's SHA-1 secret, with its 8-digit codes cut to 6.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn codes_match_the_rfc_test_vectors() {
        assert_eq!(matching_step(RFC_SECRET, "287082", 59), Some(1));
        assert_eq!(
            matching_step(RFC_SECRET, "081804", 1_111_111_109),
            Some(37_037_036)
        );
        assert_eq!(
            matching_step(RFC_SECRET, "050471", 1_111_111_111),
            Some(37_037_037)
        );
        assert_eq!(matching_step(RFC_SECRET, "28 70 82", 59), Some(1));
    }

    #[test]
    fn neighbouring_steps_are_accepted_and_others_are_not() {
        // 287082 is step 1 (30-59s).
        assert_eq!(matching_step(RFC_SECRET, "287082", 89), Some(1));
        assert_eq!(matching_step(RFC_SECRET, "287082", 0), Some(1));
        assert_eq!(matching_step(RFC_SECRET, "287082", 90), None);
        assert_eq!(matching_step(RFC_SECRET, "28708", 59), None);
        assert_eq!(matching_step(RFC_SECRET, "28708a", 59), None);
    }

    #[test]
    fn provisioning_uses_base32_and_a_clean_otpauth_uri() {
        let (secret, uri) = provisioning(RFC_SECRET, "Acme: IT", "ada@example.com");
        assert_eq!(secret, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert!(
            uri.starts_with("otpauth://totp/Acme%20%20IT:ada%40example.com?"),
            "{uri}"
        );
        assert!(
            uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
            "{uri}"
        );
    }

    #[test]
    fn recovery_codes_are_distinct_and_normalize() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        assert_eq!(
            codes
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            RECOVERY_CODE_COUNT
        );
        for code in &codes {
            assert_eq!(code.len(), 11);
            assert_eq!(&code[5..6], "-");
        }
        assert_eq!(normalize_recovery_code(" AbCde-12345 "), "abcde12345");
    }
}

//! Table and column names for `sea-query`. Each enum's `Table` variant is the
//! snake-case table name; the other variants are its columns.
use sea_query::Iden;

/// The migration history table `sqlx` maintains.
#[derive(Iden)]
#[iden = "_sqlx_migrations"]
pub enum SqlxMigrations {
    Table,
    Version,
    Success,
}

#[derive(Iden)]
pub enum UserSessions {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum Invitations {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum PasswordResets {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum AgentInstallTokens {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum RemoteHandoffs {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum RemoteSessions {
    Table,
    ExpiresAt,
}

#[derive(Iden)]
pub enum ScriptRuns {
    Table,
    CreatedAt,
}

#[derive(Iden)]
pub enum FileDeliveries {
    Table,
    CreatedAt,
}

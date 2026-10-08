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
pub enum Settings {
    Table,
    Id,
    InstanceName,
    DashboardIdleTimeoutMinutes,
    BlackoutMessage,
    DisplayBorder,
    PreventIdleLock,
    AllowIdleOverride,
    SessionBanner,
    ConnectionNotification,
    BackgroundConnectionNotification,
    ConnectionNotificationMessage,
    IdleDisconnectMinutes,
    AllowIdleDisconnectOverride,
    ClearClipboardOnClose,
    AllowClearClipboardOverride,
    ConnectionApproval,
    ConnectionApprovalMessage,
    ConnectionApprovalTimeoutSeconds,
    ConnectionApprovalLockIdleSeconds,
    RequireTwoFactor,
    PasswordMinLength,
    SessionLifetimeHours,
    SmtpHost,
    SmtpPort,
    SmtpSecurity,
    SmtpUsername,
    SmtpPasswordEncrypted,
    SmtpFrom,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden)]
pub enum Users {
    Table,
    Id,
    Email,
    DisplayName,
    PasswordHash,
    PasswordChangedAt,
    Disabled,
    OidcSubject,
    ScimExternalId,
    ScimManaged,
    CreatedAt,
    UpdatedAt,
    LastSignInAt,
}

#[derive(Iden)]
pub enum UserTotp {
    Table,
    UserId,
    SecretEncrypted,
    ConfirmedAt,
    LastUsedStep,
    CreatedAt,
}

#[derive(Iden)]
pub enum UserRecoveryCodes {
    Table,
    Id,
    UserId,
    CodeHash,
    UsedAt,
    CreatedAt,
}

#[derive(Iden)]
pub enum UserPasskeys {
    Table,
    Id,
    UserId,
    CredentialId,
    Name,
    PasskeyJson,
    CreatedAt,
    LastUsedAt,
}

#[derive(Iden)]
pub enum UserSessions {
    Table,
    Id,
    TokenHash,
    UserId,
    AuthMethod,
    CreatedAt,
    LastSeenAt,
    ExpiresAt,
    VerifiedAt,
    Ip,
    UserAgent,
}

#[derive(Iden)]
pub enum Roles {
    Table,
    Id,
    Name,
    Description,
    Builtin,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum RolePermissions {
    Table,
    RoleId,
    Permission,
}

#[derive(Iden)]
pub enum UserRoles {
    Table,
    UserId,
    RoleId,
}

/// A view: the union of `user_roles` and the roles users hold through
/// identity provider groups.
#[derive(Iden)]
pub enum EffectiveUserRoles {
    Table,
    UserId,
    RoleId,
}

#[derive(Iden)]
pub enum Invitations {
    Table,
    Id,
    TokenHash,
    Email,
    CreatedByUserId,
    CreatedAt,
    ExpiresAt,
    AcceptedAt,
    RevokedAt,
}

#[derive(Iden)]
pub enum InvitationRoles {
    Table,
    InvitationId,
    RoleId,
}

#[derive(Iden)]
pub enum PasswordResets {
    Table,
    Id,
    TokenHash,
    UserId,
    CreatedByUserId,
    CreatedAt,
    ExpiresAt,
    UsedAt,
}

#[derive(Iden)]
pub enum OidcProvider {
    Table,
    Id,
    Enabled,
    DisplayName,
    IssuerUrl,
    ClientId,
    ClientSecretEncrypted,
    Scopes,
    AutoProvision,
    DefaultRoleId,
    RequireVerifiedEmail,
    GroupsClaim,
    UpdatedAt,
}

#[derive(Iden)]
pub enum OidcGroupRoles {
    Table,
    GroupName,
    RoleId,
}

#[derive(Iden)]
pub enum UserOidcGroups {
    Table,
    UserId,
    GroupName,
}

#[derive(Iden)]
pub enum ScimTokens {
    Table,
    Id,
    Name,
    TokenHash,
    CreatedByUserId,
    CreatedAt,
    LastUsedAt,
    RevokedAt,
}

#[derive(Iden)]
pub enum ScimGroups {
    Table,
    Id,
    DisplayName,
    ExternalId,
    RoleId,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum ScimGroupMembers {
    Table,
    GroupId,
    UserId,
}

#[derive(Iden)]
pub enum AuditEvents {
    Table,
    Id,
    ActorUserId,
    ActorLabel,
    Action,
    TargetType,
    TargetId,
    MetadataJson,
    Ip,
    CreatedAt,
}

#[derive(Iden)]
pub enum Agents {
    Table,
    Id,
    Name,
    AuthTokenHash,
    PendingAuthTokenHash,
    PendingAuthTokenEncrypted,
    CreatedByUserId,
    CreatedAt,
    UpdatedAt,
    DeletionRequestedAt,
}

#[derive(Iden)]
pub enum AgentInstallTokens {
    Table,
    Id,
    TokenHash,
    CreatedByUserId,
    Platform,
    CreatedAt,
    ExpiresAt,
    UsedAt,
    DeviceId,
    ComputerName,
    RedemptionKeyHash,
}

#[derive(Iden)]
pub enum RemoteHandoffs {
    Table,
    TokenHash,
    DeviceId,
    UserId,
    StartInBackground,
    Reason,
    CreatedAt,
    ExpiresAt,
    UsedAt,
}

#[derive(Iden)]
pub enum RemoteSessions {
    Table,
    Id,
    DeviceId,
    UserId,
    RecordEncrypted,
    CreatedAt,
    ExpiresAt,
}

#[derive(Iden)]
pub enum ToolboxScripts {
    Table,
    Id,
    OwnerUserId,
    Shared,
    Folder,
    Name,
    Description,
    Language,
    Body,
    TimeoutSeconds,
    CreatedAt,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden)]
pub enum ToolboxFiles {
    Table,
    Id,
    OwnerUserId,
    Shared,
    Folder,
    Name,
    SizeBytes,
    Sha256,
    CreatedAt,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden)]
pub enum ScriptRuns {
    Table,
    Id,
    DeviceId,
    ScriptId,
    ScriptName,
    Language,
    RequestedByUserId,
    Source,
    RunAs,
    TimeoutSeconds,
    Status,
    RanAs,
    ExitCode,
    Stdout,
    Stderr,
    OutputTruncated,
    Error,
    CreatedAt,
    CompletedAt,
}

#[derive(Iden)]
pub enum FileDeliveries {
    Table,
    Id,
    DeviceId,
    FileId,
    FileName,
    SizeBytes,
    RequestedByUserId,
    Destination,
    Status,
    Path,
    Error,
    CreatedAt,
    CompletedAt,
}

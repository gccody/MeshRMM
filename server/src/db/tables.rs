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
    UpdateScanIntervalHours,
    TerminalIdleTimeoutMinutes,
    ApiTokensEnabled,
    ApiTokenMaxLifetimeDays,
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
pub enum ApiTokens {
    Table,
    Id,
    UserId,
    Name,
    TokenHash,
    PermissionsJson,
    CreatedAt,
    ExpiresAt,
    LastUsedAt,
    LastUsedIp,
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
    ApiTokenId,
    ApiTokenName,
    CreatedAt,
}

#[derive(Iden)]
pub enum Companies {
    Table,
    Id,
    Name,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum Agents {
    Table,
    Id,
    Name,
    Platform,
    AuthTokenHash,
    PendingAuthTokenHash,
    PendingAuthTokenEncrypted,
    CreatedByUserId,
    CompanyId,
    CreatedAt,
    UpdatedAt,
    DeletionRequestedAt,
}

#[derive(Iden)]
pub enum Tags {
    Table,
    Id,
    Name,
    Color,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum DeviceTags {
    Table,
    DeviceId,
    TagId,
    CreatedAt,
}

#[derive(Iden)]
pub enum DeviceNotes {
    Table,
    DeviceId,
    Revision,
    Body,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden, Clone, Copy)]
pub enum DeviceMetrics {
    Table,
    DeviceId,
    Minute,
    Samples,
    CpuPercent,
    CpuPercentMax,
    MemoryUsedBytes,
    MemoryTotalBytes,
    NetworkReceivedBytesPerSecond,
    NetworkSentBytesPerSecond,
    StorageUsedBytes,
    StorageTotalBytes,
}

#[derive(Iden, Clone, Copy)]
pub enum DeviceSystemInfo {
    Table,
    DeviceId,
    CollectedAt,
    ReceivedAt,
    PublicIp,
    OsId,
    OsName,
    OsVersion,
    OsBuild,
    Architecture,
    Manufacturer,
    Model,
    SerialNumber,
    Hostname,
    ApplicationsHash,
    Details,
}

#[derive(Iden, Clone, Copy)]
pub enum DeviceIpAddresses {
    Table,
    DeviceId,
    Adapter,
    Address,
    PrefixLength,
    MacAddress,
    Kind,
}

#[derive(Iden, Clone, Copy)]
pub enum DeviceApplications {
    Table,
    DeviceId,
    Name,
    Version,
    Publisher,
    Architecture,
    InstalledAt,
    Source,
}

#[derive(Iden, Clone, Copy)]
pub enum DeviceApplicationChanges {
    Table,
    Id,
    DeviceId,
    ObservedAt,
    Change,
    Name,
    Version,
    PreviousVersion,
    Source,
}

#[derive(Iden)]
pub enum AgentInstallTokens {
    Table,
    Id,
    TokenHash,
    CreatedByUserId,
    Platform,
    CompanyId,
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
    JobId,
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
    JobId,
}

#[derive(Iden)]
pub enum NotificationChannels {
    Table,
    Id,
    Name,
    Kind,
    Enabled,
    WebhookUrl,
    WebhookSecretEncrypted,
    LastDeliveryAt,
    LastDeliveryError,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum NotificationChannelUsers {
    Table,
    ChannelId,
    UserId,
}

#[derive(Iden)]
pub enum NotificationChannelRoles {
    Table,
    ChannelId,
    RoleId,
}

#[derive(Iden)]
pub enum AlertRules {
    Table,
    Id,
    Name,
    Severity,
    Enabled,
    Condition,
    Threshold,
    ThresholdUnit,
    DurationMinutes,
    NonzeroExitFails,
    UpdateCategory,
    Days,
    Target,
    TagMatch,
    NotifyOnResolve,
    CreatedAt,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden)]
pub enum AlertRuleCompanies {
    Table,
    RuleId,
    CompanyId,
}

#[derive(Iden)]
pub enum AlertRuleTags {
    Table,
    RuleId,
    TagId,
}

#[derive(Iden)]
pub enum AlertRuleChannels {
    Table,
    RuleId,
    ChannelId,
}

#[derive(Iden, Clone, Copy)]
pub enum Alerts {
    Table,
    Id,
    RuleId,
    DeviceId,
    Subject,
    State,
    Value,
    Detail,
    ScriptRunId,
    UpdateRunId,
    Occurrences,
    RaisedAt,
    LastOccurredAt,
    AcknowledgedAt,
    AcknowledgedByUserId,
    AcknowledgedByLabel,
    ResolvedAt,
    ResolvedByUserId,
    ResolvedByLabel,
    Resolution,
}

#[derive(Iden)]
pub enum Jobs {
    Table,
    Id,
    Kind,
    ItemId,
    ItemName,
    Language,
    Body,
    TimeoutSeconds,
    RunAs,
    Destination,
    UpdateCategories,
    Reboot,
    RebootDelayMinutes,
    Target,
    TargetLabel,
    Offline,
    ExpiresAt,
    RequestedByUserId,
    PolicyId,
    PolicyName,
    CreatedAt,
    FinishedAt,
}

#[derive(Iden)]
pub enum JobDevices {
    Table,
    JobId,
    DeviceId,
    State,
    Detail,
    UpdatedAt,
}

#[derive(Iden)]
pub enum AutomationPolicies {
    Table,
    Id,
    Name,
    Enabled,
    Action,
    ScriptId,
    RunAs,
    UpdateCategories,
    Reboot,
    RebootDelayMinutes,
    Target,
    TagMatch,
    TriggerType,
    IntervalMinutes,
    TimeOfDay,
    Weekdays,
    RunAt,
    TimeZone,
    Offline,
    CatchUpHours,
    OwnerUserId,
    NextRunAt,
    IntervalAnchorAt,
    LastRunAt,
    LastJobId,
    LastProblem,
    LastProblemAt,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
pub enum AutomationPolicyCompanies {
    Table,
    PolicyId,
    CompanyId,
}

#[derive(Iden)]
pub enum AutomationPolicyDevices {
    Table,
    PolicyId,
    DeviceId,
}

#[derive(Iden)]
pub enum AutomationPolicyTags {
    Table,
    PolicyId,
    TagId,
}

#[derive(Iden)]
pub enum AutomationPolicyChannels {
    Table,
    PolicyId,
    ChannelId,
}

#[derive(Iden)]
pub enum DeviceUpdateStatus {
    Table,
    DeviceId,
    Manager,
    Supported,
    ScannedAt,
    ReceivedAt,
    ScanError,
    RebootPending,
    RebootPendingSince,
    ScanRequestedAt,
}

#[derive(Iden)]
pub enum DevicePendingUpdates {
    Table,
    DeviceId,
    UpdateId,
    Title,
    Kb,
    Version,
    CurrentVersion,
    Category,
    Severity,
    SizeBytes,
    RebootRequired,
    ReleasedAt,
    Source,
    FirstSeenAt,
}

#[derive(Iden)]
pub enum UpdateRuns {
    Table,
    Id,
    DeviceId,
    Kind,
    RequestedByUserId,
    PolicyName,
    JobId,
    UpdateIds,
    UpdateCategories,
    Reboot,
    RebootDelayMinutes,
    Status,
    Progress,
    ProgressPercent,
    Error,
    Installed,
    Failed,
    RebootRequired,
    RebootScheduledAt,
    RebootError,
    CreatedAt,
    CompletedAt,
}

#[derive(Iden)]
pub enum UpdateRunItems {
    Table,
    RunId,
    UpdateId,
    Title,
    Kb,
    Category,
    Outcome,
    Error,
}

#[derive(Iden)]
pub enum WakeRequests {
    Table,
    Id,
    DeviceId,
    RequestedByUserId,
    JobId,
    Status,
    Detail,
    MacAddresses,
    CreatedAt,
    OnlineAt,
}

#[derive(Iden)]
pub enum WakeRelays {
    Table,
    WakeId,
    RelayDeviceId,
    RelayName,
    Broadcast,
    PrefixLength,
    Status,
    SourceAddress,
    Error,
    ReportedAt,
}

//! The application tables, as sea-query names them. They live here rather
//! than in [`crate::db::tables`] with the rest, beside the code that uses
//! them.
use sea_query::Iden;

#[derive(Iden)]
pub enum Applications {
    Table,
    Id,
    Name,
    Description,
    PinnedVersion,
    CreatedAt,
    UpdatedAt,
    UpdatedByUserId,
}

#[derive(Iden)]
pub enum ApplicationSources {
    Table,
    ApplicationId,
    Position,
    Source,
    Platform,
    PackageId,
    FileId,
    InstallArguments,
    UninstallCommand,
    Detection,
}

#[derive(Iden)]
pub enum DeviceApplicationStatus {
    Table,
    DeviceId,
    Managers,
    ScannedAt,
    ReceivedAt,
}

#[derive(Iden)]
pub enum DevicePackages {
    Table,
    DeviceId,
    Source,
    PackageId,
    Name,
    Version,
    AvailableVersion,
    OutdatedSince,
}

#[derive(Iden)]
pub enum ApplicationRuns {
    Table,
    Id,
    DeviceId,
    Action,
    Summary,
    Request,
    RequestedByUserId,
    PolicyName,
    JobId,
    Status,
    Progress,
    Error,
    Succeeded,
    Failed,
    RebootRequired,
    CreatedAt,
    CompletedAt,
}

#[derive(Iden)]
pub enum ApplicationRunItems {
    Table,
    RunId,
    Position,
    Name,
    Source,
    PackageId,
    Outcome,
    Version,
    ExitCode,
    Error,
    Output,
    RebootRequired,
}

#[derive(Iden)]
pub enum ApplicationJobs {
    Table,
    JobId,
    Spec,
}

#[derive(Iden)]
pub enum ApplicationPolicies {
    Table,
    PolicyId,
    Spec,
}

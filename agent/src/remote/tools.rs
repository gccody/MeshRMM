//! The website's remote tools on this computer: its processes and services,
//! listed and changed as SYSTEM or root. See docs/remote-tools.md.
//!
//! The server asks over the control connection ([`ToolRequest`]) and the
//! Agent answers there ([`Tools::next`]), as for the file browser. Each
//! request runs on a blocking thread, at most [`Limits::concurrent`] at
//! once; one still waiting for its turn when the server's wait runs out
//! isn't started. A service manager that never answers can't be stopped:
//! once the server has given up on it, its request is left behind as a
//! stray that no longer holds a turn, and while [`Limits::strays`] are left
//! behind new requests are refused as busy.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    EndedProcess, ProcessFailure, ProcessInfo, ProcessList, ProcessTarget, ProcessesEnded,
    ServiceList, ToolError, ToolErrorCode, ToolOperation, ToolReply, ToolRequest,
};
use tokio::sync::{Semaphore, mpsc};

// Each operating system reads only its own tools' output.
#[cfg_attr(not(test), allow(dead_code))]
mod parse;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// How the tools keep to the time the server waits.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Requests that run at once; more wait their turn.
    concurrent: usize,
    /// Requests that never returned that are left behind at most.
    strays: usize,
    /// How long before the server gives up a request answers, so the
    /// answer arrives in time; at most a quarter of the wait.
    margin: Duration,
    /// How long past the server's wait a request that hasn't returned
    /// keeps its turn before it is left behind.
    grace: Duration,
}

const LIMITS: Limits = Limits {
    concurrent: 4,
    strays: 8,
    margin: Duration::from_secs(5),
    grace: Duration::from_secs(5),
};

/// How long apart two readings of the processes are taken when there is no
/// recent one to measure CPU use from.
const CPU_SAMPLE: Duration = Duration::from_millis(500);
/// A reading older than this isn't measured from.
const CPU_SAMPLE_STALE: Duration = Duration::from_secs(60);

/// When a request must answer by: a little before the server stops waiting
/// for it, counted from its arrival on this computer's monotonic clock.
#[derive(Debug, Clone, Copy)]
pub(super) struct Deadline {
    at: Instant,
}

impl Deadline {
    fn answering(arrived: Instant, wait: Duration, margin: Duration) -> Self {
        Self {
            at: arrived + wait.saturating_sub(margin.min(wait / 4)),
        }
    }

    pub(super) fn passed(&self) -> bool {
        Instant::now() >= self.at
    }

    /// The time left, none once it has passed.
    pub(super) fn left(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    #[cfg(test)]
    pub(super) fn distant() -> Self {
        Self {
            at: Instant::now() + Duration::from_secs(60 * 60),
        }
    }
}

/// A process as the platform read it, with the processor time it has used.
#[derive(Debug, Clone)]
pub(super) struct Reading {
    pub(super) info: ProcessInfo,
    pub(super) cpu_ms: u64,
}

/// One reading of every process.
#[derive(Debug, Clone, Default)]
pub(super) struct Snapshot {
    pub(super) processes: Vec<Reading>,
    pub(super) logical_cpus: u32,
    pub(super) memory_total_bytes: Option<u64>,
}

/// The previous reading of each process's processor time, to measure CPU
/// use from.
#[derive(Default)]
struct Sampler {
    previous: HashMap<(u32, Option<i64>), u64>,
    at: Option<Instant>,
}

impl Sampler {
    fn fresh(&self) -> bool {
        self.at.is_some_and(|at| at.elapsed() < CPU_SAMPLE_STALE)
    }

    /// The list from `snapshot`, with each process's CPU share since the
    /// previous reading, which `snapshot` then becomes.
    fn measure(&mut self, snapshot: Snapshot, now: Instant) -> ProcessList {
        let elapsed_ms = self
            .at
            .map(|at| now.saturating_duration_since(at).as_secs_f64() * 1000.0)
            .unwrap_or_default();
        let capacity = elapsed_ms * f64::from(snapshot.logical_cpus.max(1));
        let mut previous = HashMap::with_capacity(snapshot.processes.len());
        let processes = snapshot
            .processes
            .into_iter()
            .map(|Reading { mut info, cpu_ms }| {
                let key = (info.pid, info.started_at);
                if capacity > 0.0
                    && let Some(before) = self.previous.get(&key)
                {
                    info.cpu_percent = cpu_ms.saturating_sub(*before) as f64 * 100.0 / capacity;
                }
                previous.insert(key, cpu_ms);
                info
            })
            .collect();
        self.previous = previous;
        self.at = Some(now);
        let mut list = ProcessList {
            processes,
            read_at: now_ms(),
            logical_cpus: snapshot.logical_cpus.max(1),
            memory_total_bytes: snapshot.memory_total_bytes,
            truncated: false,
        };
        list.processes.sort_by_key(|process| process.pid);
        list.bound();
        list
    }
}

/// Runs one request; injected by tests.
type Operate = dyn Fn(ToolOperation, &Deadline) -> ToolReply + Send + Sync;

/// The Coordinator's remote tools: run the server's requests and queue
/// their answers. It outlives a connection, so an answer that arrives
/// after a reconnect is still sent; the server ignores answers it gave up
/// on.
pub(super) struct Tools {
    replies_tx: mpsc::UnboundedSender<(String, ToolReply)>,
    replies: mpsc::UnboundedReceiver<(String, ToolReply)>,
    limits: Limits,
    operate: Arc<Operate>,
    running: Arc<Semaphore>,
    strays: Arc<AtomicUsize>,
}

impl Tools {
    pub(super) fn new() -> Self {
        let sampler = Arc::new(Mutex::new(Sampler::default()));
        Self::with(
            LIMITS,
            Arc::new(move |operation, deadline: &Deadline| operate(&sampler, operation, deadline)),
        )
    }

    fn with(limits: Limits, operate: Arc<Operate>) -> Self {
        let (replies_tx, replies) = mpsc::unbounded_channel();
        Self {
            replies_tx,
            replies,
            limits,
            operate,
            running: Arc::new(Semaphore::new(limits.concurrent)),
            strays: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Runs `request` in the background; its answer comes from [`Self::next`].
    pub(super) fn request(&self, request: ToolRequest) {
        let arrived = Instant::now();
        let wait = Duration::from_millis(request.timeout_ms);
        let deadline = Deadline::answering(arrived, wait, self.limits.margin);
        let give_up = arrived + wait + self.limits.grace;
        let limits = self.limits;
        let operate = Arc::clone(&self.operate);
        let replies = self.replies_tx.clone();
        let running = Arc::clone(&self.running);
        let strays = Arc::clone(&self.strays);
        tokio::spawn(async move {
            let ToolRequest {
                request_id,
                operation,
                ..
            } = request;
            let name = operation.name();
            let answer = |reply: ToolReply| {
                if let ToolReply::Failed { error } = &reply {
                    tracing::info!(%request_id, operation = name, code = error.code.as_str(), message = error.message, "a tool request failed");
                }
                let _ = replies.send((request_id.clone(), reply));
            };
            if strays.load(Ordering::Relaxed) >= limits.strays {
                return answer(busy(limits.strays));
            }
            let permit = match tokio::time::timeout_at(
                deadline.at.into(),
                Arc::clone(&running).acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return,
                Err(_) => return answer(not_started()),
            };
            if deadline.passed() {
                return answer(not_started());
            }
            if strays.load(Ordering::Relaxed) >= limits.strays {
                return answer(busy(limits.strays));
            }
            let mut work = tokio::task::spawn_blocking(move || operate(operation, &deadline));
            let finished = tokio::select! {
                finished = &mut work => finished,
                () = tokio::time::sleep_until(give_up.into()) => {
                    let left_behind = strays
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                            (count < limits.strays).then_some(count + 1)
                        })
                        .is_ok();
                    if left_behind {
                        drop(permit);
                    }
                    tracing::warn!(%request_id, operation = name, left_behind, "a tool request didn't return in the time the server waits");
                    let late = work.await;
                    if left_behind {
                        strays.fetch_sub(1, Ordering::Relaxed);
                    }
                    tracing::info!(%request_id, operation = name, finished = late.is_ok(), "a tool request the Agent gave up on returned; its answer is dropped");
                    return;
                }
            };
            drop(permit);
            answer(finished.unwrap_or_else(|error| {
                tracing::warn!(%error, operation = name, "a tool request's task failed");
                ToolReply::failed(ToolErrorCode::Failed, "The Agent could not finish it.")
            }));
        });
    }

    #[cfg(test)]
    fn strays(&self) -> usize {
        self.strays.load(Ordering::Relaxed)
    }

    /// The next answer to send.
    pub(super) async fn next(&mut self) -> (String, ToolReply) {
        loop {
            if let Some(reply) = self.replies.recv().await {
                return reply;
            }
        }
    }
}

fn not_started() -> ToolReply {
    ToolReply::failed(
        ToolErrorCode::TimedOut,
        "The device was busy with other tool requests and didn't start this one in time, so nothing was changed. Try again.",
    )
}

fn busy(strays: usize) -> ToolReply {
    ToolReply::failed(
        ToolErrorCode::Busy,
        format!(
            "{strays} earlier tool requests on this device never finished (its service manager may have stopped answering), so the Agent isn't starting more until they do. Restarting the MeshRMM Agent ends them."
        ),
    )
}

/// Runs one request, answering by `deadline`.
fn operate(sampler: &Mutex<Sampler>, operation: ToolOperation, deadline: &Deadline) -> ToolReply {
    let result = match operation {
        ToolOperation::ListProcesses => list_processes(sampler, deadline).map(ToolReply::Processes),
        ToolOperation::EndProcesses { processes } => {
            end_processes(&processes, deadline).map(ToolReply::ProcessesEnded)
        }
        ToolOperation::ListServices => platform::list_services().map(|services| {
            let mut list = ServiceList {
                services,
                read_at: now_ms(),
                truncated: false,
            };
            list.services
                .sort_by_cached_key(|service| service.display_name.to_lowercase());
            list.bound();
            ToolReply::Services(list)
        }),
        ToolOperation::ControlService { name, action } => {
            platform::control_service(&name, action, deadline).map(ToolReply::Service)
        }
        ToolOperation::SetServiceStartup { name, startup } => {
            platform::set_service_startup(&name, startup, deadline).map(ToolReply::Service)
        }
    };
    result.unwrap_or_else(|error| ToolReply::Failed {
        error: error.sanitized(),
    })
}

fn lock(sampler: &Mutex<Sampler>) -> std::sync::MutexGuard<'_, Sampler> {
    sampler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every process, with its CPU share since the previous reading, or over
/// [`CPU_SAMPLE`] when there is no recent one.
fn list_processes(sampler: &Mutex<Sampler>, deadline: &Deadline) -> Result<ProcessList, ToolError> {
    if !lock(sampler).fresh() {
        let first = platform::read_processes()?;
        let now = Instant::now();
        lock(sampler).measure(first, now);
        std::thread::sleep(CPU_SAMPLE.min(deadline.left() / 2));
    }
    let snapshot = platform::read_processes()?;
    Ok(lock(sampler).measure(snapshot, Instant::now()))
}

/// How many of `pid`'s ancestors `processes` holds, following each parent
/// only while it started no later than its child, so a reused parent ID
/// ends the chain.
fn depth(processes: &HashMap<u32, &ProcessInfo>, pid: u32) -> usize {
    let mut seen = HashSet::from([pid]);
    let mut current = processes.get(&pid).copied();
    while let Some(child) = current {
        current = child
            .parent_pid
            .and_then(|parent| processes.get(&parent).copied())
            .filter(|parent| {
                parent
                    .started_at
                    .zip(child.started_at)
                    .is_none_or(|(parent, child)| parent <= child)
            })
            .filter(|parent| seen.insert(parent.pid));
    }
    seen.len() - 1
}

/// The order to end `targets` in: deepest in the tree first, so children
/// go before their parents.
fn ending_order<'a>(
    processes: &HashMap<u32, &ProcessInfo>,
    targets: impl IntoIterator<Item = &'a ProcessInfo>,
) -> Vec<&'a ProcessInfo> {
    let mut ordered = targets.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|process| {
        (
            std::cmp::Reverse(depth(processes, process.pid)),
            process.pid,
        )
    });
    ordered
}

fn failure(pid: u32, name: Option<&str>, code: ToolErrorCode, message: String) -> ProcessFailure {
    ProcessFailure {
        pid,
        name: name.map(str::to_owned),
        error: ToolError::new(code, message),
    }
}

/// Ends each of `targets` that is still the process the list showed,
/// children first, never the Agent itself or what the system can't do
/// without.
fn end_processes(
    targets: &[ProcessTarget],
    deadline: &Deadline,
) -> Result<ProcessesEnded, ToolError> {
    let snapshot = platform::read_processes()?;
    let infos = snapshot
        .processes
        .iter()
        .map(|reading| &reading.info)
        .collect::<Vec<_>>();
    let by_pid = infos
        .iter()
        .map(|info| (info.pid, *info))
        .collect::<HashMap<_, _>>();
    let mut outcome = ProcessesEnded::default();
    let mut chosen = Vec::new();
    for target in targets {
        let Some(info) = by_pid.get(&target.pid) else {
            outcome.failed.push(failure(
                target.pid,
                None,
                ToolErrorCode::NotFound,
                format!("Process {} isn't running.", target.pid),
            ));
            continue;
        };
        if target.started_at.is_some() && info.started_at != target.started_at {
            outcome.failed.push(failure(
                target.pid,
                Some(&info.name),
                ToolErrorCode::Changed,
                format!(
                    "Process {} is now {}, not the one listed; refresh the list.",
                    target.pid, info.name
                ),
            ));
            continue;
        }
        if target.pid == std::process::id() || platform::is_protected(info) {
            outcome.failed.push(failure(
                target.pid,
                Some(&info.name),
                ToolErrorCode::Protected,
                format!(
                    "{} is part of the MeshRMM Agent or of the operating system, so it isn't ended.",
                    info.name
                ),
            ));
            continue;
        }
        chosen.push(*info);
    }
    let ordered = ending_order(&by_pid, chosen);
    let results = platform::end_processes(&ordered, deadline);
    for (info, result) in ordered.into_iter().zip(results) {
        match result {
            Ok(()) => outcome.ended.push(EndedProcess {
                pid: info.pid,
                name: info.name.clone(),
            }),
            Err(error) => outcome.failed.push(ProcessFailure {
                pid: info.pid,
                name: Some(info.name.clone()),
                error,
            }),
        }
    }
    Ok(outcome)
}

/// The error for a process the deadline stopped before it was ended.
pub(super) fn out_of_time(info: &ProcessInfo) -> ToolError {
    ToolError::new(
        ToolErrorCode::TimedOut,
        format!("{} wasn't ended before the time ran out.", info.name),
    )
}

/// `time` in milliseconds since the Unix epoch.
pub(super) fn unix_ms(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).ok(),
        Err(_) => None,
    }
}

fn now_ms() -> i64 {
    unix_ms(SystemTime::now()).unwrap_or_default()
}

/// A failure of the service manager, as `what` says.
pub(super) fn failed(what: impl std::fmt::Display) -> ToolError {
    ToolError::new(ToolErrorCode::Failed, what.to_string())
}

/// The refusal to stop or change the Agent's own service.
pub(super) fn own_service() -> ToolError {
    ToolError::new(
        ToolErrorCode::Protected,
        "That is the MeshRMM Agent's own service, which the remote tools don't stop or change: it would cut the device off.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, parent: Option<u32>, started_at: i64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            name: format!("p{pid}"),
            path: None,
            user: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            started_at: Some(started_at),
            command_line: None,
            command_line_truncated: false,
            session: None,
        }
    }

    #[test]
    fn children_end_before_their_parents() {
        let all = [
            info(1, None, 0),
            info(10, Some(1), 5),
            info(11, Some(10), 6),
            info(12, Some(11), 7),
            // A parent ID the system has since given a younger process.
            info(20, Some(30), 1),
            info(30, Some(1), 9),
        ];
        let by_pid = all
            .iter()
            .map(|info| (info.pid, info))
            .collect::<HashMap<_, _>>();
        let order = ending_order(&by_pid, [&all[1], &all[3], &all[2], &all[4], &all[5]])
            .iter()
            .map(|info| info.pid)
            .collect::<Vec<_>>();
        // 30 is 1's child; 20's parent ID now names a younger process, so
        // 20 is a root of its own.
        assert_eq!(order, [12, 11, 10, 30, 20]);
        assert_eq!(depth(&by_pid, 20), 0);
        // A loop of parents ends.
        let looped = [info(5, Some(6), 0), info(6, Some(5), 0)];
        let by_pid = looped
            .iter()
            .map(|info| (info.pid, info))
            .collect::<HashMap<_, _>>();
        assert_eq!(depth(&by_pid, 5), 1);
    }

    #[test]
    fn cpu_use_is_measured_between_readings() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        let reading = |pid, cpu_ms| Reading {
            info: info(pid, None, 1),
            cpu_ms,
        };
        let snapshot = |processes| Snapshot {
            processes,
            logical_cpus: 4,
            memory_total_bytes: Some(8),
        };
        let first = sampler.measure(snapshot(vec![reading(1, 100), reading(2, 0)]), start);
        assert!(
            first
                .processes
                .iter()
                .all(|process| process.cpu_percent == 0.0)
        );
        assert!(sampler.fresh());
        // One second later: 2 s of four processors' 4 s is 50%.
        let second = sampler.measure(
            snapshot(vec![reading(1, 2100), reading(3, 50)]),
            start + Duration::from_secs(1),
        );
        assert_eq!(second.processes[0].cpu_percent, 50.0);
        // A process new since the last reading has no share yet.
        assert_eq!(second.processes[1].cpu_percent, 0.0);
        assert_eq!(second.logical_cpus, 4);
    }

    #[test]
    fn requests_answer_a_little_before_the_server_gives_up() {
        let now = Instant::now();
        let by = |wait| Deadline::answering(now, wait, LIMITS.margin).at - now;
        assert_eq!(by(Duration::from_secs(30)), Duration::from_secs(25));
        assert_eq!(by(Duration::from_secs(4)), Duration::from_secs(3));
    }

    fn request(id: &str, timeout: Duration) -> ToolRequest {
        ToolRequest {
            request_id: id.into(),
            timeout_ms: timeout.as_millis() as u64,
            operation: ToolOperation::ListServices,
        }
    }

    const BOUND: Duration = Duration::from_secs(60);

    async fn answered(tools: &mut Tools) -> (String, ToolReply) {
        tokio::time::timeout(BOUND, tools.next())
            .await
            .expect("no answer came")
    }

    fn code(reply: &ToolReply) -> Option<ToolErrorCode> {
        match reply {
            ToolReply::Failed { error } => Some(error.code),
            _ => None,
        }
    }

    /// Requests whose ID starts with `hung` wait until `release` is set.
    fn hanging(release: Arc<(Mutex<bool>, std::sync::Condvar)>) -> Arc<Operate> {
        Arc::new(move |_, _: &Deadline| {
            let (released, signal) = &*release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = signal.wait(released).unwrap();
            }
            ToolReply::failed(ToolErrorCode::Failed, "done")
        })
    }

    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);

    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }

    #[tokio::test]
    async fn requests_wait_their_turn_and_are_refused_while_too_many_are_stuck() {
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let _release = Release(Arc::clone(&release));
        let limits = Limits {
            concurrent: 1,
            strays: 1,
            margin: Duration::ZERO,
            grace: Duration::ZERO,
        };
        let mut tools = Tools::with(limits, hanging(Arc::clone(&release)));
        tools.request(request("stuck", Duration::from_millis(300)));
        // It holds the only turn, so this one's time runs out waiting.
        tools.request(request("late", Duration::from_millis(100)));
        let (id, reply) = answered(&mut tools).await;
        assert_eq!(id, "late");
        assert_eq!(code(&reply), Some(ToolErrorCode::TimedOut));
        // Left behind once the server has given up, it frees its turn, but
        // the Agent is now busy.
        let failing = Instant::now() + BOUND;
        while tools.strays() != 1 {
            assert!(Instant::now() < failing);
            tokio::task::yield_now().await;
        }
        tools.request(request("refused", BOUND));
        let (id, reply) = answered(&mut tools).await;
        assert_eq!(id, "refused");
        assert_eq!(code(&reply), Some(ToolErrorCode::Busy));
        drop(_release);
        while tools.strays() != 0 {
            assert!(Instant::now() < failing);
            tokio::task::yield_now().await;
        }
        // Its late answer goes nowhere.
        assert!(tools.replies.try_recv().is_err());
    }

    /// This computer's processes: this one among them, with its parent and
    /// start time, and a CPU share in range.
    #[test]
    fn this_computer_lists_its_processes() {
        let sampler = Mutex::new(Sampler::default());
        let list = list_processes(&sampler, &Deadline::distant()).unwrap();
        let me = list
            .processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .expect("this process is listed");
        assert!(me.started_at.is_some(), "{me:?}");
        assert!(me.memory_bytes > 0, "{me:?}");
        assert!(me.parent_pid.is_some(), "{me:?}");
        assert!(me.user.is_some(), "{me:?}");
        assert!(me.command_line.is_some(), "{me:?}");
        assert!(list.processes.len() > 3);
        assert!(
            list.processes
                .iter()
                .all(|process| (0.0..=100.0).contains(&process.cpu_percent))
        );
        assert!(list.logical_cpus >= 1);
    }

    /// Ends a child this test starts, and refuses this process and one
    /// whose start time doesn't match.
    #[test]
    fn a_child_process_is_ended_and_the_agent_is_not() {
        #[cfg(windows)]
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 120"])
            .spawn()
            .unwrap();
        #[cfg(unix)]
        let mut child = std::process::Command::new("sleep")
            .arg("120")
            .spawn()
            .unwrap();
        let pid = child.id();
        let started_at = loop {
            let snapshot = platform::read_processes().unwrap();
            if let Some(found) = snapshot
                .processes
                .iter()
                .find(|reading| reading.info.pid == pid)
            {
                break found.info.started_at;
            }
        };
        let deadline = Deadline::distant();
        let wrong = end_processes(
            &[ProcessTarget {
                pid,
                started_at: started_at.map(|at| at - 5000),
            }],
            &deadline,
        )
        .unwrap();
        assert_eq!(
            wrong.failed[0].error.code,
            ToolErrorCode::Changed,
            "{wrong:?}"
        );
        let me = end_processes(
            &[ProcessTarget {
                pid: std::process::id(),
                started_at: None,
            }],
            &deadline,
        )
        .unwrap();
        assert_eq!(me.failed[0].error.code, ToolErrorCode::Protected);
        let ended = end_processes(&[ProcessTarget { pid, started_at }], &deadline).unwrap();
        assert_eq!(ended.ended.len(), 1, "{ended:?}");
        assert_eq!(ended.ended[0].pid, pid);
        child.wait().unwrap();
        let gone = end_processes(&[ProcessTarget { pid, started_at }], &deadline).unwrap();
        assert!(
            matches!(
                gone.failed[0].error.code,
                ToolErrorCode::NotFound | ToolErrorCode::Changed
            ),
            "{gone:?}"
        );
    }

    /// This computer's services, read through its service manager.
    #[test]
    #[cfg_attr(target_os = "linux", ignore = "needs systemd")]
    fn this_computer_lists_its_services() {
        let services = platform::list_services().unwrap();
        assert!(services.len() > 3, "{services:?}");
        assert!(
            services
                .iter()
                .any(|service| service.state == meshrmm_protocol::ServiceState::Running)
        );
        #[cfg(windows)]
        {
            let rpc = services
                .iter()
                .find(|service| service.name.eq_ignore_ascii_case("RpcSs"))
                .expect("RpcSs is listed");
            assert_eq!(rpc.state, meshrmm_protocol::ServiceState::Running);
            assert!(
                rpc.pid.is_some() && rpc.account.is_some() && rpc.description.is_some(),
                "{rpc:?}"
            );
            assert_eq!(
                rpc.startup,
                Some(meshrmm_protocol::ServiceStartup::Automatic)
            );
        }
    }
}

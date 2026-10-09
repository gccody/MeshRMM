//! The connection approval prompt's helper.
use super::*;

/// The connection approval prompt, in its own LocalSystem helper on the
/// console's desktop: the signed-in user's, or the sign-in screen when nobody
/// is signed in. Dropping it closes the prompt.
pub struct ApprovalHelper {
    process: OwnedHandle,
    input: InputWriter,
    answer: tokio::sync::oneshot::Receiver<Result<Decision, String>>,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

impl ApprovalHelper {
    pub fn start(prompt: &ApprovalPrompt) -> anyhow::Result<Self> {
        let target = preferred_desktop();
        let launched = launch_system_helper(target)?;
        let (answer_tx, answer) = tokio::sync::oneshot::channel();
        let reader = thread::Builder::new()
            .name("meshrmm-approval-ipc".into())
            .spawn(move || read_approval_events(launched.output, answer_tx))
            .context("failed to start the approval-helper IPC reader")?;
        let stderr = thread::Builder::new()
            .name("meshrmm-approval-stderr".into())
            .spawn(move || drain_child_stderr(launched.stderr))
            .context("failed to start the approval-helper error reader")?;
        let helper = Self {
            process: launched.process,
            input: Arc::new(CommandWriter::new(launched.input)?),
            answer,
            reader: Some(reader),
            stderr: Some(stderr),
        };
        send_command(
            &helper.input,
            &ParentCommand::PromptConnectionApproval {
                text: prompt.text.clone(),
                reason: prompt.reason.clone(),
                timeout_seconds: prompt.timeout.as_secs().try_into().unwrap_or(u32::MAX),
                lock_idle_seconds: prompt.lock_idle.as_secs().try_into().unwrap_or(u32::MAX),
            },
        )
        .context("failed to ask the approval helper")?;
        tracing::info!(
            process_id = launched.process_id,
            session_id = launched.session_id,
            desktop = target.name(),
            "connection approval helper started"
        );
        Ok(helper)
    }

    /// The prompt's answer, once the user or the policy gives it.
    pub async fn answer(&mut self) -> anyhow::Result<Decision> {
        match (&mut self.answer).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(message)) => Err(anyhow::anyhow!(message)),
            Err(_) => anyhow::bail!("the approval helper exited without an answer"),
        }
    }
}

impl Drop for ApprovalHelper {
    fn drop(&mut self) {
        let _ = send_command(&self.input, &ParentCommand::Stop);
        if unsafe { WaitForSingleObject(self.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
            terminate_and_wait(&self.process);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

/// Passes on the approval helper's answer. The helper runs as LocalSystem but
/// shows a window to the user, so any other event ends it.
fn read_approval_events(
    output: File,
    answer: tokio::sync::oneshot::Sender<Result<Decision, String>>,
) {
    let mut output = BufReader::new(output);
    let mut answer = Some(answer);
    let result = loop {
        match read_event(&mut output) {
            Ok(ChildEvent::InputStarted) => {}
            Ok(ChildEvent::ApprovalDecision(decision)) => {
                if let Some(answer) = answer.take() {
                    let _ = answer.send(Ok(decision));
                }
            }
            Ok(ChildEvent::Stopped) => break Ok(()),
            Ok(ChildEvent::Error(message)) => break Err(message),
            Ok(event) => {
                break Err(format!(
                    "approval helper sent an unexpected {} event",
                    child_event_name(&event)
                ));
            }
            Err(error) => break Err(format!("approval-helper IPC failed: {error}")),
        }
    };
    if let (Some(answer), Err(message)) = (answer, result) {
        let _ = answer.send(Err(message));
    }
}

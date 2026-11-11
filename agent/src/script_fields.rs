//! Custom fields for toolbox scripts. See docs/custom-fields.md.
//!
//! A run's request carries the device's fields the script may read, with
//! their values, which the script gets as `MESHRMM_FIELD_<KEY>` environment
//! variables. When the script may also write some, the Agent gives the run
//! a token for this local channel, a TCP listener on 127.0.0.1 the
//! coordinator opens once, and the script's `meshrmm-agent field set <key>
//! <value>` sends the token, key and value over it. The Agent checks the
//! token against the runs it is running, refuses a field the run may not
//! write, and forwards the value to the server as that run, with its own
//! credential; the server checks again and records the value as the
//! script's. The token is random, works only while its script runs, and is
//! given only to the script, through its environment (Windows) or its
//! private script file (macOS and Linux).
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meshrmm_protocol::{MAX_SCRIPT_FIELD_VALUE_BYTES, ScriptRunRequest, field_variable};
use serde::{Deserialize, Serialize};

/// The local channel's address, as `127.0.0.1:<port>`.
pub(crate) const ENDPOINT_VARIABLE: &str = "MESHRMM_FIELDS_ENDPOINT";
/// The run's token for the local channel.
pub(crate) const TOKEN_VARIABLE: &str = "MESHRMM_FIELDS_TOKEN";
/// The Agent's executable, which scripts run as `meshrmm-agent field …`.
pub(crate) const AGENT_VARIABLE: &str = "MESHRMM_AGENT";
pub(crate) const RUN_VARIABLE: &str = "MESHRMM_RUN_ID";
/// A request: the token, key and value, quoted as JSON.
const MAX_REQUEST_BYTES: usize = MAX_SCRIPT_FIELD_VALUE_BYTES * 6 + 1024;
/// Connections handled at once; more are closed at once.
const MAX_CONNECTIONS: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the CLI waits for the Agent, which waits for the server.
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);

/// What `field set` sends the Agent, as one line of JSON.
#[derive(Debug, Serialize, Deserialize)]
struct Request {
    token: String,
    key: String,
    value: String,
}

/// The Agent's answer, as one line of JSON.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Sends a script's value to the server as its run.
pub(crate) trait Forward: Send + Sync {
    /// `Err` holds why the server refused it, for the script.
    fn set(&self, run_id: &str, key: &str, value: &str) -> Result<(), String>;
}

/// A run the channel takes values from.
struct Run {
    run_id: String,
    writable: HashSet<String>,
}

/// The local channel, open for the Agent's life.
pub(crate) struct Channel {
    address: SocketAddr,
    runs: Mutex<HashMap<String, Run>>,
    forward: Box<dyn Forward>,
    connections: AtomicUsize,
}

impl Channel {
    /// Opens the channel on a free loopback port.
    pub(crate) fn start(forward: impl Forward + 'static) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let channel = Arc::new(Self {
            address: listener.local_addr()?,
            runs: Mutex::new(HashMap::new()),
            forward: Box::new(forward),
            connections: AtomicUsize::new(0),
        });
        let accepting = Arc::clone(&channel);
        std::thread::Builder::new()
            .name("meshrmm-script-fields".into())
            .spawn(move || accepting.accept(listener))?;
        Ok(channel)
    }

    /// A token for `run`'s script, if it may write any field. The token
    /// works until the grant is dropped.
    pub(crate) fn grant(self: &Arc<Self>, run: &ScriptRunRequest) -> Option<Grant> {
        let writable = run
            .fields
            .iter()
            .filter(|field| field.writable)
            .map(|field| field.key.clone())
            .collect::<HashSet<_>>();
        if writable.is_empty() {
            return None;
        }
        let mut bytes = [0_u8; 32];
        if let Err(error) = getrandom::fill(&mut bytes) {
            tracing::warn!(%error, "could not make a token for a script's custom fields");
            return None;
        }
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        self.lock().insert(
            String::clone(&token),
            Run {
                run_id: run.run_id.clone(),
                writable,
            },
        );
        Some(Grant {
            token,
            channel: Arc::clone(self),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Run>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn accept(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let taken =
                self.connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                        (open < MAX_CONNECTIONS).then_some(open + 1)
                    });
            if taken.is_err() {
                continue;
            }
            let channel = Arc::clone(&self);
            let spawned = std::thread::Builder::new()
                .name("meshrmm-script-field".into())
                .spawn(move || {
                    channel.serve(stream);
                    channel.connections.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                self.connections.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn serve(&self, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
        let reply = match read_line(&stream) {
            Ok(line) => self.handle(&line),
            Err(error) => Reply { error: Some(error) },
        };
        let mut stream = stream;
        if let Ok(mut line) = serde_json::to_vec(&reply) {
            line.push(b'\n');
            let _ = stream.write_all(&line);
        }
    }

    fn handle(&self, line: &str) -> Reply {
        let Ok(request) = serde_json::from_str::<Request>(line) else {
            return refused("the request is not one meshrmm-agent field sends");
        };
        // Compared in full, so how long it takes says nothing of the token.
        let run = self
            .lock()
            .iter()
            .find(|(token, _)| same(token, &request.token))
            .map(|(_, run)| (run.run_id.clone(), run.writable.contains(&request.key)));
        let Some((run_id, writable)) = run else {
            return refused("the script that may set custom fields has finished");
        };
        if !writable {
            return refused(&format!(
                "scripts may not change the custom field “{}”, or the device has none",
                request.key
            ));
        }
        match self.forward.set(&run_id, &request.key, &request.value) {
            Ok(()) => {
                tracing::info!(
                    run_id,
                    key = request.key,
                    "a toolbox script set a custom field"
                );
                Reply::default()
            }
            Err(error) => {
                tracing::info!(
                    run_id,
                    key = request.key,
                    error,
                    "the server refused a script's custom field"
                );
                refused(&error)
            }
        }
    }
}

fn refused(error: &str) -> Reply {
    Reply {
        error: Some(error.to_owned()),
    }
}

fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn read_line(stream: &TcpStream) -> Result<String, String> {
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES as u64 + 1))
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("could not read the request: {error}"))?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err("the value is too large".to_owned());
    }
    String::from_utf8(line).map_err(|_| "the request is not UTF-8".to_owned())
}

/// A run's token, taken back when dropped, as the run ends.
pub(crate) struct Grant {
    token: String,
    channel: Arc<Channel>,
}

impl Grant {
    pub(crate) fn endpoint(&self) -> String {
        self.channel.address.to_string()
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.channel.lock().remove(&self.token);
    }
}

/// The environment variables a run's script gets: each readable field's
/// value, the run's ID, the Agent's executable, and the local channel and
/// token when it may write fields.
pub(crate) fn variables(run: &ScriptRunRequest, grant: Option<&Grant>) -> Vec<(String, String)> {
    let mut variables = run
        .fields
        .iter()
        .filter(|field| meshrmm_protocol::valid_field_key(&field.key))
        .filter_map(|field| {
            let value = field.value.as_ref()?;
            (!value.contains('\0')).then(|| (field_variable(&field.key), value.clone()))
        })
        .collect::<Vec<_>>();
    variables.push((RUN_VARIABLE.to_owned(), run.run_id.clone()));
    if let Ok(executable) = std::env::current_exe() {
        variables.push((AGENT_VARIABLE.to_owned(), executable.display().to_string()));
    }
    if let Some(grant) = grant {
        variables.push((ENDPOINT_VARIABLE.to_owned(), grant.endpoint()));
        variables.push((TOKEN_VARIABLE.to_owned(), grant.token.clone()));
    }
    variables
}

/// The folder of the Agent's executable, which a script's `PATH` ends with
/// so it can run `meshrmm-agent` by name.
pub(crate) fn agent_folder() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

/// Forwards values to the server over HTTPS with the Agent's credential,
/// which the coordinator replaces when it rotates.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) struct Https {
    server: String,
    device_id: String,
    credential: Arc<Mutex<String>>,
    http: ureq::Agent,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Https {
    pub(crate) fn new(
        config: &crate::remote::config::Config,
        credential: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            server: config.server.clone(),
            device_id: config.device_id.clone(),
            credential,
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .http_status_as_error(false)
                .tls_config(crate::enrollment::https_tls_config())
                .build()
                .new_agent(),
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
impl Forward for Https {
    fn set(&self, run_id: &str, key: &str, value: &str) -> Result<(), String> {
        let url = meshrmm_signaling_client::endpoint_url(
            &self.server,
            &[
                "v1",
                "agents",
                &self.device_id,
                "script-runs",
                run_id,
                "custom-fields",
            ],
            &[],
            false,
        )
        .map_err(|error| format!("{error:#}"))?;
        let body = serde_json::to_vec(&meshrmm_protocol::ScriptFieldWrite {
            key: key.to_owned(),
            value: value.to_owned(),
        })
        .map_err(|error| error.to_string())?;
        let token = self
            .credential
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let response = self
            .http
            .post(url.as_str())
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(&body[..])
            .map_err(|error| format!("could not reach the server: {error}"))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let message = response
            .into_body()
            .read_to_string()
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|error| error["error"].as_str().map(str::to_owned));
        Err(message.unwrap_or_else(|| format!("the server answered {status}")))
    }
}

const USAGE: &str = "usage:
  meshrmm-agent field get <key>
  meshrmm-agent field set <key> <value>...   (a multi-select's options as more values)
  meshrmm-agent field set <key> --stdin      (the value from standard input)
  meshrmm-agent field clear <key>

Run by a toolbox script on a MeshRMM device. A field's key is shown on the
website's Custom fields page; the script reads it as $MESHRMM_FIELD_<KEY>.";

/// `meshrmm-agent field …`, from a running script. `None` when the
/// arguments aren't this command; otherwise the exit code.
pub(crate) fn command(arguments: &[String]) -> Option<i32> {
    if arguments.first().map(String::as_str) != Some("field") {
        return None;
    }
    let environment = |name: &str| std::env::var(name).ok();
    let mut stdin = || {
        let mut text = String::new();
        std::io::stdin()
            .take(MAX_SCRIPT_FIELD_VALUE_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|error| format!("could not read standard input: {error}"))
    };
    Some(
        match run_command(&arguments[1..], environment, &mut stdin, send) {
            Ok(output) => {
                if let Some(output) = output {
                    println!("{output}");
                }
                0
            }
            Err((code, message)) => {
                eprintln!("meshrmm-agent field: {message}");
                code
            }
        },
    )
}

type Outcome = Result<Option<String>, (i32, String)>;

fn run_command(
    arguments: &[String],
    environment: impl Fn(&str) -> Option<String>,
    stdin: &mut dyn FnMut() -> Result<String, String>,
    send: impl FnOnce(&str, &Request) -> Result<(), String>,
) -> Outcome {
    let usage = || Err((2, USAGE.to_owned()));
    let [action, key, values @ ..] = arguments else {
        return usage();
    };
    if !meshrmm_protocol::valid_field_key(key) {
        return Err((2, format!("“{key}” is not a field key")));
    }
    let value = match (action.as_str(), values) {
        ("get", []) => {
            return environment(&field_variable(key)).map(Some).ok_or_else(|| {
                (
                    1,
                    format!("the script can't read “{key}”, or it has no value"),
                )
            });
        }
        ("clear", []) => String::new(),
        ("set", [flag]) if flag == "--stdin" => {
            let text = stdin().map_err(|error| (1, error))?;
            text.strip_suffix('\n')
                .map(|text| text.strip_suffix('\r').unwrap_or(text))
                .unwrap_or(&text)
                .to_owned()
        }
        ("set", [_, ..]) => values.join("\n"),
        _ => return usage(),
    };
    if value.len() > MAX_SCRIPT_FIELD_VALUE_BYTES {
        return Err((1, "the value is too large".to_owned()));
    }
    let (Some(endpoint), Some(token)) =
        (environment(ENDPOINT_VARIABLE), environment(TOKEN_VARIABLE))
    else {
        return Err((
            1,
            "this script may not set custom fields: none of its device's fields lets scripts write it, or the MeshRMM Agent didn't run it".to_owned(),
        ));
    };
    let request = Request {
        token,
        key: key.clone(),
        value,
    };
    send(&endpoint, &request).map_err(|error| (1, error))?;
    Ok(None)
}

/// Sends `request` to the Agent's channel at `endpoint`, which must be on
/// this computer, and waits for its answer.
fn send(endpoint: &str, request: &Request) -> Result<(), String> {
    let address = endpoint
        .parse::<SocketAddr>()
        .ok()
        .filter(|address| address.ip().is_loopback())
        .ok_or_else(|| format!("{ENDPOINT_VARIABLE} is not an address on this computer"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|error| format!("could not reach the MeshRMM Agent: {error}"))?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(REPLY_TIMEOUT)))
        .map_err(|error| error.to_string())?;
    let mut line = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("could not reach the MeshRMM Agent: {error}"))?;
    let mut reply = String::new();
    BufReader::new(stream.take(64 * 1024))
        .read_line(&mut reply)
        .map_err(|error| format!("the MeshRMM Agent didn't answer: {error}"))?;
    let reply: Reply =
        serde_json::from_str(&reply).map_err(|_| "the MeshRMM Agent didn't answer".to_owned())?;
    match reply.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// `variables` as lines of a shell script that export them, quoted so any
/// value stays as it is, and that add the Agent's folder to the end of
/// `PATH`. macOS and Linux scripts start with these, since `sudo` would
/// drop variables set on the process.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", test)),
    allow(dead_code)
)]
pub(crate) fn shell_exports(variables: &[(String, String)], folder: Option<&str>) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    let mut lines = variables
        .iter()
        .map(|(name, value)| format!("export {name}={}\n", quote(value)))
        .collect::<String>();
    if let Some(folder) = folder {
        lines.push_str(&format!("export PATH=\"$PATH\":{}\n", quote(folder)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use meshrmm_protocol::{RunAs, ScriptField, ScriptLanguage};

    use super::*;

    fn run(fields: Vec<ScriptField>) -> ScriptRunRequest {
        ScriptRunRequest {
            run_id: "run-1".into(),
            language: ScriptLanguage::Shell,
            body: String::new(),
            run_as: RunAs::System,
            timeout_seconds: 60,
            fields,
        }
    }

    fn field(key: &str, value: Option<&str>, writable: bool) -> ScriptField {
        ScriptField {
            key: key.into(),
            value: value.map(str::to_owned),
            writable,
        }
    }

    /// Records what reaches the server, refusing the key `refused`.
    struct Recorder(Mutex<mpsc::Sender<(String, String, String)>>);

    impl Forward for Recorder {
        fn set(&self, run_id: &str, key: &str, value: &str) -> Result<(), String> {
            if key == "refused" {
                return Err("scripts may not change “Refused”".into());
            }
            self.0
                .lock()
                .unwrap()
                .send((run_id.into(), key.into(), value.into()))
                .unwrap();
            Ok(())
        }
    }

    fn environment(variables: &[(String, String)]) -> impl Fn(&str) -> Option<String> + '_ {
        move |name| {
            variables
                .iter()
                .find(|(variable, _)| variable == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn arguments(text: &[&str]) -> Vec<String> {
        text.iter().map(|argument| (*argument).to_owned()).collect()
    }

    #[test]
    fn scripts_read_fields_from_their_environment() {
        let request = run(vec![
            field("contract_tier", Some("Gold"), false),
            field("unset", None, true),
            field("Bad-Key", Some("x"), false),
        ]);
        let variables = variables(&request, None);
        let names = variables
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"MESHRMM_FIELD_CONTRACT_TIER"));
        assert!(names.contains(&RUN_VARIABLE));
        assert!(
            !names.iter().any(|name| name.contains("UNSET")),
            "no value, no variable"
        );
        assert!(
            !names.iter().any(|name| name.contains("BAD")),
            "invalid keys are skipped"
        );
        assert!(!names.contains(&TOKEN_VARIABLE), "no grant, no token");
        let mut stdin = || Ok(String::new());
        assert_eq!(
            run_command(
                &arguments(&["get", "contract_tier"]),
                environment(&variables),
                &mut stdin,
                |_, _| unreachable!(),
            ),
            Ok(Some("Gold".to_owned()))
        );
        assert_eq!(
            run_command(
                &arguments(&["set", "unset", "x"]),
                environment(&variables),
                &mut stdin,
                |_, _| unreachable!(),
            )
            .unwrap_err()
            .0,
            1,
            "without a token the script may not write"
        );
    }

    #[test]
    fn the_command_takes_values_as_arguments_or_input() {
        let variables = vec![
            (ENDPOINT_VARIABLE.to_owned(), "127.0.0.1:1".to_owned()),
            (TOKEN_VARIABLE.to_owned(), "t".to_owned()),
        ];
        let sent = |args: &[&str], input: &str| {
            let mut stdin = || Ok(input.to_owned());
            let mut value = None;
            let outcome = run_command(
                &arguments(args),
                environment(&variables),
                &mut stdin,
                |endpoint, request| {
                    assert_eq!(endpoint, "127.0.0.1:1");
                    assert_eq!(request.token, "t");
                    value = Some(request.value.clone());
                    Ok(())
                },
            );
            (outcome, value)
        };
        assert_eq!(
            sent(&["set", "tier", "Gold"], ""),
            (Ok(None), Some("Gold".into()))
        );
        assert_eq!(
            sent(&["set", "tags", "A", "B"], "").1.as_deref(),
            Some("A\nB")
        );
        assert_eq!(
            sent(&["set", "notes", "--stdin"], "line 1\nline 2\r\n")
                .1
                .as_deref(),
            Some("line 1\nline 2")
        );
        assert_eq!(sent(&["clear", "tier"], "").1.as_deref(), Some(""));
        for wrong in [
            &["set", "tier"][..],
            &["set"],
            &["delete", "tier"],
            &["set", "Tier", "x"],
        ] {
            assert_eq!(sent(wrong, "").0.unwrap_err().0, 2, "{wrong:?}");
        }
    }

    #[test]
    fn the_channel_takes_values_only_from_running_scripts_for_their_writable_fields() {
        let (sender, received) = mpsc::channel();
        let channel = Channel::start(Recorder(Mutex::new(sender))).unwrap();
        let request = run(vec![
            field("tier", Some("Gold"), false),
            field("backup", None, true),
            field("refused", None, true),
        ]);
        let grant = channel.grant(&request).expect("it may write a field");
        let variables = variables(&request, Some(&grant));
        let endpoint = environment(&variables)(ENDPOINT_VARIABLE).unwrap();
        assert!(endpoint.starts_with("127.0.0.1:"), "{endpoint}");
        let token = environment(&variables)(TOKEN_VARIABLE).unwrap();
        assert_eq!(token.len(), 64);
        let set = |token: &str, key: &str, value: &str| {
            send(
                &endpoint,
                &Request {
                    token: token.into(),
                    key: key.into(),
                    value: value.into(),
                },
            )
        };

        set(&token, "backup", "2024-05-01").unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(10)).unwrap(),
            ("run-1".into(), "backup".into(), "2024-05-01".into())
        );
        let error = set(&token, "tier", "Silver").unwrap_err();
        assert!(error.contains("may not change"), "read-only: {error}");
        let error = set(&token, "refused", "x").unwrap_err();
        assert!(
            error.contains("Refused"),
            "the server's refusal reaches the script: {error}"
        );
        let error = set(&"0".repeat(64), "backup", "x").unwrap_err();
        assert!(error.contains("finished"), "another token: {error}");

        drop(grant);
        let error = set(&token, "backup", "x").unwrap_err();
        assert!(error.contains("finished"), "the run ended: {error}");
        assert!(
            received.try_recv().is_err(),
            "nothing else reached the server"
        );

        assert!(
            channel
                .grant(&run(vec![field("tier", Some("Gold"), false)]))
                .is_none(),
            "a script that may only read gets no token"
        );
        assert!(
            send(
                "10.0.0.1:80",
                &Request {
                    token: token.clone(),
                    key: "backup".into(),
                    value: String::new()
                }
            )
            .unwrap_err()
            .contains("not an address on this computer")
        );
    }

    #[test]
    fn shell_exports_keep_values_as_they_are() {
        let exports = shell_exports(
            &[
                ("MESHRMM_FIELD_A".into(), "it's $HOME `x` \\n".into()),
                ("MESHRMM_FIELD_B".into(), "two\nlines".into()),
            ],
            Some("/opt/mesh rmm"),
        );
        assert_eq!(
            exports,
            "export MESHRMM_FIELD_A='it'\\''s $HOME `x` \\n'\nexport MESHRMM_FIELD_B='two\nlines'\nexport PATH=\"$PATH\":'/opt/mesh rmm'\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shells_read_the_exports_back_unchanged() {
        let values = ["it's $HOME `x` \\n \"q\"", "two\nlines", "", "trailing \n"];
        let variables = values
            .iter()
            .enumerate()
            .map(|(index, value)| (format!("MESHRMM_FIELD_V{index}"), (*value).to_owned()))
            .collect::<Vec<_>>();
        let mut script = shell_exports(&variables, Some("/opt/meshrmm"));
        for index in 0..values.len() {
            script.push_str(&format!("printf '%s\\0' \"$MESHRMM_FIELD_V{index}\"\n"));
        }
        script.push_str("printf '%s' \"$PATH\"\n");
        let shell = ["/bin/bash", "/bin/zsh", "/bin/sh"]
            .into_iter()
            .find(|shell| std::path::Path::new(shell).exists())
            .unwrap();
        let output = std::process::Command::new(shell)
            .arg("-c")
            .arg(&script)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let mut parts = text.split('\0');
        for value in values {
            assert_eq!(parts.next(), Some(value));
        }
        assert_eq!(parts.next(), Some("/usr/bin:/bin:/opt/meshrmm"));
    }
}

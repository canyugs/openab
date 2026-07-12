use crate::acp::protocol::{
    parse_config_options, ConfigOption, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
};
use crate::acp_turn::{TurnStartError, TurnTicket};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};
use uuid::Uuid;

/// Pick the most permissive selectable permission option from ACP options.
fn pick_best_option(options: &[Value]) -> Option<String> {
    let mut fallback: Option<&Value> = None;

    for kind in ["allow_always", "allow_once"] {
        if let Some(option) = options
            .iter()
            .find(|option| option.get("kind").and_then(|k| k.as_str()) == Some(kind))
        {
            return option
                .get("optionId")
                .and_then(|id| id.as_str())
                .map(str::to_owned);
        }
    }

    for option in options {
        let kind = option.get("kind").and_then(|k| k.as_str());
        if kind == Some("reject_once") || kind == Some("reject_always") {
            continue;
        }
        fallback = Some(option);
        break;
    }

    fallback
        .and_then(|option| option.get("optionId"))
        .and_then(|id| id.as_str())
        .map(str::to_owned)
}

/// Build a spec-compliant permission response with backward-compatible fallback.
fn build_permission_response(params: Option<&Value>) -> Value {
    match params
        .and_then(|p| p.get("options"))
        .and_then(|options| options.as_array())
    {
        None => json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_always"
            }
        }),
        Some(options) => {
            if let Some(option_id) = pick_best_option(options) {
                json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id
                    }
                })
            } else {
                json!({
                    "outcome": {
                        "outcome": "cancelled"
                    }
                })
            }
        }
    }
}

fn expand_env(val: &str) -> String {
    if val.starts_with("${") && val.ends_with('}') {
        let key = &val[2..val.len() - 1];
        std::env::var(key).unwrap_or_default()
    } else {
        val.to_string()
    }
}
use tokio::time::Instant;

/// A content block for the ACP prompt — either text or image.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
}

impl ContentBlock {
    pub fn to_json(&self) -> Value {
        match self {
            ContentBlock::Text { text } => json!({
                "type": "text",
                "text": text
            }),
            ContentBlock::Image { media_type, data } => json!({
                "type": "image",
                "data": data,
                "mimeType": media_type
            }),
        }
    }
}

/// Lock-free view of session activity, readable without the connection mutex.
pub struct SessionActivity {
    /// Milliseconds since process boot (monotonic) of the last observed activity.
    last_active_ms: AtomicU64,
    /// True while a prompt turn is in flight (mutex likely held).
    prompt_in_flight: AtomicBool,
}

impl Default for SessionActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionActivity {
    pub fn new() -> Self {
        Self {
            last_active_ms: AtomicU64::new(Self::now_ms()),
            prompt_in_flight: AtomicBool::new(false),
        }
    }

    /// Monotonic milliseconds since first use (process boot). SystemTime is
    /// unsuitable here: a wall-clock step (NTP, manual change) could make an
    /// active session look hours stale and trigger a false hung eviction.
    fn now_ms() -> u64 {
        use std::sync::OnceLock;
        static BOOT: OnceLock<std::time::Instant> = OnceLock::new();
        BOOT.get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    pub fn touch(&self) {
        self.last_active_ms.store(Self::now_ms(), Ordering::Release);
    }

    pub fn set_in_flight(&self, in_flight: bool) {
        self.prompt_in_flight.store(in_flight, Ordering::Release);
    }

    /// Milliseconds since process boot of the last observed activity.
    pub fn last_active_ms(&self) -> u64 {
        self.last_active_ms.load(Ordering::Acquire)
    }

    /// Elapsed time since the last observed activity (saturating at zero).
    pub fn age(&self) -> std::time::Duration {
        let last = self.last_active_ms.load(Ordering::Acquire);
        std::time::Duration::from_millis(Self::now_ms().saturating_sub(last))
    }

    pub fn in_flight(&self) -> bool {
        self.prompt_in_flight.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn set_last_active_ms(&self, ms: u64) {
        self.last_active_ms.store(ms, Ordering::Release);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnPhase {
    Idle,
    InFlight { request_id: u64, session_id: String },
    CancelRequested { request_id: u64, session_id: String },
    CancelWriteIndeterminate { request_id: u64 },
    Finished { request_id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelRejection {
    DifferentConnection,
    NoActiveTurn,
    StaleRequest,
    Duplicate,
    WriteIndeterminate,
    Finished,
}

impl std::fmt::Display for CancelRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::DifferentConnection => "turn ticket belongs to a different ACP connection",
            Self::NoActiveTurn => "there is no active ACP turn to cancel",
            Self::StaleRequest => "turn ticket is stale",
            Self::Duplicate => "cancellation was already requested for this turn",
            Self::WriteIndeterminate => {
                "the previous cancellation write had an indeterminate outcome"
            }
            Self::Finished => "ACP turn has already finished",
        };
        f.write_str(message)
    }
}

#[derive(Debug)]
struct TurnCancelState {
    phase: TurnPhase,
}

impl Default for TurnCancelState {
    fn default() -> Self {
        Self {
            phase: TurnPhase::Idle,
        }
    }
}

impl TurnCancelState {
    fn can_start(&self) -> bool {
        matches!(self.phase, TurnPhase::Idle | TurnPhase::Finished { .. })
    }

    fn activate(&mut self, request_id: u64, session_id: String) -> bool {
        if !self.can_start() {
            return false;
        }
        self.phase = TurnPhase::InFlight {
            request_id,
            session_id,
        };
        true
    }

    fn begin_cancel(
        &mut self,
        connection_id: Uuid,
        ticket: &TurnTicket,
    ) -> std::result::Result<String, CancelRejection> {
        if ticket.connection_id() != connection_id {
            return Err(CancelRejection::DifferentConnection);
        }

        let ticket_request_id = ticket.request_id();
        match &self.phase {
            TurnPhase::Idle => Err(CancelRejection::NoActiveTurn),
            TurnPhase::InFlight {
                request_id,
                session_id,
            } if *request_id == ticket_request_id => {
                let session_id = session_id.clone();
                self.phase = TurnPhase::CancelRequested {
                    request_id: ticket_request_id,
                    session_id: session_id.clone(),
                };
                Ok(session_id)
            }
            TurnPhase::CancelRequested { request_id, .. } if *request_id == ticket_request_id => {
                Err(CancelRejection::Duplicate)
            }
            TurnPhase::CancelWriteIndeterminate { request_id }
                if *request_id == ticket_request_id =>
            {
                Err(CancelRejection::WriteIndeterminate)
            }
            TurnPhase::Finished { request_id } if *request_id == ticket_request_id => {
                Err(CancelRejection::Finished)
            }
            TurnPhase::InFlight { .. }
            | TurnPhase::CancelRequested { .. }
            | TurnPhase::CancelWriteIndeterminate { .. }
            | TurnPhase::Finished { .. } => Err(CancelRejection::StaleRequest),
        }
    }

    fn is_open_for(&self, connection_id: Uuid, ticket: &TurnTicket) -> bool {
        if ticket.connection_id() != connection_id {
            return false;
        }
        let ticket_request_id = ticket.request_id();
        matches!(
            self.phase,
            TurnPhase::InFlight { request_id, .. }
                | TurnPhase::CancelRequested { request_id, .. }
                | TurnPhase::CancelWriteIndeterminate { request_id }
                if request_id == ticket_request_id
        )
    }

    fn current_ticket(&self, connection_id: Uuid) -> Option<TurnTicket> {
        let request_id = match self.phase {
            TurnPhase::Idle => return None,
            TurnPhase::InFlight { request_id, .. }
            | TurnPhase::CancelRequested { request_id, .. }
            | TurnPhase::CancelWriteIndeterminate { request_id }
            | TurnPhase::Finished { request_id } => request_id,
        };
        Some(TurnTicket::new(connection_id, request_id))
    }

    fn cancel_write_failed(&mut self, connection_id: Uuid, ticket: &TurnTicket) -> bool {
        if ticket.connection_id() != connection_id {
            return false;
        }
        let ticket_request_id = ticket.request_id();
        match self.phase {
            TurnPhase::CancelRequested { request_id, .. } if request_id == ticket_request_id => {
                self.phase = TurnPhase::CancelWriteIndeterminate {
                    request_id: ticket_request_id,
                };
                true
            }
            _ => false,
        }
    }

    fn cancel_write_is_indeterminate(&self, connection_id: Uuid, ticket: &TurnTicket) -> bool {
        ticket.connection_id() == connection_id
            && matches!(
                self.phase,
                TurnPhase::CancelWriteIndeterminate { request_id }
                    if request_id == ticket.request_id()
            )
    }

    /// Mark a matching turn finished. Returning true for an already-finished
    /// matching ticket lets `prompt_done` perform its remaining cleanup after
    /// `abandon_request` has terminalized the same turn.
    fn finish(&mut self, connection_id: Uuid, ticket: &TurnTicket) -> bool {
        if ticket.connection_id() != connection_id {
            return false;
        }
        let ticket_request_id = ticket.request_id();
        match self.phase {
            TurnPhase::InFlight { request_id, .. }
            | TurnPhase::CancelRequested { request_id, .. }
            | TurnPhase::CancelWriteIndeterminate { request_id }
                if request_id == ticket_request_id =>
            {
                self.phase = TurnPhase::Finished {
                    request_id: ticket_request_id,
                };
                true
            }
            TurnPhase::Finished { request_id } if request_id == ticket_request_id => true,
            TurnPhase::Idle
            | TurnPhase::InFlight { .. }
            | TurnPhase::CancelRequested { .. }
            | TurnPhase::CancelWriteIndeterminate { .. }
            | TurnPhase::Finished { .. } => false,
        }
    }
}

/// Cloneable, turn-scoped cancellation handle.
///
/// Unlike the legacy raw stdin handle, this validates both the connection and
/// request identity before writing `session/cancel`. The lifecycle mutex stays
/// locked through the stdin write, so a late cancellation cannot race turn
/// completion and cancel a subsequently-started prompt on the same session.
#[derive(Clone)]
pub struct TurnCancelHandle {
    connection_id: Uuid,
    child_pgid: Option<i32>,
    stdin: Arc<Mutex<ChildStdin>>,
    state: Arc<Mutex<TurnCancelState>>,
    poisoned: Arc<AtomicBool>,
}

impl TurnCancelHandle {
    pub fn connection_id(&self) -> Uuid {
        self.connection_id
    }

    pub async fn cancel(&self, ticket: &TurnTicket) -> Result<()> {
        let mut state = self.state.lock().await;
        let session_id = state
            .begin_cancel(self.connection_id, ticket)
            .map_err(|rejection| anyhow!(rejection.to_string()))?;

        let data = match serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id},
        })) {
            Ok(data) => data,
            Err(error) => {
                let _ = state.cancel_write_failed(self.connection_id, ticket);
                self.poisoned.store(true, Ordering::Release);
                terminate_process_group(self.child_pgid);
                return Err(error.into());
            }
        };

        // Keep `state` locked until the complete JSON line is flushed. This
        // serializes validation and delivery with prompt completion/start.
        let write_result = match tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut writer = self.stdin.lock().await;
            writer.write_all(data.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("stdin write timeout while cancelling ACP turn")),
        };
        if let Err(error) = write_result {
            let _ = state.cancel_write_failed(self.connection_id, ticket);
            self.poisoned.store(true, Ordering::Release);
            // Cancellation delivery is indeterminate: the agent may not have
            // observed the notification and could otherwise keep mutating the
            // workspace. Terminate the exact child process tree immediately;
            // this poisoned connection can never accept another turn.
            terminate_process_group(self.child_pgid);
            return Err(error);
        }

        debug!(
            connection_id = %self.connection_id,
            request_id = ticket.request_id(),
            "sent targeted ACP turn cancellation"
        );
        drop(state);
        Ok(())
    }
}

pub struct AcpConnection {
    _proc: Child,
    /// PID of the direct child, used as the process group ID for cleanup.
    child_pgid: Option<i32>,
    stdin: Arc<Mutex<ChildStdin>>,
    connection_id: Uuid,
    turn_cancel_state: Arc<Mutex<TurnCancelState>>,
    /// Set when a prompt write may have only partially reached stdin. Such a
    /// JSON stream cannot be repaired safely and the connection must be replaced.
    poisoned: Arc<AtomicBool>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>>,
    notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
    pub acp_session_id: Option<String>,
    pub supports_load_session: bool,
    pub config_options: Vec<ConfigOption>,
    pub last_active: Instant,
    pub activity: Arc<SessionActivity>,
    pub session_reset: bool,
    _reader_handle: JoinHandle<()>,
    _stderr_handle: Option<JoinHandle<()>>,
}

/// Build the final set of env vars for the agent subprocess.
/// `explicit` ([agent].env) takes precedence over `inherit` ([agent].inherit_env).
/// Returns (merged env map, list of keys that were inherited from the process).
fn build_agent_env(
    explicit: &std::collections::HashMap<String, String>,
    inherit_keys: &[String],
) -> (std::collections::HashMap<String, String>, Vec<String>) {
    let mut result: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut inherited: Vec<String> = Vec::new();

    for (k, v) in explicit {
        result.insert(k.clone(), expand_env(v));
    }

    for key in inherit_keys {
        if !result.contains_key(key) {
            if let Ok(v) = std::env::var(key) {
                result.insert(key.clone(), v);
                inherited.push(key.clone());
            }
        }
    }

    (result, inherited)
}

/// Reader loop body: reads JSON-RPC messages from `reader`, auto-replies
/// `session/request_permission` via `writer`, resolves pending responses,
/// and forwards notifications + stale id-bearing messages to the active
/// subscriber. Extracted as a free generic function so unit tests can drive
/// it with `tokio::io::duplex()` halves instead of a real child process.
pub(crate) async fn run_reader_loop<R, W>(
    reader: R,
    writer: Arc<Mutex<W>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>>,
    notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                error!("reader error: {e}");
                break;
            }
        }
        let msg: JsonRpcMessage = match serde_json::from_str(line.trim()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        debug!(line = line.trim(), "acp_recv");

        // Auto-reply session/request_permission
        if msg.method.as_deref() == Some("session/request_permission") {
            if let Some(id) = msg.id {
                let title = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("toolCall"))
                    .and_then(|t| t.get("title"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("?");

                let outcome = build_permission_response(msg.params.as_ref());
                info!(title, %outcome, "auto-respond permission");
                let reply = JsonRpcResponse::new(id, outcome);
                if let Ok(data) = serde_json::to_string(&reply) {
                    let mut w = writer.lock().await;
                    let _ = w.write_all(format!("{data}\n").as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
            continue;
        }

        // Response (has id) → resolve pending AND forward to subscriber
        if let Some(id) = msg.id {
            let mut map = pending.lock().await;
            if let Some(tx) = map.remove(&id) {
                // Forward to subscriber so they see the completion
                let sub = notify_tx.lock().await;
                if let Some(ntx) = sub.as_ref() {
                    // Clone the essential fields for the subscriber
                    let _ = ntx.send(JsonRpcMessage {
                        id: Some(id),
                        method: None,
                        result: msg.result.clone(),
                        error: msg.error.clone(),
                        params: None,
                    });
                }
                let _ = tx.send(msg);
                continue;
            }
            // Stale id (#732): pending was already abandoned. Falls through
            // to subscriber forwarding; the adapter recv loop filters by
            // request_id so it can't leak into the next prompt.
            trace!(request_id = id, "stale id-bearing message after abandon");
        }

        // Notification → forward to subscriber
        let sub = notify_tx.lock().await;
        if let Some(tx) = sub.as_ref() {
            let _ = tx.send(msg);
        }
    }

    // Connection closed — resolve all pending with error
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(JsonRpcMessage {
            id: None,
            method: None,
            result: None,
            error: Some(crate::acp::protocol::JsonRpcError {
                code: -1,
                message: "connection closed".into(),
                data: None,
            }),
            params: None,
        });
    }
    // Close the notify channel so rx.recv() returns None
    let mut sub = notify_tx.lock().await;
    *sub = None;
}

impl AcpConnection {
    pub async fn spawn(
        command: &str,
        args: &[String],
        working_dir: &str,
        env: &std::collections::HashMap<String, String>,
        inherit_env: &[String],
    ) -> Result<Self> {
        info!(cmd = command, ?args, cwd = working_dir, "spawning agent");

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(working_dir);
        // Create a new process group so we can kill the entire tree.
        // SAFETY: setpgid is async-signal-safe (POSIX.1-2008) and called
        // before exec. Return value checked — failure means the child won't
        // have its own process group, so kill(-pgid) would be unsafe.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(0x00000200); // CREATE_NEW_PROCESS_GROUP
        }
        // Clear inherited env to prevent credential leakage (e.g. DISCORD_BOT_TOKEN).
        // Only [agent].env values + essential baseline vars are passed through.
        cmd.env_clear();
        // Preserve the real HOME so agents can find OAuth/auth files (~/.codex,
        // ~/.claude, ~/.config/gh, etc.). working_dir is already set via
        // current_dir() above and is not necessarily the user's home directory.
        cmd.env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| working_dir.into()),
        );
        cmd.env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        );
        #[cfg(unix)]
        {
            cmd.env(
                "USER",
                std::env::var("USER").unwrap_or_else(|_| "agent".into()),
            );
        }
        #[cfg(windows)]
        {
            // Windows requires SystemRoot for DLL loading and basic OS functionality.
            // USERPROFILE is the Windows equivalent of HOME.
            cmd.env(
                "USERPROFILE",
                std::env::var("USERPROFILE").unwrap_or_else(|_| working_dir.into()),
            );
            cmd.env(
                "USERNAME",
                std::env::var("USERNAME").unwrap_or_else(|_| "agent".into()),
            );
            if let Ok(v) = std::env::var("SystemRoot") {
                cmd.env("SystemRoot", v);
            }
            if let Ok(v) = std::env::var("SystemDrive") {
                cmd.env("SystemDrive", v);
            }
        }
        for (k, v) in env {
            cmd.env(k, expand_env(v));
        }
        // Inherit selected env vars from the OAB process (e.g. vars injected
        // via Kubernetes envFrom).  Keys already in [agent].env are skipped —
        // explicit values take precedence.
        let (agent_env, inherited_keys) = build_agent_env(env, inherit_env);
        for (k, v) in &agent_env {
            cmd.env(k, v);
        }
        if !agent_env.is_empty() {
            let explicit_keys: Vec<&String> = env.keys().collect();
            tracing::warn!(
                ?explicit_keys,
                ?inherited_keys,
                "[agent].env/inherit_env is set -- these values are accessible to the agent and could be exfiltrated via prompt injection"
            );
        }
        let mut proc = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {command}: {e}"))?;
        let child_pgid = proc.id().and_then(|pid| i32::try_from(pid).ok());

        let stdout = proc.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stdin = proc.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdin = Arc::new(Mutex::new(stdin));

        // Capture agent stderr and log it (ACP spec: agents MAY write to stderr
        // for logging; clients MAY capture or ignore it).
        let stderr_handle = if let Some(stderr) = proc.stderr.take() {
            let cmd_name = command.to_string();
            Some(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                let sanitized: String = trimmed
                                    .chars()
                                    .filter(|c| !c.is_control() || *c == '\t')
                                    .collect();
                                if !sanitized.is_empty() {
                                    tracing::warn!(agent = %cmd_name, "{sanitized}");
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }))
        } else {
            None
        };

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(None));

        let reader_handle = tokio::spawn(run_reader_loop(
            stdout,
            stdin.clone(),
            pending.clone(),
            notify_tx.clone(),
        ));

        let activity = Arc::new(SessionActivity::new());
        let connection_id = Uuid::new_v4();
        let turn_cancel_state = Arc::new(Mutex::new(TurnCancelState::default()));

        Ok(Self {
            _proc: proc,
            child_pgid,
            stdin,
            connection_id,
            turn_cancel_state,
            poisoned: Arc::new(AtomicBool::new(false)),
            next_id: AtomicU64::new(1),
            pending,
            notify_tx,
            acp_session_id: None,
            supports_load_session: false,
            config_options: Vec::new(),
            last_active: Instant::now(),
            activity,
            session_reset: false,
            _reader_handle: reader_handle,
            _stderr_handle: stderr_handle,
        })
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) async fn send_raw(&self, data: &str) -> Result<()> {
        debug!(data = data.trim(), "acp_send");
        // A hung agent can stop draining stdin; bound the write so callers
        // (and the mutexes they hold) can never block on it indefinitely.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut w = self.stdin.lock().await;
            w.write_all(data.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("stdin write timeout"))??;
        Ok(())
    }

    async fn send_request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcMessage> {
        let id = self.next_id();
        let req = JsonRpcRequest::new(id, method, params);
        let data = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.send_raw(&data).await?;

        let timeout_secs = if method == "session/new" { 120 } else { 30 };
        let resp = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx)
            .await
            .map_err(|_| anyhow!("timeout waiting for {method} response"))?
            .map_err(|_| anyhow!("channel closed waiting for {method}"))?;

        if let Some(err) = &resp.error {
            return Err(anyhow!("{err}"));
        }
        Ok(resp)
    }

    pub async fn initialize(&mut self) -> Result<()> {
        let resp = self
            .send_request(
                "initialize",
                Some(json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "openab", "version": "0.1.0"},
                })),
            )
            .await?;

        let result = resp.result.as_ref();
        let agent_name = result
            .and_then(|r| r.get("agentInfo"))
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        self.supports_load_session = result
            .and_then(|r| r.get("agentCapabilities"))
            .and_then(|c| c.get("loadSession"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        info!(
            agent = agent_name,
            load_session = self.supports_load_session,
            "initialized"
        );
        Ok(())
    }

    pub async fn session_new(&mut self, cwd: &str) -> Result<String> {
        let resp = self
            .send_request("session/new", Some(json!({"cwd": cwd, "mcpServers": []})))
            .await?;

        let session_id = resp
            .result
            .as_ref()
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("no sessionId in session/new response"))?
            .to_string();

        info!(session_id = %session_id, "session created");
        self.acp_session_id = Some(session_id.clone());
        if let Some(result) = resp.result.as_ref() {
            self.config_options = parse_config_options(result);
            if !self.config_options.is_empty() {
                info!(count = self.config_options.len(), "parsed configOptions");
            }
        }
        Ok(session_id)
    }

    /// Set a config option (e.g. model, mode) via ACP session/set_config_option.
    /// Returns the updated list of all config options.
    pub async fn set_config_option(
        &mut self,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<ConfigOption>> {
        let session_id = self
            .acp_session_id
            .as_ref()
            .ok_or_else(|| anyhow!("no session"))?
            .clone();

        let resp = self
            .send_request(
                "session/set_config_option",
                Some(json!({
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": value,
                })),
            )
            .await;

        match resp {
            Ok(r) => {
                if let Some(result) = r.result.as_ref() {
                    self.config_options = parse_config_options(result);
                }
                info!(config_id, value, "config option set");
            }
            Err(_) => {
                // Fall back: send as a slash command (e.g. "/model claude-sonnet-4")
                let cmd = format!("/{config_id} {value}");
                info!(
                    cmd,
                    "set_config_option not supported, falling back to prompt"
                );
                let _resp = self
                    .send_request(
                        "session/prompt",
                        Some(json!({
                            "sessionId": session_id,
                            "prompt": [{"type": "text", "text": cmd}],
                        })),
                    )
                    .await?;
                for opt in &mut self.config_options {
                    if opt.id == config_id {
                        opt.current_value = value.to_string();
                    }
                }
            }
        }

        Ok(self.config_options.clone())
    }

    /// Send a prompt using the historical request-ID return contract.
    ///
    /// New turn-scoped code should call [`Self::session_prompt_typed`].
    pub async fn session_prompt(
        &mut self,
        content_blocks: Vec<ContentBlock>,
    ) -> Result<(mpsc::UnboundedReceiver<JsonRpcMessage>, u64)> {
        match self.session_prompt_typed(content_blocks).await {
            Ok((receiver, ticket)) => Ok((receiver, ticket.request_id())),
            Err(TurnStartError::NotStarted { error }) => Err(anyhow!(error)),
            Err(TurnStartError::WriteIndeterminate { ticket, error }) => {
                self.prompt_done_typed(&ticket).await;
                Err(anyhow!(error))
            }
        }
    }

    /// Send a prompt with content blocks (text and/or images), returning a
    /// receiver for streaming notifications and its exact turn ticket. The
    /// final message on the channel will have id set (the prompt response).
    pub async fn session_prompt_typed(
        &mut self,
        content_blocks: Vec<ContentBlock>,
    ) -> std::result::Result<(mpsc::UnboundedReceiver<JsonRpcMessage>, TurnTicket), TurnStartError>
    {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(TurnStartError::NotStarted {
                error: "ACP connection is unusable after an indeterminate prompt write".to_string(),
            });
        }

        let session_id = self
            .acp_session_id
            .as_ref()
            .ok_or_else(|| TurnStartError::NotStarted {
                error: "no session".to_string(),
            })?
            .clone();

        let id = self.next_id();

        // Convert content blocks to JSON
        let prompt_json: Vec<Value> = content_blocks.iter().map(|b| b.to_json()).collect();

        let req = JsonRpcRequest::new(
            id,
            "session/prompt",
            Some(json!({
                "sessionId": session_id,
                "prompt": prompt_json,
            })),
        );
        let data = serde_json::to_string(&req).map_err(|error| TurnStartError::NotStarted {
            error: error.to_string(),
        })?;

        // Refuse overlapping turns before publishing a subscriber or pending
        // request. Connections are normally serialized by SessionPool, but
        // enforcing the invariant here keeps the turn ticket trustworthy.
        if !self.turn_cancel_state.lock().await.can_start() {
            return Err(TurnStartError::NotStarted {
                error: "an ACP turn is already in flight".to_string(),
            });
        }

        let (tx, rx) = mpsc::unbounded_channel();
        *self.notify_tx.lock().await = Some(tx);

        let (resp_tx, _resp_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, resp_tx);

        let ticket = TurnTicket::new(self.connection_id, id);
        let activated = self.turn_cancel_state.lock().await.activate(id, session_id);
        if !activated {
            self.pending.lock().await.remove(&id);
            *self.notify_tx.lock().await = None;
            return Err(TurnStartError::NotStarted {
                error: format!("ACP turn lifecycle changed while starting request {id}"),
            });
        }

        self.last_active = Instant::now();
        self.activity.touch();
        self.activity.set_in_flight(true);

        if let Err(error) = self.send_raw(&data).await {
            self.pending.lock().await.remove(&id);
            self.poisoned.store(true, Ordering::Release);
            // A failed write_all/newline/flush may have left a partial JSON
            // object on stdin. Do not append cancellation or another request;
            // terminate the process tree and force SessionPool replacement.
            self.kill_process_group();
            let _ = self._proc.start_kill();
            return Err(TurnStartError::WriteIndeterminate {
                ticket,
                error: error.to_string(),
            });
        }

        Ok((rx, ticket))
    }

    /// Clean up the currently active prompt using the historical unscoped API.
    /// New turn-scoped code should call [`Self::prompt_done_typed`].
    pub async fn prompt_done(&mut self) {
        let ticket = self
            .turn_cancel_state
            .lock()
            .await
            .current_ticket(self.connection_id);
        if let Some(ticket) = ticket {
            self.prompt_done_typed(&ticket).await;
        } else {
            *self.notify_tx.lock().await = None;
            self.activity.touch();
            self.activity.set_in_flight(false);
            self.last_active = Instant::now();
        }
    }

    /// Call after prompt streaming is done to clean up the exact subscriber.
    pub async fn prompt_done_typed(&mut self, ticket: &TurnTicket) {
        let matched = self
            .turn_cancel_state
            .lock()
            .await
            .finish(self.connection_id, ticket);
        if !matched {
            debug!(
                connection_id = %self.connection_id,
                ticket_connection_id = %ticket.connection_id(),
                request_id = ticket.request_id(),
                "ignored prompt_done for a non-current ACP turn"
            );
            return;
        }

        self.pending.lock().await.remove(&ticket.request_id());
        *self.notify_tx.lock().await = None;
        self.activity.touch();
        self.activity.set_in_flight(false);
        self.last_active = Instant::now();
    }

    /// Drop the pending entry for the exact turn ticket and best-effort send
    /// `session/cancel` as a JSON-RPC notification (no id; per ACP spec the
    /// agent does not reply). Errors are swallowed: the agent process may
    /// already be dead, in which case the stdin write fails harmlessly.
    /// See #732.
    pub async fn abandon_request(&self, request_id: u64) {
        let ticket = TurnTicket::new(self.connection_id, request_id);
        self.abandon_request_typed(&ticket).await;
    }

    /// Abandon only the exact connection-qualified turn ticket.
    pub async fn abandon_request_typed(&self, ticket: &TurnTicket) {
        let is_current = self
            .turn_cancel_state
            .lock()
            .await
            .is_open_for(self.connection_id, ticket);
        if !is_current {
            debug!(
                connection_id = %self.connection_id,
                ticket_connection_id = %ticket.connection_id(),
                request_id = ticket.request_id(),
                "ignored abandon_request for a non-current ACP turn"
            );
            return;
        }

        self.pending.lock().await.remove(&ticket.request_id());
        // Best effort: a concurrent targeted caller may already have sent the
        // cancellation, and a dead child may reject the write. Either way this
        // broker is abandoning the request and must terminalize only its exact
        // ticket before a later prompt can begin.
        let _ = self.turn_cancel_handle().cancel(ticket).await;
        let _ = self
            .turn_cancel_state
            .lock()
            .await
            .finish(self.connection_id, ticket);
    }

    /// Return a clone of the stdin handle for legacy session-wide cancel.
    /// New turn-scoped callers should use [`Self::turn_cancel_handle`].
    pub fn cancel_handle(&self) -> Arc<Mutex<ChildStdin>> {
        Arc::clone(&self.stdin)
    }

    /// Return a cloneable handle that can cancel only the supplied active turn.
    pub fn turn_cancel_handle(&self) -> TurnCancelHandle {
        TurnCancelHandle {
            connection_id: self.connection_id,
            child_pgid: self.child_pgid,
            stdin: Arc::clone(&self.stdin),
            state: Arc::clone(&self.turn_cancel_state),
            poisoned: Arc::clone(&self.poisoned),
        }
    }

    pub(crate) async fn cancel_write_is_indeterminate(&self, ticket: &TurnTicket) -> bool {
        self.turn_cancel_state
            .lock()
            .await
            .cancel_write_is_indeterminate(self.connection_id, ticket)
    }

    pub fn connection_id(&self) -> Uuid {
        self.connection_id
    }

    pub fn activity_handle(&self) -> Arc<SessionActivity> {
        Arc::clone(&self.activity)
    }

    /// Process-group id of the agent child, readable without any lock state.
    pub fn child_pgid(&self) -> Option<i32> {
        self.child_pgid
    }

    pub fn alive(&self) -> bool {
        !self.poisoned.load(Ordering::Acquire) && !self._reader_handle.is_finished()
    }

    /// Resume a previous session by ID. Returns Ok(()) if the agent accepted
    /// the load, or an error if it failed (caller should fall back to session/new).
    pub async fn session_load(&mut self, session_id: &str, cwd: &str) -> Result<()> {
        let resp = self
            .send_request(
                "session/load",
                Some(json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []})),
            )
            .await?;
        // Accept any non-error response as success
        if resp.error.is_some() {
            return Err(anyhow!("session/load rejected"));
        }
        info!(session_id, "session loaded");
        self.acp_session_id = Some(session_id.to_string());
        if let Some(result) = resp.result.as_ref() {
            self.config_options = parse_config_options(result);
        }
        Ok(())
    }

    fn kill_process_group(&mut self) {
        terminate_process_group(self.child_pgid);
    }
}

/// Terminate the child process tree after an indeterminate protocol write.
///
/// Unix agents run in their own process group. Windows agents are created with
/// `CREATE_NEW_PROCESS_GROUP`, and `taskkill /T` is the available tree-aware
/// termination primitive. The delayed Unix SIGKILL uses a standard thread so
/// it still fires during runtime shutdown or panic unwinding.
fn terminate_process_group(child_pgid: Option<i32>) {
    let pgid = match child_pgid {
        Some(pid) if pid > 0 => pid,
        _ => return,
    };
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        });
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pgid.to_string(), "/T", "/F"])
            .spawn();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pgid;
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        if let Some(handle) = self._stderr_handle.take() {
            handle.abort();
        }
        self.kill_process_group();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_agent_env, build_permission_response, pick_best_option, CancelRejection,
        TurnCancelState, TurnPhase,
    };
    use crate::acp_turn::TurnTicket;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn picks_allow_always_over_other_options() {
        let options = vec![
            json!({"kind": "allow_once", "optionId": "once"}),
            json!({"kind": "allow_always", "optionId": "always"}),
            json!({"kind": "reject_once", "optionId": "reject"}),
        ];

        assert_eq!(pick_best_option(&options), Some("always".to_string()));
    }

    #[test]
    fn falls_back_to_first_unknown_non_reject_kind() {
        let options = vec![
            json!({"kind": "reject_once", "optionId": "reject"}),
            json!({"kind": "workspace_write", "optionId": "workspace-write"}),
        ];

        assert_eq!(
            pick_best_option(&options),
            Some("workspace-write".to_string())
        );
    }

    #[test]
    fn selects_bypass_permissions_for_exit_plan_mode() {
        let options = vec![
            json!({"optionId": "bypassPermissions", "kind": "allow_always"}),
            json!({"optionId": "acceptEdits", "kind": "allow_always"}),
            json!({"optionId": "default", "kind": "allow_once"}),
            json!({"optionId": "plan", "kind": "reject_once"}),
        ];

        assert_eq!(
            pick_best_option(&options),
            Some("bypassPermissions".to_string())
        );
    }

    #[test]
    fn returns_none_when_only_reject_options_exist() {
        let options = vec![
            json!({"kind": "reject_once", "optionId": "reject-once"}),
            json!({"kind": "reject_always", "optionId": "reject-always"}),
        ];

        assert_eq!(pick_best_option(&options), None);
    }

    #[test]
    fn builds_cancelled_outcome_when_no_selectable_option_exists() {
        let response = build_permission_response(Some(&json!({
            "options": [
                {"kind": "reject_once", "optionId": "reject-once"}
            ]
        })));

        assert_eq!(response, json!({"outcome": {"outcome": "cancelled"}}));
    }

    #[test]
    fn builds_cancelled_when_options_array_is_empty() {
        let response = build_permission_response(Some(&json!({
            "options": []
        })));

        assert_eq!(response, json!({"outcome": {"outcome": "cancelled"}}));
    }

    #[test]
    fn falls_back_to_allow_always_when_options_are_missing() {
        let response = build_permission_response(Some(&json!({
            "toolCall": {"title": "legacy"}
        })));

        assert_eq!(
            response,
            json!({"outcome": {"outcome": "selected", "optionId": "allow_always"}})
        );
    }

    #[test]
    fn falls_back_to_allow_always_when_params_is_none() {
        let response = build_permission_response(None);

        assert_eq!(
            response,
            json!({"outcome": {"outcome": "selected", "optionId": "allow_always"}})
        );
    }

    #[test]
    fn explicit_env_takes_precedence_over_inherit_env() {
        let key = "OAB_TEST_PRECEDENCE";
        std::env::set_var(key, "from_process");
        let mut explicit = std::collections::HashMap::new();
        explicit.insert(key.to_string(), "from_config".to_string());
        let inherit = vec![key.to_string()];

        let (result, inherited) = build_agent_env(&explicit, &inherit);

        assert_eq!(result.get(key).unwrap(), "from_config");
        assert!(!inherited.contains(&key.to_string()));
        std::env::remove_var(key);
    }

    #[test]
    fn inherit_env_copies_from_process() {
        let key = "OAB_TEST_INHERIT";
        std::env::set_var(key, "process_value");
        let explicit = std::collections::HashMap::new();
        let inherit = vec![key.to_string()];

        let (result, inherited) = build_agent_env(&explicit, &inherit);

        assert_eq!(result.get(key).unwrap(), "process_value");
        assert!(inherited.contains(&key.to_string()));
        std::env::remove_var(key);
    }

    #[test]
    fn inherit_env_skips_missing_vars() {
        let explicit = std::collections::HashMap::new();
        let inherit = vec!["OAB_TEST_NONEXISTENT_VAR_12345".to_string()];

        let (result, inherited) = build_agent_env(&explicit, &inherit);

        assert!(!result.contains_key("OAB_TEST_NONEXISTENT_VAR_12345"));
        assert!(inherited.is_empty());
    }

    #[test]
    fn targeted_cancel_rejects_stale_ticket_after_later_turn_starts() {
        let connection_id = Uuid::new_v4();
        let first = TurnTicket::new(connection_id, 10);
        let second = TurnTicket::new(connection_id, 11);
        let mut state = TurnCancelState::default();

        assert!(state.activate(first.request_id(), "session-a".into()));
        assert!(state.finish(connection_id, &first));
        assert!(state.activate(second.request_id(), "session-a".into()));

        assert!(!state.finish(connection_id, &first));
        assert_eq!(
            state.begin_cancel(connection_id, &first),
            Err(CancelRejection::StaleRequest)
        );
        assert_eq!(
            state.phase,
            TurnPhase::InFlight {
                request_id: second.request_id(),
                session_id: "session-a".into(),
            }
        );
    }

    #[test]
    fn targeted_cancel_rejects_duplicate_request() {
        let connection_id = Uuid::new_v4();
        let ticket = TurnTicket::new(connection_id, 20);
        let mut state = TurnCancelState::default();
        assert!(state.activate(ticket.request_id(), "session-b".into()));

        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Ok("session-b".into())
        );
        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Err(CancelRejection::Duplicate)
        );
    }

    #[test]
    fn failed_cancel_write_is_distinct_from_duplicate_request() {
        let connection_id = Uuid::new_v4();
        let ticket = TurnTicket::new(connection_id, 21);
        let mut state = TurnCancelState::default();
        assert!(state.activate(ticket.request_id(), "session-b".into()));
        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Ok("session-b".into())
        );

        assert!(state.cancel_write_failed(connection_id, &ticket));
        assert!(state.cancel_write_is_indeterminate(connection_id, &ticket));
        assert!(!state.cancel_write_is_indeterminate(Uuid::new_v4(), &ticket));
        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Err(CancelRejection::WriteIndeterminate)
        );
        assert_eq!(
            state.phase,
            TurnPhase::CancelWriteIndeterminate {
                request_id: ticket.request_id()
            }
        );
    }

    #[test]
    fn targeted_cancel_rejects_finished_turn() {
        let connection_id = Uuid::new_v4();
        let ticket = TurnTicket::new(connection_id, 30);
        let mut state = TurnCancelState::default();
        assert!(state.activate(ticket.request_id(), "session-c".into()));
        assert!(state.finish(connection_id, &ticket));

        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Err(CancelRejection::Finished)
        );
    }

    #[test]
    fn targeted_cancel_rejects_ticket_from_another_connection() {
        let connection_id = Uuid::new_v4();
        let ticket = TurnTicket::new(Uuid::new_v4(), 40);
        let mut state = TurnCancelState::default();
        assert!(state.activate(ticket.request_id(), "session-d".into()));

        assert_eq!(
            state.begin_cancel(connection_id, &ticket),
            Err(CancelRejection::DifferentConnection)
        );
    }
}

#[cfg(test)]
mod reader_loop_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{duplex, AsyncWriteExt};
    use tokio::sync::{mpsc, oneshot, Mutex};

    /// #732 stale-id path: when a response arrives for an id the broker has
    /// already abandoned, the reader must (a) not crash, (b) leave `pending`
    /// untouched, and (c) still forward the message to whoever is currently
    /// subscribed — the adapter recv loop is responsible for filtering by
    /// request_id so the stray response never leaks into the next prompt.
    #[tokio::test]
    async fn stale_id_response_is_forwarded_without_pending_entry() {
        let (mut agent_stdout_writer, agent_stdout_reader) = duplex(8 * 1024);
        let (agent_stdin_writer, _agent_stdin_reader) = duplex(8 * 1024);

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(None));

        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        *notify_tx.lock().await = Some(sub_tx);

        let writer = Arc::new(Mutex::new(agent_stdin_writer));
        let handle = tokio::spawn(run_reader_loop(
            agent_stdout_reader,
            writer,
            pending.clone(),
            notify_tx.clone(),
        ));

        let stale = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"stopReason\":\"ok\"}}\n";
        agent_stdout_writer.write_all(stale).await.unwrap();
        agent_stdout_writer.flush().await.unwrap();

        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(2), sub_rx.recv())
            .await
            .expect("subscriber should receive stale message before timeout")
            .expect("subscriber channel should not be closed");
        assert_eq!(forwarded.id, Some(42));
        assert!(pending.lock().await.is_empty());

        drop(agent_stdout_writer);
        handle.await.unwrap();
    }

    /// Matched-id path: when a response's id is in `pending`, the loop must
    /// resolve the oneshot AND forward a copy to the subscriber so the
    /// adapter's recv loop sees the completion. Guards against regressions
    /// that would suppress the forward branch while keeping resolve.
    #[tokio::test]
    async fn matched_id_response_resolves_pending_and_forwards() {
        let (mut agent_stdout_writer, agent_stdout_reader) = duplex(8 * 1024);
        let (agent_stdin_writer, _agent_stdin_reader) = duplex(8 * 1024);

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(None));

        let (resp_tx, resp_rx) = oneshot::channel();
        pending.lock().await.insert(7, resp_tx);

        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        *notify_tx.lock().await = Some(sub_tx);

        let writer = Arc::new(Mutex::new(agent_stdin_writer));
        let handle = tokio::spawn(run_reader_loop(
            agent_stdout_reader,
            writer,
            pending.clone(),
            notify_tx.clone(),
        ));

        let payload = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"stopReason\":\"end_turn\"}}\n";
        agent_stdout_writer.write_all(payload).await.unwrap();
        agent_stdout_writer.flush().await.unwrap();

        let resolved = tokio::time::timeout(std::time::Duration::from_secs(2), resp_rx)
            .await
            .expect("oneshot should resolve")
            .expect("oneshot should not be cancelled");
        assert_eq!(resolved.id, Some(7));

        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(2), sub_rx.recv())
            .await
            .expect("subscriber should receive forwarded copy")
            .expect("subscriber channel should not be closed");
        assert_eq!(forwarded.id, Some(7));
        assert!(pending.lock().await.is_empty());

        drop(agent_stdout_writer);
        handle.await.unwrap();
    }

    #[test]
    fn session_activity_touch_advances_last_active() {
        let activity = SessionActivity::new();
        // Warm the monotonic clock past zero so a backdated value is older.
        std::thread::sleep(std::time::Duration::from_millis(10));
        activity.set_last_active_ms(0);
        let before = activity.last_active_ms();
        activity.touch();
        assert!(activity.last_active_ms() > before);
        // Backdated last_active yields a positive age; touch resets it near zero.
        activity.set_last_active_ms(0);
        assert!(activity.age() >= std::time::Duration::from_millis(10));
        activity.touch();
        assert!(activity.age() < std::time::Duration::from_secs(60));
        // A future timestamp must not underflow: age saturates at zero.
        activity.set_last_active_ms(u64::MAX);
        assert_eq!(activity.age(), std::time::Duration::ZERO);
    }

    #[test]
    fn session_activity_set_in_flight_round_trips() {
        let activity = SessionActivity::new();
        assert!(!activity.in_flight());
        activity.set_in_flight(true);
        assert!(activity.in_flight());
        activity.set_in_flight(false);
        assert!(!activity.in_flight());
    }
}

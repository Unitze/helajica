use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    CancelNotification, ContentBlock, Error as AcpError, Implementation, InitializeRequest,
    InitializeResponse, JsonRpcMessage, NewSessionRequest, NewSessionResponse, Notification,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse, Request, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, Response,
    SelectedPermissionOutcome, SessionConfigId, SessionConfigOptionValue, SessionConfigValueId,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModeRequest, StopReason, TextContent,
};
use futures_lite::future::race;
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use maki_providers::strip_provider_keys;
use maki_storage::process::hide_console;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use smol::lock::Mutex as AsyncMutex;
use smol::process::{ChildStdin, Command, Stdio};
use thiserror::Error;
use tracing::{debug, warn};

use crate::ChildGuard;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NEWLINE: u8 = b'\n';
/// Control requests answer in seconds; a prompt runs as long as the agent
/// works, so only it waits without a timeout.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

const INITIALIZE_METHOD: &str = "initialize";
const SESSION_NEW_METHOD: &str = "session/new";
const SESSION_PROMPT_METHOD: &str = "session/prompt";
const SESSION_CANCEL_METHOD: &str = "session/cancel";
const SESSION_UPDATE_METHOD: &str = "session/update";
const SESSION_REQUEST_PERMISSION_METHOD: &str = "session/request_permission";
const SESSION_SET_MODE_METHOD: &str = "session/set_mode";
const SESSION_SET_CONFIG_OPTION_METHOD: &str = "session/set_config_option";

#[derive(Debug, Error)]
pub enum AcpClientError {
    #[error("failed to start '{name}': {reason}")]
    Start { name: Arc<str>, reason: String },
    #[error("no working directory for the session: {0}")]
    Cwd(String),
    #[error("'{name}' exited")]
    ServerDied { name: Arc<str> },
    #[error("request timed out after {}s", .0.as_secs())]
    Timeout(Duration),
    #[error("agent error {code}: {message}")]
    Agent { code: i64, message: String },
    #[error("protocol error: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error("write to '{name}' failed: {reason}")]
    Write { name: Arc<str>, reason: String },
}

/// How to answer the agent's `session/request_permission` calls. The agent's
/// own config decides which actions reach the client at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Pick the first allow option, so delegated work runs unattended.
    Allow,
    /// Pick the first reject option, so the turn continues past a refused call.
    #[default]
    Deny,
    /// Refuse the whole turn by cancelling the outstanding prompt.
    Cancel,
}

impl PermissionPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Cancel => "cancel",
        }
    }
}

/// The final state of a prompt turn: the streamed agent text plus why it ended.
#[derive(Debug)]
pub struct TurnResult {
    pub stop_reason: StopReason,
    pub text: String,
}

/// Handler invoked with every `session/update` of a turn as it arrives.
pub type UpdateHandler = Box<dyn FnMut(&SessionUpdate) + Send>;

type Pending = flume::Sender<Result<Value, AcpClientError>>;

/// Everything routing needs, none of which can write, so tests can drive it
/// without a process.
pub(crate) struct Router {
    name: Arc<str>,
    pending: StdMutex<HashMap<i64, Pending>>,
    sessions: StdMutex<HashMap<Arc<str>, flume::Sender<SessionUpdate>>>,
    alive: Arc<AtomicBool>,
    policy: PermissionPolicy,
}

pub(crate) struct Shared {
    router: Router,
    stdin: AsyncMutex<ChildStdin>,
    next_id: AtomicI64,
}

/// A live agent process. Clones share the child; the last reference out kills
/// it, so callers park one in the plugin that owns the delegation.
#[derive(Clone)]
pub struct AcpClient {
    shared: Arc<Shared>,
    _child: Arc<ChildGuard>,
    _reader_task: Arc<smol::Task<()>>,
    _stderr_task: Arc<smol::Task<()>>,
}

pub struct AcpSession {
    shared: Arc<Shared>,
    response: NewSessionResponse,
    updates: flume::Receiver<SessionUpdate>,
}

impl AcpClient {
    /// Spawns `program args` and completes the `initialize` handshake.
    pub async fn start(
        name: &str,
        program: &str,
        args: &[String],
        policy: PermissionPolicy,
    ) -> Result<(Self, InitializeResponse), AcpClientError> {
        let name: Arc<str> = Arc::from(name);
        let mut child = spawn_agent(program, args).map_err(|reason| AcpClientError::Start {
            reason: reason.to_string(),
            name: Arc::clone(&name),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| missing_pipe(&name))?;
        let stdout = child.stdout.take().ok_or_else(|| missing_pipe(&name))?;
        let stderr = child.stderr.take().ok_or_else(|| missing_pipe(&name))?;

        let shared = Arc::new(Shared {
            router: Router {
                name: Arc::clone(&name),
                pending: StdMutex::new(HashMap::new()),
                sessions: StdMutex::new(HashMap::new()),
                alive: Arc::new(AtomicBool::new(true)),
                policy,
            },
            stdin: AsyncMutex::new(stdin),
            next_id: AtomicI64::new(1),
        });

        let reader_task = {
            let shared = Arc::clone(&shared);
            smol::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let mut replies = Vec::new();
                            handle_message(&shared.router, trimmed, &mut replies);
                            for reply in replies {
                                if let Err(e) = write(&shared, &reply).await {
                                    debug!(agent = &*shared.router.name, error = %e, "reply failed");
                                }
                            }
                        }
                    }
                }
                shared.router.alive.store(false, Ordering::Release);
                // Unblock everyone parked on this process: request waiters and
                // sessions' prompt loops alike.
                for (_, sender) in shared.router.pending.lock().unwrap().drain() {
                    let _ = sender.send(Err(died(&shared.router)));
                }
                shared.router.sessions.lock().unwrap().clear();
            })
        };

        let stderr_task = {
            let name = Arc::clone(&name);
            smol::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                warn!(agent = &*name, "{trimmed}");
                            }
                        }
                    }
                }
            })
        };

        let client = Self {
            shared,
            _child: Arc::new(ChildGuard::new(child)),
            _reader_task: Arc::new(reader_task),
            _stderr_task: Arc::new(stderr_task),
        };
        let info = client
            .request(
                INITIALIZE_METHOD,
                &InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("maki", VERSION)),
                CONTROL_TIMEOUT,
            )
            .await?;
        Ok((client, info))
    }

    /// Creates a session rooted at `cwd`, defaulting to the process cwd.
    pub async fn new_session(&self, cwd: Option<PathBuf>) -> Result<AcpSession, AcpClientError> {
        let cwd = match cwd {
            Some(dir) => dir,
            None => std::env::current_dir().map_err(|e| AcpClientError::Cwd(e.to_string()))?,
        };
        let response: NewSessionResponse = self
            .request(
                SESSION_NEW_METHOD,
                &NewSessionRequest::new(cwd),
                CONTROL_TIMEOUT,
            )
            .await?;
        let (sender, receiver) = flume::unbounded();
        self.shared
            .router
            .sessions
            .lock()
            .unwrap()
            .insert(response.session_id.0.clone(), sender);
        Ok(AcpSession {
            shared: Arc::clone(&self.shared),
            response,
            updates: receiver,
        })
    }

    /// Sends a request and waits out its response, dropping the waiter entry
    /// whichever way the wait ends so a late answer finds no one.
    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<T, AcpClientError> {
        let outcome = send_request(&self.shared, method, params).await?;
        let response = race(
            async { outcome.wait().await.map_err(|_| died(&self.shared.router)) },
            async {
                smol::Timer::after(timeout).await;
                Err(AcpClientError::Timeout(timeout))
            },
        )
        .await?;
        serde_json::from_value(response?).map_err(Into::into)
    }
}

impl AcpSession {
    /// The `session/new` answer, for callers that want the modes or config
    /// options the agent advertised.
    pub fn response(&self) -> &NewSessionResponse {
        &self.response
    }

    pub async fn set_mode(&self, mode: &str) -> Result<(), AcpClientError> {
        self.request(
            SESSION_SET_MODE_METHOD,
            &SetSessionModeRequest::new(
                self.response.session_id.clone(),
                SessionModeId::from(mode.to_string()),
            ),
        )
        .await
        .map(|_: Value| ())
    }

    /// Picks a `select` config option the agent advertised (e.g. the model).
    pub async fn set_config(&self, config_id: &str, value_id: &str) -> Result<(), AcpClientError> {
        self.request(
            SESSION_SET_CONFIG_OPTION_METHOD,
            &SetSessionConfigOptionRequest::new(
                self.response.session_id.clone(),
                SessionConfigId::from(config_id.to_string()),
                SessionConfigOptionValue::value_id(SessionConfigValueId::from(
                    value_id.to_string(),
                )),
            ),
        )
        .await
        .map(|_: Value| ())
    }

    /// Sends one user message and waits out the turn, handing every update to
    /// `on_event` as it arrives.
    pub async fn prompt(
        &self,
        text: &str,
        mut on_event: Option<UpdateHandler>,
    ) -> Result<TurnResult, AcpClientError> {
        let outcome = send_request(
            &self.shared,
            SESSION_PROMPT_METHOD,
            &PromptRequest::new(
                self.response.session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(text))],
            ),
        )
        .await?;

        enum Next {
            Update(Box<SessionUpdate>),
            Done(Result<Value, AcpClientError>),
        }
        let mut text = String::new();
        loop {
            let update = self.updates.recv_async();
            let done = outcome.wait();
            match race(
                async {
                    update
                        .await
                        .map(Box::new)
                        .map(Next::Update)
                        .map_err(|_| died(&self.shared.router))
                },
                async {
                    done.await
                        .map(Next::Done)
                        .map_err(|_| died(&self.shared.router))
                },
            )
            .await?
            {
                Next::Update(update) => {
                    if let SessionUpdate::AgentMessageChunk(chunk) = update.as_ref()
                        && let ContentBlock::Text(content) = &chunk.content
                    {
                        text.push_str(&content.text);
                    }
                    if let Some(f) = on_event.as_mut() {
                        f(&update);
                    }
                }
                Next::Done(result) => {
                    let response: PromptResponse = serde_json::from_value(result?)?;
                    return Ok(TurnResult {
                        stop_reason: response.stop_reason,
                        text,
                    });
                }
            }
        }
    }

    /// Asks the agent to abort the current turn. The parked `prompt` then
    /// returns with [`StopReason::Cancelled`], not an error.
    pub async fn cancel(&self) -> Result<(), AcpClientError> {
        write(
            &self.shared,
            &JsonRpcMessage::wrap(Notification {
                method: SESSION_CANCEL_METHOD.into(),
                params: Some(serde_json::to_value(CancelNotification::new(
                    self.response.session_id.clone(),
                ))?),
            }),
        )
        .await
    }

    /// A control request on an existing session, always bounded by the
    /// control timeout.
    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T, AcpClientError> {
        let outcome = send_request(&self.shared, method, params).await?;
        let done = race(
            async { outcome.wait().await.map_err(|_| died(&self.shared.router)) },
            async {
                smol::Timer::after(CONTROL_TIMEOUT).await;
                Err(AcpClientError::Timeout(CONTROL_TIMEOUT))
            },
        )
        .await?;
        serde_json::from_value(done?).map_err(Into::into)
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        self.shared
            .router
            .sessions
            .lock()
            .unwrap()
            .remove(self.response.session_id.0.as_ref());
    }
}

/// A request whose response can be awaited later. Dropping it abandons the
/// wait, leaving the entry for the reader to resolve into a closed channel.
struct Outcome {
    shared: Arc<Shared>,
    id: i64,
    receiver: flume::Receiver<Result<Value, AcpClientError>>,
}

impl Outcome {
    async fn wait(&self) -> Result<Result<Value, AcpClientError>, flume::RecvError> {
        self.receiver.recv_async().await
    }
}

impl Drop for Outcome {
    fn drop(&mut self) {
        self.shared.router.pending.lock().unwrap().remove(&self.id);
    }
}

async fn send_request(
    shared: &Arc<Shared>,
    method: &str,
    params: impl Serialize,
) -> Result<Outcome, AcpClientError> {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = flume::bounded(1);
    shared.router.pending.lock().unwrap().insert(id, sender);
    let request = JsonRpcMessage::wrap(Request {
        id: RequestId::Number(id),
        method: method.into(),
        params: Some(serde_json::to_value(params)?),
    });
    if let Err(e) = write(shared, &request).await {
        shared.router.pending.lock().unwrap().remove(&id);
        return Err(e);
    }
    Ok(Outcome {
        shared: Arc::clone(shared),
        id,
        receiver,
    })
}

fn missing_pipe(name: &Arc<str>) -> AcpClientError {
    AcpClientError::Start {
        name: Arc::clone(name),
        reason: "no stdio".into(),
    }
}

/// Spawns the agent process. On Windows a bare program name finds no `.exe`
/// for the npm-style installs (they put a `.cmd` shim on PATH), so a missed
/// spawn falls through to the shim; the reported error stays the caller's.
fn spawn_agent(program: &str, args: &[String]) -> std::io::Result<smol::process::Child> {
    let spawn = |program: &str| {
        let mut std_cmd = StdCommand::new(program);
        hide_console(strip_provider_keys(&mut std_cmd)).args(args);

        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                let _ = rustix::process::setsid();
                Ok(())
            });
        }

        let mut cmd: Command = std_cmd.into();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn()
    };
    match spawn(program) {
        Ok(child) => Ok(child),
        Err(first) if cfg!(windows) && !program.contains(['\\', '/', '.']) => {
            match spawn(&format!("{program}.cmd")) {
                Ok(child) => Ok(child),
                Err(_) => Err(first),
            }
        }
        Err(first) => Err(first),
    }
}

fn died(router: &Router) -> AcpClientError {
    AcpClientError::ServerDied {
        name: Arc::clone(&router.name),
    }
}

async fn write(shared: &Arc<Shared>, message: &impl Serialize) -> Result<(), AcpClientError> {
    if !shared.router.alive.load(Ordering::Acquire) {
        return Err(died(&shared.router));
    }
    let mut buf = serde_json::to_vec(message)?;
    buf.push(NEWLINE);
    let mut stdin = shared.stdin.lock().await;
    stdin
        .write_all(&buf)
        .await
        .map_err(|e| AcpClientError::Write {
            name: Arc::clone(&shared.router.name),
            reason: e.to_string(),
        })?;
    stdin.flush().await.map_err(|e| AcpClientError::Write {
        name: Arc::clone(&shared.router.name),
        reason: e.to_string(),
    })
}

/// Routes one agent line. Replies the client owes (permission decisions,
/// errors for methods it does not implement) land in `replies` for the caller
/// to write, so routing itself stays sync and testable without a process.
pub(crate) fn handle_message(router: &Router, line: &str, replies: &mut Vec<Value>) {
    let raw: Value = match serde_json::from_str(line) {
        Ok(raw) => raw,
        Err(e) => {
            debug!(agent = &*router.name, error = %e, "invalid JSON from agent");
            return;
        }
    };
    let id = raw.get("id");
    if raw.get("result").is_some() || raw.get("error").is_some() {
        let Some(id) = id.and_then(Value::as_i64) else {
            debug!(agent = &*router.name, "response without a known id");
            return;
        };
        let sender = router.pending.lock().unwrap().remove(&id);
        let Some(sender) = sender else {
            debug!(agent = &*router.name, id, "response for a dropped request");
            return;
        };
        let result = if let Some(error) = raw.get("error") {
            Err(AcpClientError::Agent {
                code: error
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        } else {
            Ok(raw.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = sender.send(result);
    } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
        match id {
            Some(id) => handle_request(router, method, id, &raw, replies),
            None => handle_notification(router, method, &raw),
        }
    }
}

fn handle_request(
    router: &Router,
    method: &str,
    id: &Value,
    raw: &Value,
    replies: &mut Vec<Value>,
) {
    let id: RequestId = serde_json::from_value(id.clone()).unwrap_or(RequestId::Null);
    let result: Result<Value, AcpError> = match method {
        SESSION_REQUEST_PERMISSION_METHOD => {
            match serde_json::from_value::<RequestPermissionRequest>(
                raw.get("params").cloned().unwrap_or(Value::Null),
            ) {
                Ok(request) => serde_json::to_value(RequestPermissionResponse::new(
                    permission_outcome(&request, router.policy),
                ))
                .map_err(|e| AcpError::internal_error().data(Value::from(e.to_string()))),
                Err(e) => Err(AcpError::invalid_params().data(Value::from(e.to_string()))),
            }
        }
        _ => Err(AcpError::method_not_found()),
    };
    replies.push(
        serde_json::to_value(JsonRpcMessage::wrap(Response::new(id, result)))
            .expect("response envelope serializes"),
    );
}

fn handle_notification(router: &Router, method: &str, raw: &Value) {
    match method {
        SESSION_UPDATE_METHOD => {
            match serde_json::from_value::<SessionNotification>(
                raw.get("params").cloned().unwrap_or(Value::Null),
            ) {
                Ok(note) => {
                    let sender = router
                        .sessions
                        .lock()
                        .unwrap()
                        .get(note.session_id.0.as_ref())
                        .cloned();
                    if let Some(sender) = sender {
                        let _ = sender.send(note.update);
                    }
                }
                Err(e) => debug!(agent = &*router.name, error = %e, "bad session/update"),
            }
        }
        _ => debug!(agent = &*router.name, method, "unknown notification"),
    }
}

fn permission_outcome(
    request: &RequestPermissionRequest,
    policy: PermissionPolicy,
) -> RequestPermissionOutcome {
    match policy {
        PermissionPolicy::Cancel => RequestPermissionOutcome::Cancelled,
        PermissionPolicy::Allow => select_option(&request.options, PermissionOptionKind::AllowOnce)
            .or_else(|| select_option(&request.options, PermissionOptionKind::AllowAlways))
            .unwrap_or(RequestPermissionOutcome::Cancelled),
        PermissionPolicy::Deny => select_option(&request.options, PermissionOptionKind::RejectOnce)
            .or_else(|| select_option(&request.options, PermissionOptionKind::RejectAlways))
            .unwrap_or(RequestPermissionOutcome::Cancelled),
    }
}

fn select_option(
    options: &[PermissionOption],
    kind: PermissionOptionKind,
) -> Option<RequestPermissionOutcome> {
    options
        .iter()
        .find(|option| option.kind == kind)
        .map(|option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    const NAME: &str = "test";
    const SESSION: &str = "ses_1";

    struct TestRouter {
        router: Router,
        updates: flume::Receiver<SessionUpdate>,
        pending_rx: flume::Receiver<Result<Value, AcpClientError>>,
    }

    impl TestRouter {
        fn new(policy: PermissionPolicy) -> Self {
            let (updates_tx, updates) = flume::unbounded();
            let (pending_tx, pending_rx) = flume::bounded(1);
            let mut sessions = HashMap::new();
            sessions.insert(Arc::from(SESSION), updates_tx);
            let router = Router {
                name: Arc::from(NAME),
                pending: StdMutex::new(HashMap::from([(7, pending_tx)])),
                sessions: StdMutex::new(sessions),
                alive: Arc::new(AtomicBool::new(true)),
                policy,
            };
            Self {
                router,
                updates,
                pending_rx,
            }
        }

        fn route(&self, line: &str) -> Vec<Value> {
            let mut replies = Vec::new();
            handle_message(&self.router, line, &mut replies);
            replies
        }
    }

    #[test]
    fn response_resolves_the_matching_request() {
        smol::block_on(async {
            let client = TestRouter::new(PermissionPolicy::Deny);
            client.route(r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}"#);
            let result = client.pending_rx.recv_async().await.unwrap().unwrap();
            assert_eq!(result["stopReason"], "end_turn");
        });
    }

    #[test]
    fn rpc_error_becomes_an_agent_error() {
        smol::block_on(async {
            let client = TestRouter::new(PermissionPolicy::Deny);
            client.route(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32000,"message":"busy"}}"#);
            match client.pending_rx.recv_async().await.unwrap() {
                Err(AcpClientError::Agent { code, message }) => {
                    assert_eq!(code, -32000);
                    assert_eq!(message, "busy");
                }
                other => panic!("expected an agent error, got {other:?}"),
            }
        });
    }

    #[test]
    fn session_update_routes_to_its_session() {
        let client = TestRouter::new(PermissionPolicy::Deny);
        client.route(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": SESSION,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "hi"},
                    },
                },
            })
            .to_string(),
        );
        match client.updates.recv().unwrap() {
            SessionUpdate::AgentMessageChunk(chunk) => {
                assert_eq!(chunk.content, ContentBlock::Text(TextContent::new("hi")));
            }
            other => panic!("expected a message chunk, got {other:?}"),
        }
    }

    #[test]
    fn update_for_an_unknown_session_is_dropped() {
        let client = TestRouter::new(PermissionPolicy::Deny);
        client.route(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "ses_other",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "hi"},
                    },
                },
            })
            .to_string(),
        );
        assert!(client.updates.try_recv().is_err());
    }

    #[test_case("allow", "selected", "allow_once" ; "allow_picks_allow_once")]
    #[test_case("deny", "selected", "reject_once" ; "deny_picks_reject_once")]
    #[test_case("cancel", "cancelled", "" ; "cancel_refuses_the_turn")]
    fn permission_request_answers_from_the_policy(policy: &str, outcome: &str, picked: &str) {
        let client = TestRouter::new(PermissionPolicy::parse(policy).unwrap());
        let replies = client.route(
            &json!({
                "jsonrpc": "2.0",
                "id": "ask-1",
                "method": "session/request_permission",
                "params": {
                    "sessionId": SESSION,
                    "toolCall": {"toolCallId": "call_1", "title": "bash"},
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            })
            .to_string(),
        );
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], "ask-1");
        assert_eq!(replies[0]["result"]["outcome"]["outcome"], outcome);
        if outcome != "cancelled" {
            assert_eq!(replies[0]["result"]["outcome"]["optionId"], picked);
        }
    }

    #[test]
    fn deny_falls_back_to_cancelled_without_a_reject_option() {
        let request: RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": SESSION,
            "toolCall": {"toolCallId": "call_1", "title": "bash"},
            "options": [{"optionId": "a", "name": "Allow", "kind": "allow_once"}]
        }))
        .unwrap();
        assert_eq!(
            permission_outcome(&request, PermissionPolicy::Deny),
            RequestPermissionOutcome::Cancelled
        );
    }

    #[test]
    fn unknown_agent_request_gets_method_not_found() {
        let client = TestRouter::new(PermissionPolicy::Deny);
        let replies =
            client.route(r#"{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{}}"#);
        assert_eq!(replies[0]["error"]["code"], -32601);
    }

    #[test_case("allow", PermissionPolicy::Allow)]
    #[test_case("deny", PermissionPolicy::Deny)]
    #[test_case("cancel", PermissionPolicy::Cancel)]
    fn policy_round_trips_through_strings(key: &str, policy: PermissionPolicy) {
        assert_eq!(PermissionPolicy::parse(key), Some(policy));
        assert_eq!(policy.as_str(), key);
        assert_eq!(PermissionPolicy::parse("nonsense"), None);
    }

    /// Runs `opencode` from PATH through a full turn. Ignored by default: it
    /// needs the binary (plus its own model config) and on Windows it is the
    /// only coverage the `.cmd` shim fallback in [`spawn_agent`] gets.
    #[test]
    #[ignore = "needs `opencode` on PATH with a configured model"]
    fn live_opencode_round_trip() {
        smol::block_on(async {
            let (client, _info) = AcpClient::start(
                "opencode",
                "opencode",
                &["acp".to_string()],
                PermissionPolicy::Allow,
            )
            .await
            .expect("opencode acp starts");
            let session = client.new_session(None).await.expect("session/new");
            let result = session
                .prompt("Reply with exactly: ACP-OK", None)
                .await
                .expect("session/prompt");
            assert_eq!(result.stop_reason, StopReason::EndTurn);
            assert!(
                result.text.to_lowercase().contains("acp-ok"),
                "unexpected reply: {}",
                result.text
            );
        });
    }

    /// The same live session, aborted mid-turn: `session/cancel` must park the
    /// prompt with [`StopReason::Cancelled`] instead of an error.
    #[test]
    #[ignore = "needs `opencode` on PATH with a configured model"]
    fn live_opencode_cancel_stops_the_turn() {
        smol::block_on(async {
            let (client, _info) = AcpClient::start(
                "opencode",
                "opencode",
                &["acp".to_string()],
                PermissionPolicy::Allow,
            )
            .await
            .expect("opencode acp starts");
            let session = Arc::new(client.new_session(None).await.expect("session/new"));
            let canceller = Arc::clone(&session);
            let turn = smol::spawn(async move {
                session
                    .prompt("Count from 1 to 200, one number per line.", None)
                    .await
            });
            smol::Timer::after(Duration::from_secs(5)).await;
            canceller.cancel().await.expect("session/cancel");
            let result = turn.await.expect("cancelled prompt still returns");
            assert_eq!(result.stop_reason, StopReason::Cancelled);
        });
    }
}

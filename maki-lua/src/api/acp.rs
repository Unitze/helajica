//! maki.acp: drive an external agent process speaking the Agent Client
//! Protocol (e.g. `opencode acp`) as a delegate. One client is one process
//! hosting any number of sessions; the plugin that registers the tool decides
//! how to use them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use maki_agent::acp::{
    AcpClient, AcpClientError, AcpSession, InitializeResponse, PermissionPolicy, SessionUpdate,
    TurnResult, UpdateHandler,
};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{AnyUserData, Function, Lua, Result as LuaResult, Table, UserDataRef};

use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const CMD_REQUIRED_ERR: &str = "maki.acp.client: 'cmd' is required";
const POLICY_ERR: &str = "maki.acp.client: 'policy' must be 'allow', 'deny', or 'cancel'";

pub(crate) struct LuaAcpClient {
    client: AcpClient,
    info: InitializeResponse,
}

pub(crate) struct LuaAcpSession {
    session: Arc<AcpSession>,
}

fn acp_err(error: AcpClientError) -> String {
    error.to_string()
}

/// Start one ACP agent process and finish its `initialize` handshake. Keep the
/// returned client around: every {client.new_session} runs in this process,
/// and dropping the last reference kills it.
///
/// The agent works with its own tools, models and permissions; maki only pipes
/// prompts in and final text out. Give it room to work with {policy}.
///
/// @param opts table Required fields:
///   `cmd` (string) - executable to start.
/// Optional fields:
///   `args` (string[]?) - command line arguments (e.g. `{"acp"}`).
///   `policy` (string?) - how to answer the agent's permission requests:
///     "allow" runs unattended, "deny" refuses each call and lets the turn
///     continue, "cancel" refuses the whole turn. Defaults to "deny".
/// @return (Client?, string?) The client, plus an error message on failure.
/// @example
/// local client, err = maki.acp.client({ cmd = "opencode", args = { "acp" }, policy = "allow" })
/// if err then error(err) end
#[lua_fn(guard = Run, name = "client")]
async fn acp_client(lua: Lua, opts: Table) -> LuaResult<Pair<AnyUserData>> {
    let cmd: String = match opts.get("cmd") {
        Ok(cmd) => cmd,
        Err(_) => return Ok(err_pair(CMD_REQUIRED_ERR)),
    };
    let args: Vec<String> = opts.get("args").ok().flatten().unwrap_or_default();
    let policy = match opts.get::<Option<String>>("policy")? {
        None => PermissionPolicy::Deny,
        Some(key) => {
            PermissionPolicy::parse(&key).ok_or_else(|| mlua::Error::runtime(POLICY_ERR))?
        }
    };
    let (client, info) = try_pair!(
        AcpClient::start(&cmd, &cmd, &args, policy)
            .await
            .map_err(acp_err)
    );
    let ud = lua.create_userdata(LuaAcpClient { client, info })?;
    Ok((Some(ud), None))
}

/// Open a conversation in {client}'s process. Independent sessions share the
/// process but nothing else.
///
/// @param opts table? Optional fields:
///   `cwd` (string?) - absolute working directory for the session. Defaults
///     to maki's working directory.
/// @return (Session?, string?) The new session, plus an error message on failure.
/// @example
/// local session, err = client:new_session()
/// if err then error(err) end
#[lua_fn]
async fn new_session(
    lua: Lua,
    this: UserDataRef<LuaAcpClient>,
    opts: Option<Table>,
) -> LuaResult<Pair<AnyUserData>> {
    let cwd = match opts {
        Some(table) => table.get::<Option<String>>("cwd")?,
        None => None,
    };
    let client = this.client.clone();
    drop(this);
    match client.new_session(cwd.map(PathBuf::from)).await {
        Ok(session) => {
            let ud = lua.create_userdata(LuaAcpSession {
                session: Arc::new(session),
            })?;
            Ok((Some(ud), None))
        }
        Err(e) => Ok(err_pair(acp_err(e))),
    }
}

/// The agent's name and version from the `initialize` handshake.
///
/// @return (table) `{ name = string?, version = string? }`.
/// @example
/// local info = client:info()
#[lua_fn]
fn info(lua: &Lua, this: &LuaAcpClient) -> LuaResult<Pair<Table>> {
    let out = lua.create_table()?;
    if let Some(implementation) = &this.info.agent_info {
        out.set("name", implementation.name.clone())?;
        out.set("version", implementation.version.clone())?;
    }
    Ok((Some(out), None))
}

/// Send one message and wait out the turn. The result carries the agent's
/// final text and why the turn ended. Updates stream to {opts.on_update} as
/// they arrive, as wire-shaped tables (`sessionUpdate` names the kind).
///
/// @param text string The user message for the agent.
/// @param opts table? Optional fields:
///   `on_update` (function?) - called with each `session/update` table. Must
///     not yield.
/// @return (table?, string?) `{ text = string, stop_reason = string }` (
///   "end_turn", "max_tokens", "refusal", or "cancelled"), plus an error
///   message on failure.
/// @example
/// local result, err = session:prompt("Fix the failing test in foo.rs", {
///   on_update = function(update)
///     if update.sessionUpdate == "tool_call" then print(update.title) end
///   end,
/// })
/// if err then error(err) end
/// print(result.text)
#[lua_fn]
async fn prompt(
    lua: Lua,
    this: UserDataRef<LuaAcpSession>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let on_update = match opts {
        Some(table) => table.get::<Option<Function>>("on_update")?,
        None => None,
    };
    let session = Arc::clone(&this.session);
    drop(this);
    let cb_error: Arc<StdMutex<Option<String>>> = Arc::default();
    let callback: Option<UpdateHandler> = on_update.map(|f| {
        let lua = lua.clone();
        let cb_error = Arc::clone(&cb_error);
        let cb: UpdateHandler = Box::new(move |update: &SessionUpdate| {
            if let Ok(value) = serde_json::to_value(update)
                && let Ok(table) = json_to_lua(&lua, &value)
                && let Err(e) = f.call::<()>(table)
                && cb_error.lock().unwrap().is_none()
            {
                *cb_error.lock().unwrap() = Some(e.to_string());
            }
        });
        cb
    });
    let result: Result<TurnResult, AcpClientError> = session.prompt(&text, callback).await;
    if let Some(e) = cb_error.lock().unwrap().take() {
        return Ok(err_pair(e));
    }
    let turn = try_pair!(result.map_err(acp_err));
    let out = lua.create_table()?;
    out.set("text", turn.text)?;
    out.set(
        "stop_reason",
        serde_json::to_value(turn.stop_reason)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
    )?;
    Ok((Some(out), None))
}

/// Pick a `select` config option the agent offered on {session.new_session}
/// (e.g. `("model", "opencode/grok-code")`).
///
/// @param config_id string The option id from the session's config options.
/// @param value_id string The option value id to select.
/// @return (boolean?, string?) True on success, plus an error message on failure.
/// @example
/// local ok, err = session:set_config("model", "opencode/grok-code")
#[lua_fn]
async fn set_config(
    _lua: Lua,
    this: UserDataRef<LuaAcpSession>,
    config_id: String,
    value_id: String,
) -> LuaResult<Pair<bool>> {
    let session = Arc::clone(&this.session);
    drop(this);
    Ok(pair(
        session
            .set_config(&config_id, &value_id)
            .await
            .map_err(acp_err)
            .map(|()| true),
    ))
}

/// Switch the session mode the agent advertised (e.g. "build" or "plan").
///
/// @param mode string The mode id.
/// @return (boolean?, string?) True on success, plus an error message on failure.
/// @example
/// local ok, err = session:set_mode("plan")
#[lua_fn]
async fn set_mode(
    _lua: Lua,
    this: UserDataRef<LuaAcpSession>,
    mode: String,
) -> LuaResult<Pair<bool>> {
    let session = Arc::clone(&this.session);
    drop(this);
    Ok(pair(
        session
            .set_mode(&mode)
            .await
            .map_err(acp_err)
            .map(|()| true),
    ))
}

/// Ask the agent to abort the running turn. The parked {session.prompt} then
/// returns with `stop_reason = "cancelled"`, not an error.
///
/// @return
/// @example
/// session:cancel()
#[lua_fn]
async fn cancel(_lua: Lua, this: UserDataRef<LuaAcpSession>) -> LuaResult<()> {
    let session = Arc::clone(&this.session);
    drop(this);
    session.cancel().await.map_err(mlua::Error::external)
}

lua_class! {
    /// A conversation in an ACP agent process. Send messages with `:prompt()`,
    /// cancel the running turn with `:cancel()`. The session remembers previous
    /// turns, so you can hold a conversation. Dropping it ends it.
    "maki.acp.Session" => LuaAcpSession, SESSION_DOCS [prompt, set_config, set_mode, cancel]
}

lua_class! {
    /// A live ACP agent process, started with `maki.acp.client()`. Sessions
    /// opened with `:new_session()` share it. Dropping the last reference
    /// kills the process.
    "maki.acp.Client" => LuaAcpClient, CLIENT_DOCS [new_session, info]
}

lua_table! {
    /// Drive an external agent process that speaks the Agent Client Protocol
    /// as a delegate. One client is one process hosting many sessions; the
    /// agent brings its own tools, models and permissions.
    ///
    /// ```lua
    /// local client, err = maki.acp.client({ cmd = "opencode", args = { "acp" } })
    /// if err then error(err) end
    /// local session, err = client:new_session()
    /// if err then error(err) end
    /// local result, err = session:prompt("Summarize the repository structure")
    /// if err then error(err) end
    /// print(result.text)
    /// ```
    "maki.acp" => pub(crate) fn create_acp_table(perms: &PluginPermissions), DOCS [
        acp_client(perms),
    ]
}

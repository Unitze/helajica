//! A topic bus between Lua plugins and the desktop GUI (`maki --gui`).
//!
//! The GUI host attaches an outbox once at startup. Without one every
//! `emit` reports that nothing is listening, so a plugin written for the GUI
//! keeps working, silently, under the TUI.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use flume::{Receiver, Sender};
use maki_agent::cancel::CancelToken;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Value};
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::util::convert::{json_to_lua, lua_to_json};
use super::util::pair::{Pair, err_pair, try_pair};
use crate::runtime::{TaskHandle, lock_cell};

const NO_GUI_ERR: &str = "no gui attached";
const GUI_CLOSED_ERR: &str = "gui closed";

#[derive(Debug, Clone, Serialize)]
pub struct GuiMessage {
    pub topic: String,
    pub data: JsonValue,
}

type Topic = (Sender<JsonValue>, Receiver<JsonValue>);

static OUTBOX: OnceLock<Sender<GuiMessage>> = OnceLock::new();
static INBOX: OnceLock<Mutex<HashMap<String, Topic>>> = OnceLock::new();

/// Routes every later `maki.gui.emit` to `tx`. Only the first attach wins:
/// one process hosts at most one GUI.
pub fn attach_gui(tx: Sender<GuiMessage>) {
    let _ = OUTBOX.set(tx);
}

/// Hands a GUI message to whichever plugin waits on `topic`. Queued until one
/// does, so a plugin that starts listening late still sees it.
pub fn deliver_to_lua(topic: &str, data: JsonValue) {
    let _ = inbox_topic(topic).0.send(data);
}

fn inbox_topic(topic: &str) -> Topic {
    let inbox = INBOX.get_or_init(Default::default);
    let mut topics = inbox.lock().unwrap_or_else(|e| e.into_inner());
    topics
        .entry(topic.to_owned())
        .or_insert_with(flume::unbounded)
        .clone()
}

/// Whether a desktop GUI is attached to this process.
///
/// @return (boolean) true inside the Helajica desktop app (`maki gui`).
/// @example
/// if maki.gui.active() then
///   maki.gui.emit("my-plugin/status", { ok = true })
/// end
#[lua_fn]
fn active(_lua: &Lua) -> LuaResult<bool> {
    Ok(OUTBOX.get().is_some())
}

/// Send {data} to the GUI under {topic}. The reserved `ui.*` topics draw:
/// `ui.panel` (a sidebar panel), `ui.status` (a status item by the input),
/// `ui.command` (a palette command, answered on `ui.command.<id>`),
/// `ui.notify`, `ui.prompt` and `ui.theme`. Re-emit with the same `id` to
/// redraw; `{ id = ..., remove = true }` takes an entry away.
///
/// @param topic string A `ui.*` topic, or one namespaced with your plugin name.
/// @param data any JSON-serializable value.
/// @return (boolean?, string?) true, or nil plus an error when no GUI is attached.
/// @example
/// maki.gui.emit("ui.status", { id = "git", text = "main" })
#[lua_fn]
fn emit(lua: &Lua, topic: String, data: Value) -> LuaResult<Pair<bool>> {
    let Some(outbox) = OUTBOX.get() else {
        return Ok(err_pair(NO_GUI_ERR));
    };
    let data = try_pair!(lua_to_json(lua, &data));
    Ok(match outbox.send(GuiMessage { topic, data }) {
        Ok(()) => (Some(true), None),
        Err(_) => err_pair(GUI_CLOSED_ERR),
    })
}

/// Wait for the next message the GUI sends under {topic}, such as a click on
/// a panel button (its `topic` and `data`) or a palette command
/// (`ui.command.<id>`). Messages queue until read. Suspends the calling task, so run it inside
/// `maki.async.spawn`. Cancelling the task ends the wait with an error.
///
/// @param topic string Topic to listen on.
/// @return (any?, string?) The message data, or nil plus an error.
/// @example
/// maki.async.spawn(function()
///   while true do
///     maki.gui.recv("ui.command.git-refresh")
///     maki.gui.emit("ui.status", { id = "git", text = current_branch() })
///   end
/// end)
#[lua_fn]
async fn recv(lua: Lua, topic: String) -> LuaResult<Pair<Value>> {
    let cancel = lua
        .app_data_ref::<TaskHandle>()
        .map(|h| lock_cell(&h).cancel.clone())
        .unwrap_or_else(CancelToken::none);
    let rx = inbox_topic(&topic).1;
    let data = try_pair!(cancel.race(rx.recv_async()).await);
    let data = try_pair!(data);
    Ok((Some(json_to_lua(&lua, &data)?), None))
}

lua_table! {
    /// Message bus between Lua plugins and the Helajica desktop app (`maki gui`).
    ///
    /// Topics are free-form strings; namespace them with your plugin name.
    /// Under the TUI `active()` is false and `emit` returns an error pair.
    ///
    /// ```lua
    /// maki.gui.emit("ui.notify", { message = "hi", level = "success" })
    /// ```
    "maki.gui" => pub(crate) fn create_gui_table(), DOCS [
        active, emit, recv,
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TOPIC: &str = "test/topic";

    #[test]
    fn delivered_messages_wait_for_a_listener() {
        deliver_to_lua(TOPIC, json!({ "n": 1 }));
        let (_, rx) = inbox_topic(TOPIC);
        assert_eq!(rx.try_recv().unwrap(), json!({ "n": 1 }));
    }
}

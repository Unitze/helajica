//! What Lua plugins put on screen. A plugin emits a reserved `ui.*` topic with
//! `maki.gui.emit`, the GUI folds it in here, and the views draw the result.
//! Clicks travel back as `maki.gui.recv(topic)` messages, so a plugin owns its
//! whole loop without a line of Rust.

use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

pub const PANEL: &str = "ui.panel";
pub const STATUS: &str = "ui.status";
pub const COMMAND: &str = "ui.command";
pub const NOTIFY: &str = "ui.notify";
pub const PROMPT: &str = "ui.prompt";
pub const THEME: &str = "ui.theme";
/// Prefix of the topic a palette command answers on: `ui.command.<id>`.
pub const COMMAND_TOPIC_PREFIX: &str = "ui.command.";

/// A declarative element tree. Small on purpose: plugins compose these, and
/// anything richer belongs in the GUI itself.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    Text {
        text: String,
        #[serde(default)]
        muted: bool,
    },
    Markdown {
        text: String,
    },
    Code {
        text: String,
    },
    Badge {
        text: String,
    },
    Separator,
    Progress {
        value: f32,
    },
    Button {
        label: String,
        topic: String,
        #[serde(default)]
        data: Value,
        #[serde(default)]
        variant: ButtonVariant,
    },
    Row {
        #[serde(default)]
        children: Vec<Node>,
    },
    Col {
        #[serde(default)]
        children: Vec<Node>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonVariant {
    #[default]
    Outline,
    Primary,
    Ghost,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Panel {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StatusItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tooltip: String,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PaletteCommand {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// Something a topic asks of the window rather than of the extension state.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Notify {
        message: String,
        level: NotifyLevel,
    },
    Prompt {
        text: String,
        send: bool,
    },
    Theme(Value),
    /// Not a `ui.*` topic: left for whoever listens to it.
    Unhandled,
}

#[derive(Default)]
pub struct Extensions {
    pub panels: Vec<Panel>,
    pub status: Vec<StatusItem>,
    pub commands: Vec<PaletteCommand>,
}

#[derive(Deserialize)]
struct Removal {
    id: String,
    #[serde(default)]
    remove: bool,
}

#[derive(Deserialize)]
struct Notify {
    message: String,
    #[serde(default)]
    level: NotifyLevel,
}

#[derive(Deserialize)]
struct Prompt {
    text: String,
    #[serde(default)]
    send: bool,
}

impl Extensions {
    pub fn apply(&mut self, topic: &str, data: Value) -> Effect {
        let result = match topic {
            PANEL => upsert(&mut self.panels, data, |p| &p.id),
            STATUS => upsert(&mut self.status, data, |s| &s.id),
            COMMAND => upsert(&mut self.commands, data, |c| &c.id),
            NOTIFY => serde_json::from_value::<Notify>(data).map(|n| Effect::Notify {
                message: n.message,
                level: n.level,
            }),
            PROMPT => serde_json::from_value::<Prompt>(data).map(|p| Effect::Prompt {
                text: p.text,
                send: p.send,
            }),
            THEME => Ok(Effect::Theme(data)),
            _ => Ok(Effect::Unhandled),
        };
        result.unwrap_or_else(|e| {
            warn!(topic, error = %e, "malformed gui extension message");
            Effect::None
        })
    }
}

/// `{ id, remove = true }` drops an entry; anything else replaces it in place
/// or appends it, so a plugin re-emits a panel to redraw it.
fn upsert<T: for<'de> Deserialize<'de>>(
    entries: &mut Vec<T>,
    data: Value,
    id: impl Fn(&T) -> &String,
) -> serde_json::Result<Effect> {
    if let Ok(Removal {
        id: gone,
        remove: true,
    }) = serde_json::from_value(data.clone())
    {
        entries.retain(|e| *id(e) != gone);
        return Ok(Effect::None);
    }
    let entry: T = serde_json::from_value(data)?;
    match entries.iter_mut().find(|e| id(e) == id(&entry)) {
        Some(slot) => *slot = entry,
        None => entries.push(entry),
    }
    Ok(Effect::None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const PANEL_ID: &str = "git";

    #[test]
    fn clickable_status_preserves_its_action_and_tooltip() {
        let mut ext = Extensions::default();
        ext.apply(STATUS, json!({ "id": "jobs", "text": "1 shell", "tooltip": "cargo check", "topic": "bash.jobs.toggle", "data": { "job": 1 } }));
        assert_eq!(ext.status[0].topic.as_deref(), Some("bash.jobs.toggle"));
        assert_eq!(ext.status[0].tooltip, "cargo check");
        assert_eq!(ext.status[0].data["job"], 1);
        ext.apply(STATUS, json!({ "id": "jobs", "text": "idle" }));
        assert!(ext.status[0].topic.is_none());
    }

    #[test]
    fn panels_replace_by_id_and_remove() {
        let mut ext = Extensions::default();
        ext.apply(
            PANEL,
            json!({ "id": PANEL_ID, "title": "Git", "body": [{ "type": "separator" }] }),
        );
        ext.apply(PANEL, json!({ "id": PANEL_ID, "title": "Git (2)" }));
        assert_eq!(ext.panels.len(), 1);
        assert_eq!(ext.panels[0].title, "Git (2)");
        ext.apply(PANEL, json!({ "id": PANEL_ID, "remove": true }));
        assert!(ext.panels.is_empty());
    }

    #[test]
    fn nested_nodes_parse() {
        let node: Node = serde_json::from_value(json!({
            "type": "row",
            "children": [
                { "type": "text", "text": "branch" },
                { "type": "button", "label": "Pull", "topic": "git/pull", "variant": "primary" },
            ],
        }))
        .unwrap();
        let Node::Row { children } = node else {
            panic!("expected a row")
        };
        assert!(matches!(
            &children[1],
            Node::Button {
                variant: ButtonVariant::Primary,
                ..
            }
        ));
    }

    #[test_case(NOTIFY, json!({ "message": "hi" }), true ; "notify")]
    #[test_case(PROMPT, json!({ "text": "go", "send": true }), true ; "prompt")]
    #[test_case("my-plugin/thing", json!(null), false ; "foreign_topic")]
    fn effects_reach_the_window(topic: &str, data: Value, handled: bool) {
        let effect = Extensions::default().apply(topic, data);
        assert_eq!(effect != Effect::Unhandled, handled);
    }
}

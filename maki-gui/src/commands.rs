//! Slash commands typed into the composer: the window's own, and the custom
//! commands maki found for the project. The window runs its own; the rest go
//! to the agent, which expands them the way the TUI does.

const PREFIX: char = '/';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    New,
    Btw,
    Goal,
    Model,
    Plan,
    Ask,
    Yolo,
    Stop,
    Settings,
    Theme,
    Help,
    Exit,
}

impl Builtin {
    pub const ALL: [Self; 12] = [
        Self::New,
        Self::Btw,
        Self::Goal,
        Self::Model,
        Self::Plan,
        Self::Ask,
        Self::Yolo,
        Self::Stop,
        Self::Settings,
        Self::Theme,
        Self::Help,
        Self::Exit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Btw => "btw",
            Self::Goal => "goal",
            Self::Model => "model",
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::Yolo => "yolo",
            Self::Stop => "stop",
            Self::Settings => "settings",
            Self::Theme => "theme",
            Self::Help => "help",
            Self::Exit => "exit",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::New => "Start a new session",
            Self::Btw => "Ask a side question: no tools, nothing added to the session",
            Self::Goal => "Keep working until an objective is met (/goal clear stops it)",
            Self::Model => "Switch model",
            Self::Plan => "Plan mode: plan before editing",
            Self::Ask => "Ask before edits",
            Self::Yolo => "Toggle bypass permissions",
            Self::Stop => "Stop the current run",
            Self::Settings => "Open settings",
            Self::Theme => "Toggle dark mode",
            Self::Help => "List the commands",
            Self::Exit => "Quit Helajica",
        }
    }

    /// What the command takes after its name, shown next to it.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Btw => "<question>",
            Self::Goal => "<objective>",
            Self::Model => "[model]",
            _ => "",
        }
    }

    fn takes_args(self) -> bool {
        !self.hint().is_empty()
    }

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.name() == name)
    }
}

/// `/name args`, split. `None` for anything that is not a command.
pub fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix(PREFIX)?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name, args.trim()))
}

pub fn builtin(text: &str) -> Option<(Builtin, &str)> {
    let (name, args) = split(text)?;
    Some((Builtin::named(name)?, args))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub name: String,
    pub hint: String,
    pub description: String,
    pub takes_args: bool,
}

impl Suggestion {
    pub fn completion(&self) -> String {
        match self.takes_args {
            true => format!("{PREFIX}{} ", self.name),
            false => format!("{PREFIX}{}", self.name),
        }
    }
}

/// While the input is a bare `/word`, the commands it could become: names
/// that start with it first, then names that merely contain it.
pub fn suggest(text: &str, custom: &[(String, String)]) -> Vec<Suggestion> {
    let Some(query) = text.strip_prefix(PREFIX) else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let all = Builtin::ALL
        .into_iter()
        .map(|b| Suggestion {
            name: b.name().to_owned(),
            hint: b.hint().to_owned(),
            description: b.description().to_owned(),
            takes_args: b.takes_args(),
        })
        .chain(custom.iter().map(|(name, description)| Suggestion {
            name: name.clone(),
            hint: String::new(),
            description: description.clone(),
            takes_args: true,
        }));
    let (mut prefixed, mut containing): (Vec<_>, Vec<_>) = all
        .filter(|s| s.name.to_lowercase().contains(&query))
        .partition(|s| s.name.to_lowercase().starts_with(&query));
    prefixed.append(&mut containing);
    prefixed
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const REVIEW: &str = "review";
    const REVIEW_DESCRIPTION: &str = "Review a file";

    fn custom() -> Vec<(String, String)> {
        vec![(REVIEW.into(), REVIEW_DESCRIPTION.into())]
    }

    #[test_case("/btw why sqlite?", Some((Builtin::Btw, "why sqlite?")) ; "with_args")]
    #[test_case("  /new", Some((Builtin::New, "")) ; "leading_space")]
    #[test_case("/review x", None ; "custom_is_not_builtin")]
    #[test_case("hello /new", None ; "not_at_start")]
    #[test_case("/", None ; "bare_slash")]
    fn finds_builtins(text: &str, expected: Option<(Builtin, &str)>) {
        assert_eq!(builtin(text), expected);
    }

    #[test_case("/", 13 ; "everything")]
    #[test_case("/go", 1 ; "prefix")]
    #[test_case("/btw now", 0 ; "args_close_the_list")]
    #[test_case("plain", 0 ; "not_a_command")]
    #[test_case("/zzz", 0 ; "no_match")]
    fn suggests_while_typing_a_name(text: &str, expected: usize) {
        assert_eq!(suggest(text, &custom()).len(), expected);
    }

    #[test]
    fn prefix_matches_come_first() {
        let names: Vec<String> = suggest("/e", &custom())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "exit", "new", "model", "settings", "theme", "help", "review"
            ]
        );
    }

    #[test_case(Builtin::Btw, "/btw " ; "takes_args")]
    #[test_case(Builtin::New, "/new" ; "no_args")]
    fn completes_with_room_for_args(command: Builtin, expected: &str) {
        let suggestion = suggest(&format!("/{}", command.name()), &[])
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(suggestion.completion(), expected);
    }
}

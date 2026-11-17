//! What a transcript's tool rows show: the call's input, like the command
//! or the path, or the description the agent wrote for the call, where it
//! wrote one. Any built-in tool's `description` counts, but not an MCP
//! tool's: those often use the field for content, like an issue's body.

use leptos::prelude::*;

use crate::inboxes::storage;

const KEY: &str = "tool-summaries";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolSummaries {
    Input,
    Description,
}

impl ToolSummaries {
    pub fn parse(s: &str) -> ToolSummaries {
        match s {
            "description" => ToolSummaries::Description,
            _ => ToolSummaries::Input,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ToolSummaries::Input => "input",
            ToolSummaries::Description => "description",
        }
    }
}

/// The selected choice, persisted in localStorage.
pub fn signal() -> RwSignal<ToolSummaries> {
    let choice = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(KEY).ok().flatten())
            .map_or(ToolSummaries::Input, |v| ToolSummaries::parse(&v)),
    );
    Effect::new(move |_| {
        let choice = choice.get();
        if let Some(s) = storage() {
            let _ = s.set_item(KEY, choice.as_str());
        }
    });
    choice
}

/// The first line of a tool call's `description`, trimmed, or `None` when it
/// has no description to show or is an MCP tool's.
pub fn description<'a>(name: &str, input: &'a serde_json::Value) -> Option<&'a str> {
    if name.starts_with("mcp__") {
        return None;
    }
    let line = input["description"].as_str()?.lines().next()?.trim();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips() {
        for choice in [ToolSummaries::Input, ToolSummaries::Description] {
            assert_eq!(ToolSummaries::parse(choice.as_str()), choice);
        }
    }

    #[test]
    fn parse_unknown_is_input() {
        assert_eq!(ToolSummaries::parse(""), ToolSummaries::Input);
        assert_eq!(ToolSummaries::parse("Description"), ToolSummaries::Input);
    }

    #[test]
    fn descriptions() {
        let d = |v: serde_json::Value| description("Bash", &v).map(str::to_owned);
        assert_eq!(
            d(serde_json::json!({"command": "ls", "description": "List files"})),
            Some("List files".to_owned())
        );
        assert_eq!(
            d(serde_json::json!({"description": "  Two\nlines"})),
            Some("Two".to_owned())
        );
        assert_eq!(d(serde_json::json!({"command": "ls"})), None);
        assert_eq!(d(serde_json::json!({"description": ""})), None);
        assert_eq!(d(serde_json::json!({"description": "  \n x"})), None);
        assert_eq!(d(serde_json::json!({"description": 3})), None);
        assert_eq!(d(serde_json::json!(null)), None);
        let input = serde_json::json!({"description": "This change fixes"});
        assert_eq!(description("mcp__tracker__create_issue", &input), None);
        assert_eq!(description("Agent", &input), Some("This change fixes"));
    }
}

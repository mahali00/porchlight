use crate::core::Tool;
use indexmap::{IndexMap, IndexSet};
use regex::{RegexSet, RegexSetBuilder};
use std::sync::LazyLock;

fn patterns(sources: &[&str]) -> Option<RegexSet> {
    RegexSetBuilder::new(sources).case_insensitive(true).build().ok()
}

static DANGEROUS_NAMES: LazyLock<Option<RegexSet>> = LazyLock::new(|| {
    patterns(&[
        r"(^|[_\-.])(shell|bash|zsh|terminal|subprocess|spawn|eval)([_\-.]|$)",
        r"^(exec|execute|run)$",
        r"(exec|execute|run)[_\-.]?(command|cmd|code|script|shell|bash|python|js|javascript)",
        r"write[_\-.]?file",
    ])
});

static DANGEROUS_PHRASES: LazyLock<Option<RegexSet>> = LazyLock::new(|| {
    patterns(&[
        r"\b(runs?|executes?|executing|running)\s+(an?\s+|any\s+|arbitrary\s+)?(shell|terminal|bash)\s+commands?\b",
        r"\b(runs?|executes?|executing|running)\s+(an?\s+|any\s+|arbitrary\s+)?(system|os|cli)\s+commands?\b",
        r"\b(runs?|executes?|executing|running|evaluates?)\s+(arbitrary\s+)?(code|scripts?|python|javascript|js)\b",
        r"\bshell\s+commands?\b",
    ])
});

static DANGEROUS_PARAMETER: LazyLock<Option<RegexSet>> =
    LazyLock::new(|| patterns(&[r"^(command|commands|cmd|script|shell|shell_command|bash)$"]));

fn flagged(set: &LazyLock<Option<RegexSet>>, text: &str) -> bool {
    set.as_ref().is_none_or(|patterns| patterns.is_match(text))
}

fn parameter_names(tool: &Tool) -> Vec<&str> {
    tool.input_schema
        .as_ref()
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.as_object())
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

pub fn is_dangerous(tool: &Tool) -> bool {
    flagged(&DANGEROUS_NAMES, &tool.name)
        || flagged(&DANGEROUS_PHRASES, tool.description.as_deref().unwrap_or_default())
        || parameter_names(tool).into_iter().any(|name| flagged(&DANGEROUS_PARAMETER, name))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    app: String,
    tool: String,
    whole_app: bool,
    on: bool,
}

pub type Rules = Vec<Rule>;

pub fn is_rule_pattern(pattern: &str) -> bool {
    matches!(pattern.split_once('/'), Some((app, tool)) if !app.is_empty() && !tool.is_empty() && !tool.contains('/'))
}

fn glob_matches(glob: &[char], text: &[char]) -> bool {
    match glob.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => (0..=text.len())
            .take_while(|&skip| text.iter().take(skip).all(|c| *c != '/'))
            .any(|skip| text.get(skip..).is_some_and(|tail| glob_matches(rest, tail))),
        Some((expected, rest)) => match text.split_first() {
            Some((actual, tail)) if *actual != '/' && (*expected == '?' || expected == actual) => {
                glob_matches(rest, tail)
            }
            _ => false,
        },
    }
}

fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    glob_matches(&pattern, &text)
}

pub fn rules_from(record: &IndexMap<String, bool>) -> Rules {
    record
        .iter()
        .filter(|(pattern, _)| is_rule_pattern(pattern))
        .filter_map(|(pattern, on)| {
            let (app, tool) = pattern.split_once('/')?;

            Some(Rule { app: app.to_owned(), tool: tool.to_owned(), whole_app: tool == "*", on: *on })
        })
        .collect()
}

pub fn tool_on(rules: &Rules, app: &str, tool: &str) -> bool {
    rules.iter().rev().find(|rule| glob(&rule.app, app) && glob(&rule.tool, tool)).is_none_or(|rule| rule.on)
}

pub fn app_default_on(rules: &Rules, app: &str) -> bool {
    rules.iter().rev().find(|rule| rule.whole_app && glob(&rule.app, app)).is_none_or(|rule| rule.on)
}

pub fn allowed_tool_names(tools: &[Tool], rules: &Rules, app: &str) -> IndexSet<String> {
    tools.iter().filter(|tool| tool_on(rules, app, &tool.name)).map(|tool| tool.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(value: serde_json::Value) -> Tool {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn tools_that_run_commands_or_code_are_flagged_ordinary_tools_are_not() {
        let flagged = [
            json!({ "name": "run_shell" }),
            json!({ "name": "execute_command" }),
            json!({ "name": "bash" }),
            json!({ "name": "write_file" }),
            json!({ "name": "query", "description": "Runs shell commands on the host" }),
            json!({ "name": "debugger-evaluate", "description": "Execute arbitrary JavaScript in the app" }),
            json!({ "name": "query", "inputSchema": { "type": "object", "properties": { "command": { "type": "string" } } } }),
        ];
        let ordinary = [
            json!({ "name": "gesture-swipe", "description": "Execute a swipe gesture on the simulator" }),
            json!({ "name": "native-describe-screen", "description": "Describe the screen. Output is printed in the terminal" }),
            json!({ "name": "boot-device", "description": "Spawn a simulator and wait for it to boot" }),
            json!({ "name": "flow-execute", "description": "Replay a saved flow" }),
            json!({ "name": "delete_nodes", "description": "Delete nodes from the design" }),
            json!({ "name": "write_html", "inputSchema": { "type": "object", "properties": { "html": {}, "targetNodeId": {}, "mode": {} } } }),
            json!({ "name": "odd_schema", "inputSchema": { "properties": "not an object" } }),
        ];

        assert!(flagged.into_iter().all(|value| is_dangerous(&tool(value))));
        assert!(!ordinary.into_iter().any(|value| is_dangerous(&tool(value))));
    }

    #[test]
    fn tool_patterns_stay_inside_their_app_match_literally_and_the_last_match_wins() {
        let rules = rules_from(&IndexMap::from([
            ("garage/*".to_owned(), false),
            ("a/b.c".to_owned(), false),
            ("*/delete_*".to_owned(), false),
            ("x/*".to_owned(), false),
            ("x/read".to_owned(), true),
        ]));

        assert!(!tool_on(&rules, "garage", "open"));
        assert!(tool_on(&rules, "garage2", "open"));
        assert!(!tool_on(&rules, "a", "b.c"));
        assert!(tool_on(&rules, "a", "bxc"));
        assert!(!tool_on(&rules, "paper", "delete_nodes"));
        assert!(tool_on(&rules, "paper", "get/delete_nodes"));
        assert!(tool_on(&rules, "x", "read"));
        assert!(!tool_on(&rules, "x", "write"));
        assert!(!app_default_on(&rules, "x"));
        assert!(app_default_on(&rules, "garage2"));
    }
}

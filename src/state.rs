use crate::core::{Problem, link_path};
use crate::links::{LinkEntry, ServerState};
use crate::policy::Level;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonState {
    pub pid: u32,
    pub tunnel: String,
    pub public_url: String,
    pub servers: Vec<ServerView>,
    pub problems: Vec<Problem>,
    #[serde(default)]
    pub refused: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerView {
    pub name: String,
    pub url: String,
    pub state: ServerState,
    pub detail: String,
    pub tools: usize,
    pub names: Vec<String>,
    #[serde(default)]
    pub catalog: Vec<ToolInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub app: String,
    pub tool: String,
    pub about: Option<String>,
    pub risk: Level,
    #[serde(default)]
    pub effect: Option<String>,
    pub resource: bool,
    pub on: bool,
    #[serde(default)]
    pub inputs: Vec<String>,
}

pub fn view_of(entry: &LinkEntry, public_url: &str) -> ServerView {
    let mut names: Vec<String> = entry.allowed.iter().cloned().collect();
    names.sort();
    let catalog = entry
        .tools
        .iter()
        .map(|tool| {
            let origin = entry.origins.get(&tool.name);
            let risk = entry.risks.get(&tool.name).cloned().unwrap_or_else(|| crate::sources::announced_risk(tool));
            ToolInfo {
                name: tool.name.clone(),
                app: origin.map_or_else(|| entry.name.clone(), |origin| origin.app.clone()),
                tool: origin.map_or_else(|| tool.name.clone(), |origin| origin.tool.clone()),
                about: tool.description.clone(),
                risk: risk.level(),
                effect: risk.effect.map(str::to_owned),
                resource: entry.resources.values().any(|exposed| *exposed == tool.name),
                on: entry.allowed.contains(&tool.name),
                inputs: tool
                    .input_schema
                    .as_ref()
                    .and_then(|schema| schema.get("properties"))
                    .and_then(serde_json::Value::as_object)
                    .map(|properties| properties.keys().cloned().collect())
                    .unwrap_or_default(),
            }
        })
        .collect();

    ServerView {
        name: entry.name.clone(),
        url: format!("{public_url}{}", link_path(&entry.name)),
        state: entry.state,
        detail: entry.detail.clone(),
        tools: entry.allowed.len(),
        names,
        catalog,
    }
}

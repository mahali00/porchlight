use crate::apps::AppSpec;
use crate::core::{Tool, exposed_name, resource_uri};
use crate::sources::commands::{CommandSource, CommandTool, CommandsSpec, command_tools};
use crate::sources::{CommandContribution, Notices, Source};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Starting,
    Waiting,
    Live,
    Refused,
}

#[derive(Clone)]
pub struct PhysicalApp {
    pub spec: AppSpec,
    pub source: Source,
    pub state: ServerState,
    pub detail: String,
    pub tools: Vec<Tool>,
    pub allowed: IndexSet<String>,
    pub default_on: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOrigin {
    pub app: String,
    pub tool: String,
}

#[derive(Clone)]
pub struct LinkEntry {
    pub name: String,
    pub members: IndexSet<String>,
    pub source_app: Option<String>,
    pub source: Source,
    pub state: ServerState,
    pub detail: String,
    pub tools: Vec<Tool>,
    pub allowed: IndexSet<String>,
    pub default_on: bool,
    pub filter_resources: bool,
    pub origins: IndexMap<String, ToolOrigin>,
    pub resources: IndexMap<String, String>,
}

pub struct Projection<'a> {
    pub app: &'a dyn Fn(&str) -> Option<PhysicalApp>,
    pub notices: Notices,
    pub host_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkPlan {
    Direct { name: String, app: String },
    Commands { name: String, apps: Vec<String>, membership: Vec<String> },
}

fn contribution_of(entry: &PhysicalApp) -> Option<&CommandContribution> {
    entry.spec.source.contribution()
}

impl LinkPlan {
    pub fn name(&self) -> &str {
        match self {
            Self::Direct { name, .. } | Self::Commands { name, .. } => name,
        }
    }

    pub fn signature(&self) -> String {
        match self {
            Self::Direct { app, .. } => format!("direct:{app}"),
            Self::Commands { membership, .. } => {
                let mut sorted = membership.clone();
                sorted.sort();
                format!("commands:{}", sorted.join(","))
            }
        }
    }

    pub fn members(&self) -> IndexSet<String> {
        match self {
            Self::Direct { app, .. } => IndexSet::from([app.clone()]),
            Self::Commands { membership, .. } => membership.iter().cloned().collect(),
        }
    }

    pub fn project(&self, context: &Projection) -> Option<LinkEntry> {
        match self {
            Self::Direct { name, app } => {
                let entry = (context.app)(app)?;
                let origins = entry
                    .tools
                    .iter()
                    .map(|tool| (tool.name.clone(), ToolOrigin { app: app.clone(), tool: tool.name.clone() }))
                    .collect();

                Some(LinkEntry {
                    name: name.clone(),
                    members: IndexSet::from([app.clone()]),
                    source_app: Some(app.clone()),
                    source: entry.source,
                    state: entry.state,
                    detail: entry.detail,
                    tools: entry.tools,
                    allowed: entry.allowed,
                    default_on: entry.default_on,
                    filter_resources: false,
                    origins,
                    resources: IndexMap::new(),
                })
            }
            Self::Commands { name, apps, membership } => Some(project_commands(name, apps, membership, context)),
        }
    }
}

fn project_commands(name: &str, apps: &[String], membership: &[String], context: &Projection) -> LinkEntry {
    let members: Vec<PhysicalApp> =
        apps.iter().filter_map(|app| (context.app)(app)).filter(|entry| contribution_of(entry).is_some()).collect();
    let mut tools: IndexMap<String, CommandTool> = IndexMap::new();
    let mut origins: IndexMap<String, ToolOrigin> = IndexMap::new();
    let mut resources: IndexMap<String, String> = IndexMap::new();
    let mut allowed = IndexSet::new();
    let mut instructions = vec![format!("Command-line tools shared on {name}, named app_tool.")];

    for entry in &members {
        let Some(contribution) = contribution_of(entry) else {
            continue;
        };
        let app = &entry.spec.name;

        for (tool, command) in &contribution.tools {
            let exposed = exposed_name(app, tool);
            tools.insert(exposed.clone(), command.clone());
            origins.insert(exposed.clone(), ToolOrigin { app: app.clone(), tool: tool.clone() });

            if command.resource {
                resources.insert(resource_uri(app, tool), exposed);
            }
        }

        if entry.state == ServerState::Live {
            allowed.extend(entry.allowed.iter().map(|tool| exposed_name(app, tool)));
        }

        if let Some(description) = &contribution.description {
            instructions.push(format!("{app}: {description}"));
        }
    }

    let lookup = origins.clone();
    let link = name.to_owned();
    let listed = command_tools(&tools);
    let detail = format!("{} of {} tools", allowed.len(), tools.len());
    let source = CommandSource::new(CommandsSpec {
        name: format!("{name} · {}", context.host_name),
        tools,
        uri_of: Box::new(move |tool| match lookup.get(tool) {
            Some(origin) => resource_uri(&origin.app, &origin.tool),
            None => resource_uri(&link, tool),
        }),
        instructions: Some(instructions.join("\n")),
        notices: Some(context.notices.clone()),
    });

    LinkEntry {
        name: name.to_owned(),
        members: membership.iter().cloned().collect(),
        source_app: None,
        source: Source::Commands(Arc::new(source)),
        state: ServerState::Live,
        detail,
        tools: listed,
        allowed,
        default_on: true,
        filter_resources: true,
        origins,
        resources,
    }
}

pub fn signatures_of(plans: &[LinkPlan]) -> IndexMap<String, String> {
    plans.iter().map(|plan| (plan.name().to_owned(), plan.signature())).collect()
}

pub fn links_for_app(plans: &[LinkPlan], app: &str) -> Vec<String> {
    plans.iter().filter(|plan| plan.members().contains(app)).map(|plan| plan.name().to_owned()).collect()
}

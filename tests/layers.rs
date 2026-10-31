#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const LAYERS: [&[&str]; 7] = [
    &["core", "crypto", "exit"],
    &["config", "policy", "store", "system"],
    &["spec", "sources", "rpc"],
    &["apps", "links", "registry", "surfaces", "auth", "client_metadata", "tunnels"],
    &["serve", "gateway", "approval", "pages", "control", "daemon", "state"],
    &["snapshot", "actions", "discovery", "run"],
    &["cli", "tui", "explain", "lib", "main"],
];

fn layer_of(module: &str) -> Option<usize> {
    LAYERS.iter().position(|layer| layer.contains(&module))
}

fn uses(text: &str) -> BTreeSet<String> {
    text.match_indices("crate::")
        .filter_map(|(at, marker)| text.get(at + marker.len()..))
        .map(|rest| rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == '_').collect::<String>())
        .filter(|name| !name.is_empty())
        .collect()
}

fn collect(dir: &Path, root: &Path, graph: &mut BTreeMap<String, BTreeSet<String>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, root, graph);
            continue;
        }
        let relative = path.strip_prefix(root).unwrap();
        let first = relative.components().next().unwrap().as_os_str().to_str().unwrap();
        let module = first.trim_end_matches(".rs").to_owned();
        let found = uses(&std::fs::read_to_string(&path).unwrap());
        let edges = graph.entry(module.clone()).or_default();
        edges.extend(found.into_iter().filter(|used| *used != module));
    }
}

fn graph() -> BTreeMap<String, BTreeSet<String>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut graph = BTreeMap::new();
    collect(&root, &root, &mut graph);
    graph
}

#[test]
fn every_module_has_a_layer() {
    let unplaced: Vec<String> = graph().into_keys().filter(|module| layer_of(module).is_none()).collect();

    assert!(unplaced.is_empty(), "add these to LAYERS and to docs/design/primitives.md: {unplaced:?}");
}

#[test]
fn modules_only_use_their_own_layer_or_lower_ones() {
    let upward: Vec<String> = graph()
        .iter()
        .flat_map(|(module, used)| used.iter().map(move |other| (module, other)))
        .filter(|(module, other)| layer_of(other) > layer_of(module))
        .map(|(module, other)| format!("{module} -> {other}"))
        .collect();

    assert!(upward.is_empty(), "these point up a layer: {upward:?}");
}

#[test]
fn modules_in_one_layer_never_depend_on_each_other_in_a_cycle() {
    let graph = graph();
    let same_layer = |module: &str| -> Vec<String> {
        graph.get(module).into_iter().flatten().filter(|other| layer_of(other) == layer_of(module)).cloned().collect()
    };

    for start in graph.keys() {
        let mut stack: Vec<Vec<String>> = vec![vec![start.clone()]];

        while let Some(path) = stack.pop() {
            let Some(last) = path.last() else { continue };
            for next in same_layer(last) {
                assert!(&next != start, "cycle: {} -> {next}", path.join(" -> "));
                if !path.contains(&next) {
                    stack.push(path.iter().cloned().chain([next]).collect());
                }
            }
        }
    }
}

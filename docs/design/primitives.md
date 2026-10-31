# porchlight primitives

Status: agreed, 2026-10-31. Step 1 is done.

## Why

porchlight grew by adding features where they were needed. The result works, but the same facts are worked out in several places. A tool's command was parsed in six places, and its risk had five vocabularies. Each copy drifts a little, and that drift is where bugs come from. A real example: the TUI's try screen skipped the warning for the riskiest tools, because its check listed risk levels by hand.

This document names the primitives, says who owns each one, and sets rules so the drift can't come back.

## Principles

1. **One fact, one owner.** Each fact about a tool is computed in exactly one place. Everything else reads it.
2. **Declared, compiled, live.** A tool is declared in `porchlight.json`, compiled into something runnable, then run by the daemon. Each stage has its own type, and no type mixes two stages.
3. **Dependencies point down.** A module only uses modules in lower layers (see [Layers](#layers)).
4. **Operations return outcomes, not sentences.** Wording belongs at the edges: CLI, TUI and web pages.
5. **Honest by default.** When porchlight can't tell whether a tool only reads, it says the tool can change things.
6. **The gate stays green.** Every step passes `cargo fmt --check`, `cargo clippy` and `cargo test` before the next starts.

## The primitives

| Stage | Primitive | Owner | What it is |
|---|---|---|---|
| Declared | `Settings` | config.rs | `porchlight.json` as data |
| Declared | `CommandSpec` | spec.rs | One command tool as declared, with group settings inherited. Owns its template, placeholders, risk and JSON form. **Done in step 1.** |
| Compiled | `AppSpec`, `LinkPlan`, `Rules`, `Problem` | apps.rs, links.rs, policy.rs, core.rs | What porchlight will serve, and what's wrong with the file |
| Compiled | `Source`, `CommandTool` | sources/ | How to reach one app and call its tools |
| Policy | `Risk`, `Level` | policy.rs (type), each source (computes it) | What a client could make a tool do |
| Live | `PhysicalApp`, `LinkEntry`, `McpSurface` | registry.rs, links.rs, surfaces.rs | Running apps and what each link serves |
| Access | request, grant, token, `ClientIdentity` | auth.rs, store.rs, client_metadata.rs | Who may connect |
| Reported | `DaemonState`, `ServerView`, `ToolInfo` | state.rs (new) | What the running daemon reports. Today these live in serve.rs and control.rs |
| Read | `Snapshot`, `ToolRow` | snapshot.rs | What the CLI and TUI show |
| Changed | `Shared` and friends | actions.rs | The result of an operation (step 3) |

Each primitive must not know about anything above it. For example, `CommandSpec` doesn't know about the daemon, and `Risk` doesn't know about wording.

## Layers

```
6  cli, tui, explain, lib, main              edges: wording and prompts
5  snapshot, actions, discovery, run         reading and changing, for the edges
4  serve, gateway, approval, pages, control, daemon, state
3  apps, links, registry, surfaces, auth, client_metadata, tunnels
2  spec, sources/*, rpc
1  config, policy, store, system
0  core, crypto, exit                        depend on nothing
```

A module may use modules in lower layers, and modules in its own layer as long as there is no cycle. `tests/layers.rs` enforces this, and a new module has to be added to its table.

**Breaks today, and the fix for each:**

| Break | Fix |
|---|---|
| `apps ↔ links` cycle | Move `AppSpec` (a named source) into sources.rs. links then needs only sources. |
| `gateway ↔ approval` cycle | Move `form_params`, `query_params` and `MAX_REQUEST_BODY` into rpc.rs. |
| snapshot reads serve.rs internals | Move `DaemonState`, `ServerView` and `ToolInfo` into a new state.rs. serve produces them; snapshot reads them. |
| `rpc → sources` | Move `ListChange` and `UpstreamError` into rpc.rs, since they're protocol types. |
| `pages → auth` (added with the time-left bar) | Put the request's start time in `RequestSummary`, so the page computes the fraction itself. |
| `store → config` for a path | Move `in_state_dir` and the other path helpers into core.rs. |
| `spec → apps` for `slugify` | Move `slugify` into core.rs. |
| `system` sat in layer 5 but only needs config and core | Place it in layer 1: it's platform plumbing (notifications, login service, host name). |

Accepted as they are: `sources → store` (stdio sessions are saved directly; store is lower, so this is allowed) and `config → policy` (same layer, no cycle).

## Risk for every tool

`Risk` holds four facts, and `Level` is derived from them. Levels are ordered:

| Level | Means | Example |
|---|---|---|
| `Reads` | Claimed read-only, and nothing known contradicts that | `date +%F`, a tool with `readOnlyHint` |
| `Changes` | Can change things, but the client can't choose what | `rm -rf /tmp/cache` |
| `ClientChooses` | Can change things, and the client fills free text | `./cleanup.sh {path}` |
| `RunsCode` | Client input reaches a shell or interpreter | `sh -c {script}`, an MCP `run_command` tool |

The order matters for the future. A later policy can say "at `ClientChooses` or above, ask before each call" without adding any new checks.

**Each source computes it:**

- **Command tools:** from the `CommandSpec`. This is done.
- **MCP tools**, from what the server announces:
  - `runs_code` comes from `is_dangerous`: the tool's name, description and parameters.
  - `claimed_read_only` is `readOnlyHint == true`.
  - `effect` is "can delete or overwrite things" when `destructiveHint == true`.
  - `open` lists the text inputs in its schema that have no `enum`.

**Who reads it:**

- The **registry** refuses `RunsCode` unless `allowDangerous` is set. This is unchanged.
- **Clients** are told `readOnlyHint` = `Reads` for command tools. porchlight passes third-party MCP annotations through as they were sent.
- **`try`** runs an MCP tool only at `Reads`. Command tools can always be tried, because a person is running them.
- **Adding a command tool** at `ClientChooses` needs a person at the computer. This is done.
- **The approval page** shows: "What it can use: 12 tools · 3 change things · 1 runs commands". Today it calls code-running tools "can change things", and doesn't mark command tools that change things at all.
- **`tools list`, the TUI and `status --json`** all show the level. `Badge` turns into `Level` plus a resource flag.

## Operations return outcomes

Today `actions::verdict` and `share_mcp` return English sentences, so the CLI and TUI each build their own flow around them. In step 3, actions owns each whole operation and returns data:

```rust
pub struct Shared {
    pub app: String,
    pub links: Vec<Placement>,   // name, plus its address when porchlight is running
    pub created: Vec<String>,    // links that didn't exist before
    pub turned_off: Vec<String>, // tools switched off because they run code
    pub reapproval: bool,        // clients on these links must approve again
}
```

`add_command`, `edit_command`, `share_server` and `import` return `Shared`. Checking a command returns `Fine`, `Note(effect)` or `NeedsPerson(Risk)`, and the CLI or TUI asks the question. A new explain.rs holds the wording that both edges share, such as the caution text.

## Order of work

1. ✅ `CommandSpec`: one compiled form for command tools.
2. ✅ `Risk` for MCP tools. `ToolInfo` and `ToolRow` carry `Level`. Fix the approval page wording and `status --json`.
3. ✅ Outcomes: actions owns add, edit, share and import, and explain.rs holds the shared wording.
4. ✅ state.rs for the daemon's reported state.
5. ✅ The layer fixes in the table above, enforced by `tests/layers.rs`.
6. ✅ Split large files without changing behaviour:
   - `cli.rs` into `cli/` with one file per command group (args, tools, servers, links, access, status);
   - `tui/mod.rs` into rows, keys, act and answer;
   - `sources/commands.rs` into template, risk and inputs.
   `tui/view.rs` (about 1,700 lines of rendering) is the next candidate.
7. ✅ Update AGENTS.md to describe these layers and primitives.

## Left for later, on purpose

- **Approving each call on the computer** for tools at `ClientChooses` or above. `Level` is ordered so this is one rule.
- **OS sandboxing**, which would let porchlight enforce what a tool can touch instead of trusting claims.
- **MCP elicitation**, for asking the user in the middle of a call through the client.

## Decisions

Made 2026-10-31.

1. **`status --json` uses `risk` now.** Each tool's `readOnly` and `dangerous` fields are replaced by `risk` (the level) and `effect`, with no transition period.
2. **MCP tools at `ClientChooses` stay on** when a server is added. Clients ask before each call, and only `RunsCode` tools are turned off.
3. **MCP tools with no `readOnlyHint` show as "changes things".**
4. **A test enforces the layers.** `tests/layers.rs` fails when a module uses a module in a higher layer, or when two modules in the same layer depend on each other.

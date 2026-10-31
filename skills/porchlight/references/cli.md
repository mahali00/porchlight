# CLI reference

Every action in the porchlight app is also a command. Run `porchlight` with no command to open the app.

```
porchlight <command> [options]
```

- `--json` prints one JSON value for scripts. On errors it prints `{"ok": false, "error", "next", "exit"}` to stderr.
- `-h, --help` shows the options and examples for any command.
- `-V, --version` prints the version.

## Quick start

```sh
porchlight tools add today --read-only -- date +%F
porchlight start
porchlight links list
porchlight approve
```

## Status and service

| Command | What it does |
|---|---|
| `porchlight status` | What's shared, what's waiting, and the next step |
| `porchlight start` | Start sharing. Installs a login service (launchd or systemd) so sharing keeps running, and OpenTunnel if you use it |
| `porchlight stop` | Stop sharing and remove the login service |
| `porchlight logs` | Recent calls and changes |

`logs` options:

| Option | |
|---|---|
| `--tool <TOOL>` | Only calls to this tool |
| `-l, --link <LINK>` | Only calls on this link |
| `-c, --client <CLIENT>` | Only calls from this client |
| `-n, --lines <N>` | How many recent lines (default 20) |
| `--follow` | Keep printing new lines (the default in a terminal) |
| `--once` | Print recent lines and stop |

## Tools

Share command-line programs as tools, and turn any tool on or off. Everything after `--` is the command, word for word; porchlight never runs a shell. A `{name}` in the command becomes an input the client fills in.

| Command | What it does |
|---|---|
| `tools list [--link LINK]` | Every tool, whether it's on, what it can do, and its links |
| `tools add NAME [options] -- COMMAND...` | Share a program as a tool |
| `tools edit TOOL [options] [-- COMMAND...]` | Change its command, description or read-only setting |
| `tools remove TOOL [--force]` | Stop sharing it |
| `tools enable TOOL` | Turn it on |
| `tools disable TOOL` | Turn it off without removing it |
| `tools try TOOL [KEY=VALUE...]` | Run it once on this computer and print the result |

`TOOL` is a tool name, `group/tool` for a tool inside a group, or `server/tool` for an MCP server's tool (`enable`, `disable` and `try` only).

`tools add` options:

| Option | |
|---|---|
| `--read-only` | It only reads, it never changes anything |
| `-d, --description <TEXT>` | What it does, shown to the client |
| `-l, --link <LINK>` | Put it on this link (repeat for more). Without it, the tool goes on the `tools` link |
| `-e, --env <KEY=VALUE>` | Environment variable for the command (repeat for more) |

`tools edit` takes `--read-only` (or `--read-only=false`), `-d`, and an optional new command after `--`.

A command that deletes or overwrites things (`rm`, `shred`, `dd`, `mkfs`, `kill`, `find -delete`, `git push --force`, `docker rm` and similar) can't be `--read-only`. When a client chooses one of its inputs, `tools add` and `tools edit` explain the risk and ask in a terminal; in a script they exit 5. No client input may contain a `..` path segment, for any tool.

Tool names: a tool inside an MCP server can be named on its own (`wipe`) when only one server has it, or as `server/tool` (`fixture/wipe`).

```sh
porchlight tools add weather --read-only -d "Weather for a city" -- curl -s "wttr.in/{city}?format=3"
porchlight tools add backup -l home -e DEST=/Volumes/Backup -- backup.sh {folder}
porchlight tools try weather city="New York"
porchlight tools disable notes/delete_note
```

MCP server tools can only be tried when they only read.

## Servers

Share MCP servers that run on this computer. Each server gets its own link.

| Command | What it does |
|---|---|
| `servers list` | Servers and their state |
| `servers add NAME --url URL [-H KEY=VALUE...]` | Share a server by its Streamable HTTP address, with optional headers |
| `servers add NAME [-e KEY=VALUE...] -- COMMAND...` | Share a server by the command that starts it |
| `servers import [NAME...] [--all]` | List the servers Claude, Cursor and VS Code use, or share the named ones. It reads their settings and never changes them |
| `servers remove NAME [--force]` | Stop sharing it |
| `servers enable NAME` | Turn it back on |
| `servers disable NAME` | Turn it off without removing it |

Headers and environment values can use `{env:NAME}` and `{file:path}`.

```sh
porchlight servers add notes --url http://127.0.0.1:8080/mcp
porchlight servers add notes --url https://notes.local/mcp -H 'Authorization=Bearer {env:NOTES_TOKEN}'
porchlight servers add files -- npx -y @modelcontextprotocol/server-filesystem ~/Documents
```

## Links

A link is the address a client connects to. Command-line tools are shared on the links you put them on. When a link's tools change, its clients approve again.

| Command | What it does |
|---|---|
| `links list` | Links, their addresses and tools |
| `links create NAME [--tool TOOL...]` | Create a link for command-line tools |
| `links edit NAME [--add TOOL...] [--remove TOOL...]` | Add or take tools off a link |
| `links delete NAME [--force]` | Delete a link. Its clients lose access |

## Approving clients

| Command | What it does |
|---|---|
| `approve [REQUEST]` | Print the code for a waiting client. Type it on the device that's connecting |
| `deny [REQUEST]` | Turn a waiting client down |
| `clients list` | Waiting and approved clients |
| `clients revoke CLIENT [--link LINK] [--force]` | Take a client's access away, everywhere or on one link |
| `clients revoke --all [--force]` | Take every client's and token's access away |

When more than one request is waiting, `approve` and `deny` ask which one in a terminal and need `REQUEST` in scripts. Only a person at this computer can approve: in a script, `approve` exits with code 5.

## Tokens

For scripts and automations that can't sign in. Send a token as `Authorization: Bearer <token>`.

| Command | What it does |
|---|---|
| `tokens create --link LINK [--name NAME] [--expires 30d]` | Create a token, shown once. Needs a person at this computer |
| `tokens list` | List tokens |
| `tokens revoke NAME [--force]` | Revoke a token |

`--expires` takes values like `30d`, `12h` or `60m`.

## Tunnel

| Command | What it does |
|---|---|
| `tunnel` | The current tunnel, public address and local port |
| `tunnel use opentunnel` | Use OpenTunnel (the default) |
| `tunnel use https://ADDRESS` | Use your own tunnel, such as cloudflared, pointed at the local port shown by `porchlight tunnel` |

## Shell completions

```sh
porchlight completions zsh > ~/.zfunc/_porchlight
porchlight completions bash > ~/.local/share/bash-completion/completions/porchlight
porchlight completions fish > ~/.config/fish/completions/porchlight.fish
```

## Scripts and agents

- Add `--json` to any command.
- `remove`, `delete` and `revoke` ask first in a terminal. In scripts, pass `--force`. They never exit 0 when nothing happened.
- `approve` and `tokens create` need a person at this computer.
- Agents should start from [SKILL.md](../SKILL.md).

| Exit code | Meaning |
|---|---|
| 0 | Done |
| 1 | The tool failed |
| 2 | Not found or invalid input |
| 4 | Tunnel problem |
| 5 | Needs a person at this computer |
| 10 | Internal error |

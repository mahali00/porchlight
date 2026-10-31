---
name: porchlight
description: Share command-line programs and local MCP servers with remote MCP clients through porchlight. Use when a user wants a tool, script or MCP server on their computer reachable from Claude, ChatGPT or another MCP client, when setting porchlight up, adding or changing shared tools, links or tunnels, or when reading porchlight's status, clients or logs.
---

# porchlight

porchlight runs on the user's computer and turns command-line programs and local MCP servers into MCP tools that remote clients reach over HTTPS. A person at that computer approves every client.

1. **Share.** `porchlight tools add NAME -- COMMAND...` wraps a program as a tool. `porchlight servers add NAME --url URL` or `servers add NAME -- COMMAND...` shares an MCP server. `porchlight servers import` lists the servers the user's other MCP clients already use; `servers import NAME` shares one.
2. **Start.** `porchlight start` opens the tunnel and keeps sharing running in a login service.
3. **Connect.** The user adds a link's address (`porchlight links list`) to their MCP client as a remote MCP server, then approves on the computer with the code porchlight shows.

You can do steps 1 and 2. Step 3's approval needs the person: never try to approve, create tokens, or work around it.

## Workflow

When asked to share something:

1. Run `porchlight status --json` to see what's running, what's shared and the next step.
2. Pick the narrowest command that does the job. Prefer one program with arguments (`df -h`) over a shell script. Never wrap `sh -c`, `bash -c`, `python -c` or similar: porchlight refuses them, and they give the client full control of the computer.
3. Add it with everything after `--` word for word. Use `{name}` for each value the client should fill in. Add `--read-only` only if it truly never changes anything. Add `-d` with a one-line description the client will see. A command that deletes or overwrites things (`rm`, `dd`, `git push --force` and similar) with a client-chosen input needs the person to confirm it in a terminal and exits 5 for you: tell the user to run it, and suggest fixing the risky part, like `rm ~/Downloads/{name}` instead of `rm {path}`.

   ```sh
   porchlight tools add disk --read-only -d "Free disk space" -- df -h
   porchlight tools add logs --read-only -d "Last lines of a service log" -- journalctl -u {service} -n 50 --no-pager
   ```

4. If an input needs a type, choices or a description, edit `inputs` in `porchlight.json` ([configuration](references/configuration.md)).
5. Try it once: `porchlight tools try NAME KEY=VALUE`.
6. Choose the link. Tools go on `tools` unless you pass `-l LINK`. Put tools for different clients on different links. Changing a link's tools makes its clients approve again, so tell the user.
7. Run `porchlight start` if it isn't running, then `porchlight links list` for the address.
8. Tell the user: the address to add in their MCP client, that they'll get a code from porchlight on this computer, and where to type it.

## Rules

- Use `--json` and read `ok`, `error` and `next`. Exit 5 means a person must do it.
- Destructive commands (`remove`, `delete`, `revoke`) need `--force` without a terminal. Only use `--force` when the user asked for that exact change.
- Don't set `allowDangerous` unless the user explicitly asks and understands the tool can run anything.
- Don't edit MCP clients' config files to add porchlight. Give the user the address.
- `porchlight start` installs a login service and possibly OpenTunnel. Say so before running it the first time.

## References

Read the one you need:

| File | Read it when |
|---|---|
| [`references/cli.md`](references/cli.md) | You need a command, option or exit code |
| [`references/configuration.md`](references/configuration.md) | Editing `porchlight.json`: inputs, groups, environment, MCP server settings, tool rules, links |
| [`references/tunnels.md`](references/tunnels.md) | Using the user's own domain or tunnel (cloudflared, a reverse proxy) instead of OpenTunnel |
| [`references/limits.md`](references/limits.md) | Judging what's safe to share, and what approval does and doesn't protect |

## Commands

| Command | What it does |
|---|---|
| `status` | What's shared, what's waiting, and the next step |
| `start` / `stop` | Start sharing (login service) / stop and remove the service |
| `tools list\|add\|edit\|remove\|enable\|disable\|try` | Command-line tools, and turning any tool on or off |
| `servers list\|add\|import\|remove\|enable\|disable` | MCP servers |
| `links list\|create\|edit\|delete` | Addresses clients connect to |
| `approve` / `deny` | Show the code for a waiting client / turn it down (person only) |
| `clients list\|revoke` | Waiting and approved clients |
| `tokens create\|list\|revoke` | Tokens for automations (`create` needs a person) |
| `tunnel [use]` | Show or change the tunnel |
| `logs` | Recent calls and changes |

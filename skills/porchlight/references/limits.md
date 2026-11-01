# What porchlight does not guarantee

porchlight decides who may call your tools. It doesn't make a tool safe to call.

- **Tools run as you.** A shared command runs with your user's permissions, in its `cwd`, with the allowlisted environment. It isn't sandboxed.
- **"Runs commands" is a heuristic.** porchlight refuses commands that hand client input to a shell or interpreter (`sh -c`, `python -c`, `busybox sh`, `env`/`xargs` wrappers and similar) unless you set `allowDangerous`. It works from a list of known programs and flags, so a program that evaluates its input in some other way won't be caught. Only share programs you understand.
- **"Destructive" is a heuristic too.** porchlight recognizes common programs that delete or overwrite things (`rm`, `dd`, `mkfs`, `git push --force` and similar), asks a person before sharing one with client-chosen inputs, and tells clients it's destructive. A script of your own that deletes things isn't recognized; mark it honestly by leaving out `readOnly`. Blocking `..` keeps an input inside a fixed folder, but symlinks inside that folder still lead wherever they point.
- **MCP servers describe their own tools.** A tool's `readOnlyHint` and description come from the server. porchlight checks the tool definitions it sees, again whenever the list changes, but a server can still lie about what a tool does.
- **Editing a tool keeps its approvals.** Changing a tool's command, description or read-only setting keeps the link's clients approved. Adding or removing tools on a link makes them approve again.
- **Anyone with a link's address can ask to connect.** Only a person at this computer can approve. A request expires after 10 minutes, and its code locks after 5 wrong tries.
- **Tokens are bearer secrets.** Anyone holding an access token or a `porchlight tokens create` token can use that link until it expires or you revoke it. Access tokens last an hour; sign-in refresh lasts 30 days.
- **Your tunnel sees what it relays.** With OpenTunnel, TLS ends on your computer. With your own tunnel or proxy, that provider can see the requests, including tool inputs and outputs.
- **Local users.** porchlight's state, config and control socket are readable only by your user. Anyone who can act as your user can approve clients.

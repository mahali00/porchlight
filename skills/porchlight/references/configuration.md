# Configuration

Everything porchlight shares lives in `~/.porchlight/porchlight.json` (or `$PORCHLIGHT_HOME/porchlight.json`). The CLI and the app edit this file for you, keeping your comments and key order. You can also edit it by hand. porchlight reloads it when you save, and keeps it readable only by you (0600).

An entry that doesn't parse becomes a problem shown in `porchlight status` and the app. The rest of the file keeps working.

The full schema is [`porchlight.schema.json`](../../../porchlight.schema.json). Add `"$schema"` pointing at it to get completion in your editor.

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/mahali00/porchlight/main/porchlight.schema.json",
  "tunnel": "opentunnel",
  "commands": {
    "today": { "run": "date +%F", "description": "Today's date", "readOnly": true }
  },
  "mcp": {
    "notes": { "url": "http://127.0.0.1:8080/mcp" }
  },
  "links": {
    "tools": ["today"]
  }
}
```

## Top level

| Key | What it is |
|---|---|
| `tunnel` | `"opentunnel"`, or your own tunnel's `https://` address ([tunnels](tunnels.md)) |
| `port` | Local port the tunnel forwards to. Set it when your own tunnel needs a fixed port |
| `commands` | Command-line programs shared as tools, by name |
| `mcp` | MCP servers on this computer, by name |
| `tools` | Turn tools on or off with `"app/tool"` patterns |
| `links` | Named links for command-line tools |

## Command tools

The short form is just the command:

```jsonc
"commands": { "uptime": "uptime" }
```

The long form:

| Key | What it is |
|---|---|
| `run` | The command, as a string or an array of words. `{name}` is filled in by the client |
| `description` | What it does, shown to the client |
| `readOnly` | `true` if it only reads and never changes anything |
| `resource` | `true` to share its output as a resource the client reads, instead of a tool it calls |
| `inputs` | Describe or limit each `{name}` (below) |
| `environment` | Environment variables. Values can use `{env:NAME}` and `{file:path}` |
| `cwd` | Folder it runs in |
| `allowDangerous` | Share it even though it hands client input to a shell or interpreter |

Commands run without a shell. Each input reaches the program as exactly one argument, and an input can't start with `-`. Only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL` and `TMPDIR` are passed through, plus the tool's own `environment`.

### Inputs

```jsonc
"weather": {
  "run": ["curl", "-s", "wttr.in/{city}?format={format}"],
  "readOnly": true,
  "inputs": {
    "city": { "description": "City name, like Lisbon" },
    "format": { "type": "integer", "choices": ["1", "2", "3"], "optional": true }
  }
}
```

| Key | What it is |
|---|---|
| `type` | `string` (default), `integer`, `number` or `boolean` |
| `description` | What to fill in, shown to the client |
| `choices` | The only values allowed |
| `min`, `max` | Smallest and largest number allowed |
| `optional` | The client may leave it out, which drops its word from the command |
| `flag` | For a boolean: the flag added when it's true, like `--verbose` |

### Groups

Several tools that belong together go in one group. Group-level `description`, `environment`, `cwd` and `allowDangerous` apply to every tool in it. Clients see the tools as `group_tool`, for example `garage_open`.

```jsonc
"garage": {
  "cwd": "/srv/garage",
  "open": { "run": "garage open", "description": "Opens the garage door" },
  "status": { "run": "garage status", "readOnly": true }
}
```

## MCP servers

By address:

| Key | What it is |
|---|---|
| `url` | Streamable HTTP address |
| `headers` | Headers sent to this server only. Values can use `{env:NAME}` and `{file:path}` |
| `enabled` | `false` keeps it here without running it |
| `allowDangerous` | Share it even if some tools can run shell commands or code |

By command:

| Key | What it is |
|---|---|
| `command` | Command that starts a stdio server, as a string or an array |
| `environment` | Environment for the server. `{env:NAME}` and `{file:path}` work |
| `cwd` | Folder it runs in |
| `enabled`, `allowDangerous` | As above |

A server with tools that can run shell commands or code is refused unless those tools are off or `allowDangerous` is `true`. `porchlight servers add` turns such tools off for you and says which ones.

## Turning tools on and off

```jsonc
"tools": {
  "notes/delete_*": false,
  "garage/*": false,
  "garage/status": true
}
```

Patterns are `app/tool`, and `*` matches within one part. The last matching rule wins, and tools nothing matches stay on. `porchlight tools enable` and `tools disable` write these rules for you.

## Links

```jsonc
"links": {
  "home": ["garage", "weather"],
  "everything": "*"
}
```

A link lists command tools by name, or uses `"*"` for every command tool. Each MCP server already has its own link with its name. When a link's tools change, porchlight takes access away from its clients so they approve again.

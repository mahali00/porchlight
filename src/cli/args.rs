use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "porchlight",
    version,
    about = "Share MCP servers and command-line tools with MCP clients. Approve every client.",
    after_help = "Quick start\n  porchlight tools add <name> -- <command> [args...]\n  porchlight start\n  porchlight links list\n  porchlight approve\n\nFor agents\n  Use --json for machine-readable output.\n  approve and tokens create need a person at this computer and exit 5 for scripts.\n  Exit codes: 1 tool failed, 2 not found/invalid, 4 tunnel, 5 needs a person, 10 internal."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(short, long, global = true, help = "print JSON for scripts")]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(hide = true, about = "Run the daemon in the foreground")]
    Serve,
    #[command(
        about = "Show what's shared, what's waiting, and what to do next",
        after_help = "Example:\n  porchlight status --json"
    )]
    Status,
    #[command(
        about = "Show recent calls and changes",
        after_help = "Examples:\n  porchlight logs -n 50\n  porchlight logs --tool backup --follow"
    )]
    Logs(LogsArgs),
    #[command(
        about = "Start sharing; installs a login service so it keeps running",
        after_help = "Example:\n  porchlight start"
    )]
    Start,
    #[command(about = "Stop sharing and remove the login service", after_help = "Example:\n  porchlight stop")]
    Stop,
    #[command(
        about = "Share command-line programs as tools, and turn any tool on or off",
        after_help = "Examples:\n  porchlight tools add today --read-only -- date +%F\n  porchlight tools list"
    )]
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
    #[command(
        about = "Share MCP servers that run on this computer",
        after_help = "Examples:\n  porchlight servers add notes --url http://127.0.0.1:8080/mcp\n  porchlight servers add files -- npx -y @modelcontextprotocol/server-filesystem ~/Documents"
    )]
    Servers {
        #[command(subcommand)]
        action: ServersAction,
    },
    #[command(
        about = "Group tools under an address that clients connect to",
        after_help = "Each MCP server has its own link. Command-line tools are shared on the links you put them on.\n\nExample:\n  porchlight links list"
    )]
    Links {
        #[command(subcommand)]
        action: LinksAction,
    },
    #[command(
        about = "Show the code for a waiting client (needs a person at this computer)",
        after_help = "Type the code on the device that asked to connect.\n\nExamples:\n  porchlight approve\n  porchlight approve REQUEST"
    )]
    Approve {
        #[arg(help = "which request, when more than one is waiting")]
        request: Option<String>,
    },
    #[command(about = "Turn down a waiting client", after_help = "Example:\n  porchlight deny REQUEST")]
    Deny {
        #[arg(help = "which request, when more than one is waiting")]
        request: Option<String>,
    },
    #[command(
        about = "See waiting and approved clients, and take access away",
        after_help = "Example:\n  porchlight clients list"
    )]
    Clients {
        #[command(subcommand)]
        action: ClientsAction,
    },
    #[command(
        about = "Tokens for scripts and automations that can't sign in",
        after_help = "Example:\n  porchlight tokens create --link tools --name backup-job"
    )]
    Tokens {
        #[command(subcommand)]
        action: TokensAction,
    },
    #[command(
        about = "Show or change how clients reach this computer",
        after_help = "Examples:\n  porchlight tunnel\n  porchlight tunnel use https://mcp.example.com"
    )]
    Tunnel {
        #[command(subcommand)]
        action: Option<TunnelAction>,
    },
    #[command(
        about = "Print shell completions",
        after_help = "Example:\n  porchlight completions zsh > ~/.zfunc/_porchlight"
    )]
    Completions {
        #[arg(help = "bash, zsh, fish, elvish or powershell")]
        shell: clap_complete::Shell,
    },
}

#[derive(Args)]
pub struct LogsArgs {
    #[arg(long, help = "only calls to this tool")]
    pub tool: Option<String>,
    #[arg(short, long, help = "only calls on this link")]
    pub link: Option<String>,
    #[arg(short, long, help = "only calls from this client")]
    pub client: Option<String>,
    #[arg(short = 'n', long, default_value_t = 20, help = "how many recent lines")]
    pub lines: usize,
    #[arg(long, help = "keep printing new lines (the default in a terminal)")]
    pub follow: bool,
    #[arg(long, conflicts_with = "follow", help = "print recent lines and stop")]
    pub once: bool,
}

#[derive(Subcommand)]
pub enum ToolsAction {
    #[command(
        about = "List every tool, whether it's on, what it can do and its links",
        after_help = "Example:\n  porchlight tools list --link home"
    )]
    List {
        #[arg(long, help = "only tools on this link")]
        link: Option<String>,
    },
    #[command(
        about = "Share a command-line program as a tool",
        after_help = "Everything after -- is the command, word for word. {name} becomes an input the client fills in.\nWithout --link the tool goes on the link \"tools\".\n\nExamples:\n  porchlight tools add today --read-only -- date +%F\n  porchlight tools add weather --read-only -d 'Weather for a city' -- curl -s 'wttr.in/{city}?format=3'\n  porchlight tools add backup -l home -e DEST=/Volumes/Backup -- backup.sh {folder}"
    )]
    Add {
        #[arg(help = "tool name: lowercase letters, numbers and -")]
        name: String,
        #[arg(long, help = "it only reads, it never changes anything")]
        read_only: bool,
        #[arg(short, long, help = "what it does, shown to the client")]
        description: Option<String>,
        #[arg(short, long, help = "put it on this link (repeat for more)")]
        link: Vec<String>,
        #[arg(short, long, value_name = "KEY=VALUE", help = "environment variable for the command (repeat for more)")]
        env: Vec<String>,
        #[arg(last = true, required = true, value_name = "COMMAND", help = "the command and its arguments")]
        command: Vec<String>,
    },
    #[command(
        about = "Change a tool's command, description or read-only setting",
        after_help = "Examples:\n  porchlight tools edit backup -d 'Copy a folder to the backup drive'\n  porchlight tools edit backup --read-only=false\n  porchlight tools edit backup -- backup.sh --verbose {folder}"
    )]
    Edit {
        #[arg(help = "tool, or tool/name inside a group")]
        target: String,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", help = "it only reads (--read-only=false to undo)")]
        read_only: Option<bool>,
        #[arg(short, long, help = "what it does, shown to the client")]
        description: Option<String>,
        #[arg(last = true, value_name = "COMMAND", help = "a new command, word for word")]
        command: Vec<String>,
    },
    #[command(
        about = "Stop sharing a command-line tool",
        after_help = "Example:\n  porchlight tools remove backup --force"
    )]
    Remove {
        #[arg(help = "tool, or tool/name inside a group")]
        target: String,
        #[arg(long, help = "don't ask first (needed in scripts)")]
        force: bool,
    },
    #[command(
        about = "Turn a tool on",
        after_help = "Examples:\n  porchlight tools enable backup\n  porchlight tools enable notes/delete_note"
    )]
    Enable {
        #[arg(help = "tool, or server/tool")]
        target: String,
    },
    #[command(
        about = "Turn a tool off without removing it",
        after_help = "Examples:\n  porchlight tools disable backup\n  porchlight tools disable notes/delete_note"
    )]
    Disable {
        #[arg(help = "tool, or server/tool")]
        target: String,
    },
    #[command(
        about = "Run a tool once on this computer and print what it returns",
        after_help = "MCP server tools can only be tried when they only read.\n\nExamples:\n  porchlight tools try today\n  porchlight tools try weather city='New York'"
    )]
    Try {
        #[arg(help = "tool, or server/tool")]
        target: String,
        #[arg(value_name = "KEY=VALUE", help = "inputs, one per argument")]
        values: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ServersAction {
    #[command(about = "List MCP servers and their state", after_help = "Example:\n  porchlight servers list")]
    List,
    #[command(
        about = "Share an MCP server by URL, or by the command that starts it",
        after_help = "Examples:\n  porchlight servers add notes --url http://127.0.0.1:8080/mcp\n  porchlight servers add notes --url https://notes.local/mcp -H 'Authorization=Bearer {env:NOTES_TOKEN}'\n  porchlight servers add files -e ROOT=~/Documents -- npx -y @modelcontextprotocol/server-filesystem ~/Documents"
    )]
    Add {
        #[arg(help = "server name: lowercase letters, numbers and -")]
        name: String,
        #[arg(long, conflicts_with = "command", help = "its Streamable HTTP address")]
        url: Option<String>,
        #[arg(
            short = 'H',
            long,
            requires = "url",
            value_name = "KEY=VALUE",
            help = "header sent to it (repeat for more)"
        )]
        header: Vec<String>,
        #[arg(
            short,
            long,
            requires = "command",
            value_name = "KEY=VALUE",
            help = "environment variable (repeat for more)"
        )]
        env: Vec<String>,
        #[arg(last = true, value_name = "COMMAND", help = "the command that starts it, word for word")]
        command: Vec<String>,
    },
    #[command(
        about = "Share MCP servers your other clients already use",
        after_help = "porchlight reads Claude, Cursor and VS Code settings. It never changes them.\nWithout names it lists what it found.\n\nExamples:\n  porchlight servers import\n  porchlight servers import github files\n  porchlight servers import --all"
    )]
    Import {
        #[arg(help = "servers to share, as listed")]
        names: Vec<String>,
        #[arg(long, conflicts_with = "names", help = "share every one that runs on this computer")]
        all: bool,
    },
    #[command(about = "Stop sharing an MCP server", after_help = "Example:\n  porchlight servers remove notes --force")]
    Remove {
        #[arg(help = "server name")]
        name: String,
        #[arg(long, help = "don't ask first (needed in scripts)")]
        force: bool,
    },
    #[command(about = "Turn a server back on", after_help = "Example:\n  porchlight servers enable notes")]
    Enable {
        #[arg(help = "server name")]
        name: String,
    },
    #[command(
        about = "Turn a server off without removing it",
        after_help = "Example:\n  porchlight servers disable notes"
    )]
    Disable {
        #[arg(help = "server name")]
        name: String,
    },
}

#[derive(Subcommand)]
pub enum LinksAction {
    #[command(about = "List links, their addresses and tools", after_help = "Example:\n  porchlight links list --json")]
    List,
    #[command(
        about = "Create a link for command-line tools",
        after_help = "Example:\n  porchlight links create home --tool backup --tool today"
    )]
    Create {
        #[arg(help = "link name: lowercase letters, numbers and -")]
        name: String,
        #[arg(long = "tool", help = "a tool to put on it (repeat for more)")]
        tools: Vec<String>,
    },
    #[command(
        about = "Add or take tools off a link",
        after_help = "Clients on the link approve again after its tools change.\n\nExample:\n  porchlight links edit home --add weather --remove backup"
    )]
    Edit {
        #[arg(help = "link name")]
        name: String,
        #[arg(long, help = "a tool to add (repeat for more)")]
        add: Vec<String>,
        #[arg(long, help = "a tool to take off (repeat for more)")]
        remove: Vec<String>,
    },
    #[command(
        about = "Delete a link; its clients lose access",
        after_help = "Example:\n  porchlight links delete home --force"
    )]
    Delete {
        #[arg(help = "link name")]
        name: String,
        #[arg(long, help = "don't ask first (needed in scripts)")]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum ClientsAction {
    #[command(about = "List waiting and approved clients", after_help = "Example:\n  porchlight clients list")]
    List,
    #[command(
        about = "Take a client's access away",
        after_help = "Examples:\n  porchlight clients revoke Claude\n  porchlight clients revoke Claude --link home\n  porchlight clients revoke --all --force"
    )]
    Revoke {
        #[arg(required_unless_present = "all", help = "client name or id")]
        client: Option<String>,
        #[arg(long, help = "only on this link")]
        link: Option<String>,
        #[arg(long, conflicts_with_all = ["client", "link"], help = "every client and token")]
        all: bool,
        #[arg(long, help = "don't ask first (needed in scripts)")]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum TokensAction {
    #[command(
        about = "Create a token (needs a person at this computer; shown once)",
        after_help = "Send it as Authorization: Bearer <token>.\n\nExample:\n  porchlight tokens create --link tools --name backup-job --expires 7d"
    )]
    Create {
        #[arg(long, help = "the link it can use")]
        link: String,
        #[arg(long, help = "a name to recognize it by")]
        name: Option<String>,
        #[arg(long, default_value = "30d", help = "how long it lasts, like 30d, 12h or 60m")]
        expires: String,
    },
    #[command(about = "List tokens", after_help = "Example:\n  porchlight tokens list")]
    List,
    #[command(about = "Revoke a token", after_help = "Example:\n  porchlight tokens revoke backup-job --force")]
    Revoke {
        #[arg(help = "token name or id")]
        target: String,
        #[arg(long, help = "don't ask first (needed in scripts)")]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum TunnelAction {
    #[command(
        about = "Use OpenTunnel, or your own tunnel's https address",
        after_help = "Point your own tunnel at the local port shown by porchlight tunnel.\n\nExamples:\n  porchlight tunnel use opentunnel\n  porchlight tunnel use https://mcp.example.com"
    )]
    Use {
        #[arg(help = "opentunnel, or your tunnel's https:// address")]
        tunnel: String,
    },
}

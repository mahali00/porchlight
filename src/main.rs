use clap::Parser;
use porchlight::cli::Command;
use porchlight::exit::{ExitCode, ExitError};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(
    name = "porchlight",
    version,
    about = "Share MCP servers and command-line tools on this computer with MCP clients. You approve every client.",
    after_help = "Run porchlight on its own to open the full-screen app."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(short, long, global = true, help = "print JSON for scripts")]
    json: bool,
}

async fn run(cli: Cli) -> Result<(), ExitError> {
    match cli.command {
        Some(command) => porchlight::cli::run(command, cli.json).await,
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => porchlight::tui::run().await,
        None => Err(ExitError::new(ExitCode::NeedsHuman, "porchlight opens in a terminal")
            .next("Run it in a terminal on this computer. Scripts can use porchlight status --json.")),
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("✗ {}", error.message);

            if let Some(next) = &error.next_action {
                eprintln!("  {next}");
            }

            std::process::ExitCode::from(error.code.value())
        }
    }
}

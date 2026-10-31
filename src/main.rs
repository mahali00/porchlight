use clap::Parser;
use porchlight::cli::Cli;
use porchlight::exit::{ExitCode, ExitError};
use serde_json::json;
use std::io::{IsTerminal, Write};

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
    let json_output = std::env::args()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json" || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('j')));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(error.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion) =>
        {
            let _ = write!(std::io::stdout(), "{error}");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            if json_output {
                eprintln!(
                    "{}",
                    json!({"ok": false, "error": error.to_string(), "next": "Use porchlight --help.", "exit": 2})
                );
            } else {
                eprint!("{error}");
            }
            return std::process::ExitCode::from(2);
        }
    };
    let json_output = cli.json;
    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                eprintln!(
                    "{}",
                    json!({
                        "ok": false,
                        "error": error.message,
                        "next": error.next_action,
                        "exit": error.code.value()
                    })
                );
            } else {
                eprintln!("✗ {}", error.message);
                if let Some(next) = &error.next_action {
                    eprintln!("  {next}");
                }
            }
            std::process::ExitCode::from(error.code.value())
        }
    }
}

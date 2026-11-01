macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(error) = writeln!(std::io::stdout(), $($arg)*) {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("stdout: {error}");
            std::process::exit(crate::exit::ExitCode::Internal.value().into());
        }
    }};
}

pub mod actions;
pub mod approval;
pub mod apps;
pub mod auth;
pub mod cli;
pub mod client_metadata;
pub mod config;
pub mod control;
pub mod core;
pub mod crypto;
pub mod daemon;
pub mod discovery;
pub mod exit;
pub mod explain;
pub mod gateway;
pub mod links;
pub mod pages;
pub mod policy;
pub mod registry;
pub mod rpc;
pub mod run;
pub mod serve;
pub mod snapshot;
pub mod sources;
pub mod spec;
pub mod state;
pub mod store;
pub mod surfaces;
pub mod system;
pub mod tui;
pub mod tunnels;

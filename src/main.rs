mod agents;
mod audit;
mod cli;
mod client;
mod daemon;
mod git_engine;
mod ledger;
mod mcp;
mod prompt;
mod protocol;
mod rules;
mod session;
mod shim;
mod style;
mod ui;
mod workspace;

use std::path::Path;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let executable = argv
        .first()
        .and_then(|arg0| {
            Path::new(arg0)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "sp".to_string());
    let args = argv.iter().skip(1).cloned().collect::<Vec<_>>();
    let code = if executable == "git" {
        shim::run_git_shim(&args)
    } else if executable == "scratchpostd" {
        match daemon::run_daemon() {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("scratchpostd: {err}");
                2
            }
        }
    } else if args.first().map(|s| s.as_str()) == Some("--mcp") {
        mcp::run(cli::master_root())
    } else {
        cli::run(&args)
    };
    std::process::exit(code);
}

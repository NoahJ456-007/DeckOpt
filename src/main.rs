//! DeckOpt - per-game Steam Deck tuning that adapts to the charger and the screen.
//!
//! Watches three things and re-tunes whenever any of them changes:
//!   * which game is running (Steam AppId)
//!   * whether the Deck is on the charger (and how low the battery is)
//!   * which screen you're playing on (built-in panel, or an external monitor/TV)
//!
//! Settings come from layered profiles in profiles.toml (see that file for the format).
//! Needs root for the sysfs writes; run it through the bundled systemd service.

mod adaptive;
mod autotdp;
mod cli;
mod config;
mod detect;
mod gamescope;
mod graphics;
mod input;
mod learned;
mod names;
mod profile;
mod service;
mod tuner;
mod util;

use std::path::PathBuf;

const USAGE: &str = "usage: deckopt [-h] [-c CONFIG] [-n] [-v] [{run,status,apply,check}]\n\nDeckOpt - per-game Steam Deck tuning that adapts to the charger and the screen.\n\npositional arguments:\n  {run,status,apply,check}\n                        run: background service | status: show what would be applied | apply: apply once | check: health check\n\noptions:\n  -h, --help            show this help message and exit\n  -c, --config CONFIG\n  -n, --dry-run         log changes instead of writing them\n  -v, --verbose\n  --version             print the version and exit";

fn main() {
    let mut command = "status".to_string();
    let mut config: Option<String> = None;
    let mut dry_run = false;
    let mut verbose = false;
    let mut seen_command = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--version" => {
                println!("deckopt {} ({})", service::version(), env!("CARGO_PKG_VERSION"));
                return;
            }
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "-c" | "--config" => match args.next() {
                Some(c) => config = Some(c),
                None => usage_error("argument -c/--config: expected one argument"),
            },
            s if s.starts_with("--config=") => config = Some(s["--config=".len()..].to_string()),
            "run" | "status" | "apply" | "check" if !seen_command => {
                command = arg.clone();
                seen_command = true;
            }
            other => usage_error(&format!("argument command: invalid choice: '{other}' (choose from 'run', 'status', 'apply', 'check')")),
        }
    }
    util::set_verbose(verbose);
    let config = PathBuf::from(config.unwrap_or_else(cli::default_config));
    let code = match command.as_str() {
        "run" => service::run(&config, dry_run),
        "check" => cli::cmd_check(&config),
        "apply" => cli::cmd_status(&config, true, dry_run),
        _ => cli::cmd_status(&config, false, dry_run),
    };
    std::process::exit(code);
}

fn usage_error(msg: &str) -> ! {
    eprintln!("{}\ndeckopt: error: {msg}", USAGE.lines().next().unwrap_or(""));
    std::process::exit(2);
}

mod cli;

use std::io::Write;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use slycapture::config::AppConfig;
use slycapture::core::control::{self, Action, Request};
use slycapture::core::instance_lock::InstanceLock;
use slycapture::core::status::Status;

use crate::cli::{Cli, Commands, StartArgs, StatusArgs, StatusFormat};

/// How long `status --follow` waits before reconnecting to a stopped applet.
const FOLLOW_RETRY_INTERVAL: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => run(cli.config),
        Commands::Start(StartArgs { target, now }) => send(Action::Start {
            target,
            skip_dialog: now,
        }),
        Commands::Stop => send(Action::Stop),
        Commands::Toggle(StartArgs { target, now }) => send(Action::Toggle {
            target,
            skip_dialog: now,
        }),
        Commands::Status(args) => status(args),
        Commands::ValidateConfig => validate_config(cli.config),
    }
}

fn run(config_path_override: Option<std::path::PathBuf>) -> Result<()> {
    println!("slycapture v{}", env!("CARGO_PKG_VERSION"));
    let instance_lock = InstanceLock::acquire()?;

    let loaded = AppConfig::load(config_path_override)?;
    loaded.config.validate()?;
    match &loaded.path {
        Some(path) => println!("Loaded config from {}", path.display()),
        None => println!("No config file found; using defaults"),
    }

    #[cfg(target_os = "linux")]
    slycapture::ui::tray::run(loaded.config, instance_lock.listener().try_clone()?)?;
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("slycapture only supports Linux/X11");

    drop(instance_lock);
    Ok(())
}

fn send(action: Action) -> Result<()> {
    control::request(&Request::Action(action))?;
    Ok(())
}

fn status(args: StatusArgs) -> Result<()> {
    if !args.follow {
        let status = control::request(&Request::Status)
            .ok()
            .and_then(|response| response.status)
            .unwrap_or_else(Status::offline);
        println!("{}", format_status(&status, args.format));
        return Ok(());
    }

    let mut stdout = std::io::stdout();
    loop {
        // Errors just mean the applet isn't running (yet); report offline and retry.
        let _ = control::subscribe(|status| {
            let _ = writeln!(stdout, "{}", format_status(status, args.format));
            let _ = stdout.flush();
        });
        let _ = writeln!(stdout, "{}", format_status(&Status::offline(), args.format));
        let _ = stdout.flush();
        thread::sleep(FOLLOW_RETRY_INTERVAL);
    }
}

fn format_status(status: &Status, format: StatusFormat) -> String {
    match format {
        StatusFormat::Plain => status.format_plain(),
        StatusFormat::Json => serde_json::to_string(status).unwrap_or_default(),
        StatusFormat::Polybar => status.format_polybar(),
    }
}

fn validate_config(config_path_override: Option<std::path::PathBuf>) -> Result<()> {
    let loaded = AppConfig::load(config_path_override)?;
    loaded.config.validate()?;
    match &loaded.path {
        Some(path) => println!("Config is valid: {}", path.display()),
        None => println!("No config file found; defaults are valid"),
    }
    Ok(())
}

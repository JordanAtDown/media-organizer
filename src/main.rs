use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use media_organizer::cli::{Cli, Command, LogFormat};
use media_organizer::config::Config;
use media_organizer::daemon;
use media_organizer::health::{client, HealthServer, HealthState};
use media_organizer::logtime::LocalTime;
use media_organizer::notify::{Changes, Notifier};
use media_organizer::scanner::Scanner;
use media_organizer::shutdown::Shutdown;
use media_organizer::staging::Staging;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn init_logging(cli: &Cli, config: Option<&Config>) {
    // RUST_LOG wins over the flag, which wins over the config file.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cli.effective_log_level(config)));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(LocalTime)
        .with_writer(std::io::stdout);
    match cli.log_format {
        // Flat: `message` and the event's properties sit next to `level`, instead of
        // under a nested `fields` object, and the module path is left out.
        LogFormat::Json => builder.json().flatten_event(true).with_target(false).init(),
        // Escape codes only on a terminal: `docker logs` would show them raw.
        LogFormat::Text => builder
            .with_target(false)
            .with_ansi(std::io::stdout().is_terminal())
            .init(),
    }
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let settings = cli.settings();
    init_logging(&cli, settings.as_ref().ok().map(|s| &s.config));

    match cli.command {
        Some(Command::Healthcheck) => return healthcheck(&cli, settings),
        Some(Command::ValidateConfig) => return validate_config(settings),
        _ => {}
    }

    let settings = settings.with_context(|| format!("loading {}", cli.config.display()))?;
    match cli.command {
        Some(Command::Recover) => recover(&settings.config, &settings.state_dir),
        _ => run_daemon(&cli, settings),
    }
}

fn validate_config(
    settings: Result<media_organizer::cli::Settings, media_organizer::error::ConfigError>,
) -> Result<ExitCode> {
    let settings = settings.context("configuration is invalid")?;
    println!("{}", toml::to_string_pretty(&settings.config)?);
    eprintln!(
        "configuration is valid (state dir: {})",
        settings.state_dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn healthcheck(
    cli: &Cli,
    settings: Result<media_organizer::cli::Settings, media_organizer::error::ConfigError>,
) -> Result<ExitCode> {
    let bind = match (&cli.health_bind, settings) {
        (Some(b), _) => b.clone(),
        (None, Ok(s)) => s.config.health.bind,
        (None, Err(e)) => return Err(e).context("cannot determine the health address"),
    };
    if bind.is_empty() {
        println!("health endpoint disabled; nothing to probe");
        return Ok(ExitCode::SUCCESS);
    }
    client::probe(&bind)?;
    Ok(ExitCode::SUCCESS)
}

fn recover(config: &Config, state_dir: &std::path::Path) -> Result<ExitCode> {
    let work_root = config.work_root(state_dir);
    let mut notifier = Notifier::new(&config.notify, state_dir);
    for folder in &config.folders {
        let staging = Staging::new(
            folder.work_subdir(&work_root),
            folder.library.clone(),
            folder.file_mode(&config.permissions),
            folder.dir_mode(&config.permissions),
        )?;
        let report = staging.replay()?;
        println!(
            "{}: {} committed, {} still pending",
            folder.label(),
            report.committed.len(),
            report.failed.len()
        );
        // A replay publishes into the libraries like any batch: announce it. Now,
        // not after the debounce: this process exits and no later scan would flush it.
        if let Some(notifier) = &mut notifier {
            let mut changes = Changes::new(&folder.input, &folder.library);
            for (source, dest) in report.published() {
                changes.committed(source, dest);
            }
            notifier.announce_now(folder, changes, unix_now());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_daemon(cli: &Cli, settings: media_organizer::cli::Settings) -> Result<ExitCode> {
    let config = settings.config;
    // `Local` silently becomes UTC without tzdata/TZ, and files then land in the
    // wrong month directory: make a wrong zone obvious in the first log lines.
    info!(
        version = env!("CARGO_PKG_VERSION"),
        timezone_offset = %Local::now().offset(),
        workers = config.workers,
        dry_run = cli.dry_run,
        "media-organizer starting"
    );

    let shutdown = Shutdown::install().context("installing signal handlers")?;
    let health = Arc::new(HealthState::new(
        config.health.clone(),
        config.poll_interval_secs,
    ));

    // Bind before scanning: a taken port must fail startup, not run unsupervised.
    let server = if config.health.bind.is_empty() {
        None
    } else {
        Some(HealthServer::start(
            &config.health.bind,
            health.clone(),
            shutdown.clone(),
        )?)
    };

    let interval = Duration::from_secs(config.poll_interval_secs);
    let mut scanner =
        Scanner::new(config, settings.state_dir, cli.dry_run).context("starting the scanner")?;
    health.restore_notify(scanner.last_events());

    let summary = daemon::run(&mut scanner, &health, &shutdown, interval, cli.once);

    shutdown.request(); // stops the health thread after a --once run
    if let Some(server) = server {
        server.join();
    }
    if cli.once {
        for f in &summary.folders {
            println!(
                "{}: {} organized, {} duplicates, {} without date, {} failed",
                f.name, f.organized, f.duplicates, f.no_date, f.failed
            );
        }
    }
    info!("media-organizer stopped");
    Ok(ExitCode::SUCCESS)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

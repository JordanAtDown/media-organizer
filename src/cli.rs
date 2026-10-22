//! Command-line surface. Precedence: flag > environment variable (`MO_*`,
//! handled by clap) > config file > built-in default.

use crate::config::{self, Config};
use crate::error::ConfigError;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub const DEFAULT_CONFIG: &str = "/etc/media-organizer/config.toml";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/media-organizer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(name = "media-organizer", version, about)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "MO_CONFIG", default_value = DEFAULT_CONFIG, global = true)]
    pub config: PathBuf,

    /// Directory for the persistent caches (a volume of its own).
    #[arg(long, env = "MO_STATE_DIR", global = true)]
    pub state_dir: Option<PathBuf>,

    #[arg(
        long,
        env = "MO_LOG_FORMAT",
        value_enum,
        default_value = "text",
        global = true
    )]
    pub log_format: LogFormat,

    /// error | warn | info | debug | trace. `RUST_LOG` still wins.
    #[arg(long, env = "MO_LOG_LEVEL", global = true)]
    pub log_level: Option<String>,

    /// Shorthand for `--log-level debug`.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Run phases 1-3 and print the plan; write nothing.
    #[arg(long, env = "MO_DRY_RUN")]
    pub dry_run: bool,

    /// Do one scan, then exit.
    #[arg(long, env = "MO_ONCE")]
    pub once: bool,

    #[arg(long, env = "MO_POLL_INTERVAL_SECS")]
    pub poll_interval_secs: Option<u64>,

    #[arg(long, env = "MO_WORKERS")]
    pub workers: Option<usize>,

    /// `host:port` of the health endpoint; empty disables it.
    #[arg(long, env = "MO_HEALTH_BIND", global = true)]
    pub health_bind: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Parse and validate the configuration, then print the effective one.
    ValidateConfig,
    /// Probe the local /healthz endpoint (for the container HEALTHCHECK).
    Healthcheck,
    /// Replay the staging journal and exit.
    Recover,
}

/// The configuration with CLI/env overrides folded in and re-validated.
pub struct Settings {
    pub config: Config,
    pub state_dir: PathBuf,
}

impl Cli {
    /// Load the config file and apply the overrides.
    pub fn settings(&self) -> Result<Settings, ConfigError> {
        let mut config = config::load(&self.config)?;
        if let Some(v) = self.poll_interval_secs {
            config.poll_interval_secs = v;
        }
        if let Some(v) = self.workers {
            config.workers = v;
        }
        if let Some(v) = &self.health_bind {
            config.health.bind = v.clone();
        }
        config::validate(&config)?;

        let state_dir = self
            .state_dir
            .clone()
            .or_else(|| config.state_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));
        Ok(Settings { config, state_dir })
    }

    /// The level to log at, before `RUST_LOG` is considered.
    pub fn effective_log_level(&self, config: Option<&Config>) -> String {
        if self.verbose {
            return "debug".into();
        }
        self.log_level
            .clone()
            .or_else(|| config.map(|c| c.log_level.clone()))
            .unwrap_or_else(|| "info".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cfg_file(extra: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "{extra}\n[[folders]]\ninput = \"/in\"\noutput = \"/out\"\n"
        )
        .unwrap();
        f
    }

    fn cli(args: &[&str], file: &tempfile::NamedTempFile) -> Cli {
        let mut full = vec!["media-organizer", "--config", file.path().to_str().unwrap()];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }

    #[test]
    fn flags_override_the_config_file() {
        let f = cfg_file("poll_interval_secs = 99\nworkers = 3");
        let s = cli(&["--poll-interval-secs", "5", "--workers", "2"], &f)
            .settings()
            .unwrap();
        assert_eq!(s.config.poll_interval_secs, 5);
        assert_eq!(s.config.workers, 2);
    }

    #[test]
    fn config_file_beats_the_default() {
        let f = cfg_file("poll_interval_secs = 99");
        let s = cli(&[], &f).settings().unwrap();
        assert_eq!(s.config.poll_interval_secs, 99);
    }

    #[test]
    fn an_override_is_validated_too() {
        let f = cfg_file("");
        assert!(cli(&["--workers", "0"], &f).settings().is_err());
        assert!(cli(&["--health-bind", "nonsense"], &f).settings().is_err());
    }

    #[test]
    fn empty_health_bind_disables_the_endpoint() {
        let f = cfg_file("");
        let s = cli(&["--health-bind", ""], &f).settings().unwrap();
        assert!(s.config.health.bind.is_empty());
    }

    #[test]
    fn state_dir_precedence_is_flag_then_config_then_default() {
        let f = cfg_file("state_dir = \"/from/config\"");
        assert_eq!(
            cli(&["--state-dir", "/from/flag"], &f)
                .settings()
                .unwrap()
                .state_dir,
            PathBuf::from("/from/flag")
        );
        assert_eq!(
            cli(&[], &f).settings().unwrap().state_dir,
            PathBuf::from("/from/config")
        );
        let bare = cfg_file("");
        assert_eq!(
            cli(&[], &bare).settings().unwrap().state_dir,
            PathBuf::from(DEFAULT_STATE_DIR)
        );
    }

    #[test]
    fn log_level_precedence() {
        let f = cfg_file("log_level = \"warn\"");
        let c = config::load(f.path()).unwrap();
        assert_eq!(cli(&[], &f).effective_log_level(Some(&c)), "warn");
        assert_eq!(
            cli(&["--log-level", "trace"], &f).effective_log_level(Some(&c)),
            "trace"
        );
        assert_eq!(cli(&["-v"], &f).effective_log_level(Some(&c)), "debug");
        assert_eq!(cli(&[], &f).effective_log_level(None), "info");
    }

    #[test]
    fn subcommands_parse() {
        let f = cfg_file("");
        assert_eq!(
            cli(&["validate-config"], &f).command,
            Some(Command::ValidateConfig)
        );
        assert_eq!(
            cli(&["healthcheck"], &f).command,
            Some(Command::Healthcheck)
        );
        assert_eq!(cli(&["recover"], &f).command, Some(Command::Recover));
        assert_eq!(cli(&[], &f).command, None);
    }

    #[test]
    fn run_flags_parse() {
        let f = cfg_file("");
        let c = cli(&["--dry-run", "--once"], &f);
        assert!(c.dry_run && c.once);
    }
}

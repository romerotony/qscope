//! Command-line arguments and optional TOML configuration.

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::path::PathBuf;

/// `Auto` is a selection policy, not a backend that executes commands.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Scheduler {
  #[default]
  Auto,
  Slurm,
  Flux,
}

#[derive(Parser)]
#[command(version, about = "Monitor and manage Slurm or Flux jobs on an HPC")]
pub struct Args {
  /// Use simulated jobs without invoking scheduler clients
  #[arg(long)]
  pub demo: bool,
  #[arg(long, value_enum)]
  pub scheduler: Option<Scheduler>,
  #[arg(long)]
  pub config: Option<PathBuf>,
  /// Scheduler polling interval in seconds (default 5, minimum 1)
  #[arg(long)]
  pub refresh: Option<u64>,
  /// Stdout polling interval in seconds (default 1, minimum 1)
  #[arg(long)]
  pub stdout_refresh: Option<u64>,
  /// Resource polling interval in seconds (default 30, minimum 1)
  #[arg(long)]
  pub resource_refresh: Option<u64>,
}

/// Missing settings inherit defaults; unknown keys are rejected to catch typos.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  pub scheduler: Scheduler,
  pub refresh_interval_secs: u64,
  pub stdout_refresh_interval_secs: u64,
  pub resource_refresh_interval_secs: u64,
  pub command_timeout_secs: u64,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      scheduler: Scheduler::Auto,
      refresh_interval_secs: 5,
      stdout_refresh_interval_secs: 1,
      resource_refresh_interval_secs: 30,
      command_timeout_secs: 15,
    }
  }
}

impl Config {
  /// Resolve settings in order: built-in defaults, config file, CLI overrides.
  /// A missing default file is allowed; a missing explicitly requested file is not.
  pub fn load(args: &Args) -> Result<Self> {
    let path = args.config.clone().or_else(|| {
      std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("qscope/config.toml"))
    });
    let mut config = match path {
      Some(path) => match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Self>(&text)
          .with_context(|| format!("Invalid config {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && args.config.is_none() => {
          Self::default()
        }
        Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
      },
      None => Self::default(),
    };
    if let Some(scheduler) = args.scheduler {
      config.scheduler = scheduler;
    }
    if let Some(refresh) = args.refresh {
      config.refresh_interval_secs = refresh;
    }
    if let Some(refresh) = args.stdout_refresh {
      config.stdout_refresh_interval_secs = refresh;
    }
    if let Some(refresh) = args.resource_refresh {
      config.resource_refresh_interval_secs = refresh;
    }
    // Reject zero to avoid an unbounded polling loop.
    ensure!(
      config.resource_refresh_interval_secs >= 1,
      "Resource refresh interval must be at least 1 second"
    );
    ensure!(
      config.stdout_refresh_interval_secs >= 1,
      "Stdout refresh interval must be at least 1 second"
    );
    ensure!(
      config.refresh_interval_secs >= 1,
      "Refresh interval must be at least 1 second"
    );
    ensure!(
      config.command_timeout_secs > 0,
      "Command timeout must be positive"
    );
    Ok(config)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn refresh_allows_one_second_but_rejects_zero() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), "scheduler = 'auto'\n").unwrap();
    let mut args = Args {
      demo: true,
      scheduler: None,
      config: Some(file.path().to_path_buf()),
      refresh: None,
      stdout_refresh: None,
      resource_refresh: None,
    };
    let defaults = Config::load(&args).unwrap();
    assert_eq!(defaults.refresh_interval_secs, 5);
    assert_eq!(defaults.stdout_refresh_interval_secs, 1);
    assert_eq!(defaults.resource_refresh_interval_secs, 30);
    args.refresh = Some(0);
    assert!(Config::load(&args).is_err());
    args.refresh = Some(1);
    assert_eq!(Config::load(&args).unwrap().refresh_interval_secs, 1);
    args.refresh = Some(30);
    assert_eq!(Config::load(&args).unwrap().refresh_interval_secs, 30);
    args.stdout_refresh = Some(0);
    assert!(Config::load(&args).is_err());
    args.stdout_refresh = Some(2);
    let config = Config::load(&args).unwrap();
    assert_eq!(config.stdout_refresh_interval_secs, 2);
    assert_eq!(config.refresh_interval_secs, 30);
    args.resource_refresh = Some(0);
    assert!(Config::load(&args).is_err());
    args.resource_refresh = Some(60);
    assert_eq!(
      Config::load(&args).unwrap().resource_refresh_interval_secs,
      60
    );
  }
  #[test]
  fn config_defaults_and_validation() {
    let config: Config = toml::from_str("scheduler = 'flux'").unwrap();
    assert_eq!(config.scheduler, Scheduler::Flux);
    assert_eq!(config.refresh_interval_secs, 5);
    assert_eq!(config.stdout_refresh_interval_secs, 1);
    assert_eq!(config.resource_refresh_interval_secs, 30);
    assert!(toml::from_str::<Config>("scheduler = 'pbs'").is_err());
    assert!(toml::from_str::<Config>("typo = 1").is_err());
  }
}

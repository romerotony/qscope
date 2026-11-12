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
  /// Polling interval in seconds (minimum 5)
  #[arg(long)]
  pub refresh: Option<u64>,
}

/// Missing settings inherit defaults; unknown keys are rejected to catch typos.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  pub scheduler: Scheduler,
  pub refresh_interval_secs: u64,
  pub command_timeout_secs: u64,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      scheduler: Scheduler::Auto,
      refresh_interval_secs: 10,
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
    // A lower bound protects the scheduler from overly aggressive polling.
    ensure!(
      config.refresh_interval_secs >= 5,
      "Refresh interval must be at least 5 seconds"
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
  fn config_defaults_and_validation() {
    let config: Config = toml::from_str("scheduler = 'flux'").unwrap();
    assert_eq!(config.scheduler, Scheduler::Flux);
    assert_eq!(config.refresh_interval_secs, 10);
    assert!(toml::from_str::<Config>("scheduler = 'pbs'").is_err());
    assert!(toml::from_str::<Config>("typo = 1").is_err());
  }
}

//! Scheduler adapters: invoke installed CLI clients and normalize their output.
//! No shell or SSH layer is used for scheduler operations.

use crate::config::Scheduler;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::time::Duration;
use tokio::{process::Command, time::timeout};

/// Common display fields. IDs remain strings to preserve Slurm array syntax and
/// large Flux IDs; native states are not forced into a Slurm-specific model.
#[derive(Clone, Debug)]
pub struct Job {
  pub id: String,
  pub name: String,
  pub owner: String,
  pub state: String,
  pub queue: String,
  pub runtime: String,
  pub nodes: String,
  pub cores: String,
  pub reason: String,
}

/// A concrete scheduler selected at startup; `kind` must not be `Auto`.
#[derive(Debug)]
pub struct Backend {
  pub kind: Scheduler,
  pub timeout: Duration,
}

impl Backend {
  pub async fn nodes(&self, hosts: &str) -> Result<String> {
    ensure!(
      self.kind == Scheduler::Slurm,
      "Node metadata requires Slurm"
    );
    // Hostlists remain one CLI argument; reject options and non-hostlist syntax.
    ensure!(
      !hosts.is_empty()
        && !hosts.starts_with('-')
        && hosts
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || "_-.,[]".contains(c)),
      "Unsafe node list: {hosts}"
    );
    self.run("scontrol", &["show", "node", hosts]).await
  }

  pub async fn info(&self, id: &str, attribute: &str) -> Result<String> {
    validate_id(id)?;
    ensure!(
      self.kind == Scheduler::Flux && matches!(attribute, "jobspec" | "R"),
      "Unsupported job attribute"
    );
    self.run("flux", &["job", "info", id, attribute]).await
  }

  pub fn name(&self) -> &'static str {
    match self.kind {
      Scheduler::Slurm => "Slurm",
      Scheduler::Flux => "Flux",
      Scheduler::Auto => unreachable!(),
    }
  }

  /// Pass arguments directly to the executable, capture diagnostics, and bound
  /// execution time. Dropping a timed-out command kills its direct child process.
  async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command
      .args(args)
      .kill_on_drop(true)
      .stdin(std::process::Stdio::null());
    // Keep inherited Slurm display/filter preferences from changing our protocol.
    if program == "squeue" {
      for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SQUEUE_") {
          command.env_remove(key);
        }
      }
      command.env_remove("SLURM_CLUSTERS");
    }
    let output = timeout(self.timeout, command.output())
      .await
      .with_context(|| format!("{program} timed out after {}s", self.timeout.as_secs()))?
      .with_context(|| {
        format!("Cannot run {program}; check PATH and installed scheduler clients")
      })?;
    ensure!(
      output.status.success(),
      "{program} failed ({}): {}",
      output.status,
      String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("Scheduler returned non-UTF-8 output")
  }

  /// List the current user's jobs in the accessible local scheduler context.
  /// Slurm uses an explicit format to avoid version-dependent JSON schemas;
  /// Flux JSON provides structured data without a default job-count cap.
  pub async fn jobs(&self) -> Result<Vec<Job>> {
    match self.kind {
      Scheduler::Slurm => parse_slurm(
        &self
          .run(
            "squeue",
            &[
              "--local",
              "--me",
              "--array",
              "--noheader",
              "--states=all",
              "--format=%i\u{1f}%j\u{1f}%u\u{1f}%T\u{1f}%P\u{1f}%M\u{1f}%D\u{1f}%C\u{1f}%R",
            ],
          )
          .await?,
      ),
      Scheduler::Flux => parse_flux(&self.run("flux", &["jobs", "--json", "--count=0"]).await?),
      Scheduler::Auto => unreachable!(),
    }
  }

  /// Fetch a detail snapshot, retaining scheduler-specific fields for inspection.
  pub async fn details(&self, id: &str) -> Result<String> {
    validate_id(id)?;
    match self.kind {
      Scheduler::Slurm => self.run("scontrol", &["show", "job", id]).await,
      Scheduler::Flux => {
        let text = self.run("flux", &["jobs", "--json", id]).await?;
        Ok(serde_json::to_string_pretty(
          &serde_json::from_str::<Value>(&text)?,
        )?)
      }
      Scheduler::Auto => unreachable!(),
    }
  }

  /// Request cancellation once. Success means the client accepted the request,
  /// not that the job has already disappeared from the scheduler.
  pub async fn cancel(&self, id: &str) -> Result<()> {
    validate_id(id)?;
    match self.kind {
      Scheduler::Slurm => {
        self.run("scancel", &[id]).await?;
      }
      Scheduler::Flux => {
        self.run("flux", &["cancel", id]).await?;
      }
      Scheduler::Auto => unreachable!(),
    }
    Ok(())
  }
}

// Direct arguments avoid shell injection, but IDs must also be prevented from
// becoming CLI options or comma-separated lists that target additional jobs.
fn validate_id(id: &str) -> Result<()> {
  ensure!(
    !id.is_empty()
      && id
        .chars()
        .all(|c| c.is_alphanumeric() || "_+.-".contains(c))
      && !id.starts_with('-'),
    "Unsafe job ID: {id}"
  );
  Ok(())
}

/// Probe connectivity with read-only job queries, not just executable presence.
/// Return both backends if usable so the caller can ask rather than guess.
pub async fn detect(kind: Scheduler, seconds: u64) -> Result<Vec<Backend>> {
  let make = |kind| Backend {
    kind,
    timeout: Duration::from_secs(seconds),
  };
  if kind != Scheduler::Auto {
    let backend = make(kind);
    backend
      .jobs()
      .await
      .with_context(|| format!("{} is not usable", backend.name()))?;
    return Ok(vec![backend]);
  }
  let slurm = make(Scheduler::Slurm);
  let flux = make(Scheduler::Flux);
  // Independent probes run concurrently, each with its own command timeout.
  let (s, f) = tokio::join!(slurm.jobs(), flux.jobs());
  let mut found = Vec::new();
  let mut errors = Vec::new();
  for (backend, result) in [(slurm, s), (flux, f)] {
    match result {
      Ok(_) => found.push(backend),
      Err(error) => errors.push(format!("{}: {error:#}", backend.name())),
    }
  }
  if found.is_empty() {
    bail!(
      "No usable scheduler. Load the cluster's client modules or check your broker/controller connection.\n{}",
      errors.join("\n")
    );
  }
  Ok(found)
}

// The ASCII unit separator matches the requested squeue format and permits
// ordinary spaces in job names. Reject malformed rows instead of shifting fields.
fn parse_slurm(text: &str) -> Result<Vec<Job>> {
  text
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|line| {
      let fields: Vec<_> = line.split('\u{1f}').map(str::trim).collect();
      ensure!(
        fields.len() == 9,
        "Unexpected squeue output: expected 9 fields, got {}",
        fields.len()
      );
      Ok(Job {
        id: fields[0].into(),
        name: fields[1].into(),
        owner: fields[2].into(),
        state: fields[3].into(),
        queue: fields[4].into(),
        runtime: fields[5].into(),
        nodes: fields[6].into(),
        cores: fields[7].into(),
        reason: fields[8].into(),
      })
    })
    .collect()
}

/// Missing Flux attributes mean unavailable, not zero resources or empty state.
fn field(value: &Value) -> String {
  match value {
    Value::Null => "—".into(),
    Value::String(s) => s.clone(),
    _ => value.to_string(),
  }
}

// The CLI normally emits state names. Also accept numeric Flux state constants,
// preserving unrecognized values rather than silently classifying them incorrectly.
fn flux_state(value: &Value) -> String {
  if let Some(number) = value.as_u64() {
    match number {
      1 => "NEW",
      2 => "DEPEND",
      4 => "PRIORITY",
      8 => "SCHED",
      16 => "RUN",
      32 => "CLEANUP",
      64 => "INACTIVE",
      _ => return number.to_string(),
    }
    .into()
  } else {
    field(value)
  }
}

fn parse_flux(text: &str) -> Result<Vec<Job>> {
  let root: Value = serde_json::from_str(text).context("Invalid Flux JSON")?;
  let jobs = root["jobs"]
    .as_array()
    .context("Flux JSON has no jobs array")?;
  jobs
    .iter()
    .map(|j| {
      // Convert the JSON integer directly to text, never through f64:
      // Flux's 64-bit identifiers can exceed floating-point precision.
      let id = field(&j["id"]);
      validate_id(&id).context("Invalid Flux job ID")?;
      let runtime = j["runtime"]
        .as_f64()
        .map(|s| format!("{:.0}s", s.max(0.0)))
        .unwrap_or_else(|| "—".into());
      Ok(Job {
        id,
        name: field(&j["name"]),
        owner: field(j.get("username").unwrap_or(&j["userid"])),
        state: flux_state(&j["state"]),
        queue: field(&j["queue"]),
        runtime,
        nodes: field(&j["nnodes"]),
        cores: field(&j["ncores"]),
        // Pending jobs may have scheduler annotations; running jobs
        // generally provide an allocated nodelist instead.
        reason: field(
          j.pointer("/annotations/sched/reason_pending")
            .unwrap_or(&j["nodelist"]),
        ),
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  #[tokio::test]
  async fn node_queries_reject_options_and_non_hostlist_input() {
    let backend = Backend {
      kind: Scheduler::Slurm,
      timeout: Duration::from_secs(1),
    };
    for hosts in ["", "-a", "n1 n2", "n1;id", "n1\nn2", "n1/../n2"] {
      assert!(
        backend
          .nodes(hosts)
          .await
          .unwrap_err()
          .to_string()
          .contains("Unsafe node list")
      );
    }
  }
  #[tokio::test]
  async fn command_errors_are_actionable() {
    let backend = Backend {
      kind: Scheduler::Flux,
      timeout: Duration::from_secs(2),
    };
    let error = backend
      .run("/bin/sh", &["-c", "echo denied >&2; exit 7"])
      .await
      .unwrap_err();
    assert!(error.to_string().contains("denied"));
    let error = backend
      .run("/nonexistent/qscope-test-command", &[])
      .await
      .unwrap_err();
    assert!(error.to_string().contains("check PATH"));
  }

  #[tokio::test]
  async fn slow_commands_time_out() {
    let backend = Backend {
      kind: Scheduler::Flux,
      timeout: Duration::from_millis(20),
    };
    let error = backend.run("/bin/sleep", &["2"]).await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
  }

  #[test]
  fn flux_cli_string_state_and_runtime() {
    let jobs = parse_flux(r#"{"jobs":[{"id":123,"jobid":"ƒ38","username":"alice","name":"sim","state":"RUN","runtime":12.5,"ncores":8,"nnodes":1,"nodelist":"node0"}]}"#).unwrap();
    assert_eq!(jobs[0].state, "RUN");
    assert_eq!(jobs[0].owner, "alice");
    assert_eq!(jobs[0].cores, "8");
    assert_eq!(jobs[0].reason, "node0");
  }
  #[test]
  fn slurm_array_ids_and_empty_results() {
    let jobs = parse_slurm(
      &[
        "123_4",
        "my job",
        "alice",
        "PENDING",
        "batch",
        "0:00",
        "2",
        "8",
        "(Resources)",
      ]
      .join("\u{1f}"),
    )
    .unwrap();
    assert_eq!(jobs[0].id, "123_4");
    assert_eq!(jobs[0].state, "PENDING");
    assert!(parse_slurm("").unwrap().is_empty());
    assert!(parse_slurm("bad output").is_err());
  }
  #[test]
  fn flux_numeric_state_and_large_id() {
    let jobs = parse_flux(r#"{"jobs":[{"id":18446744073709551614,"name":"sim","state":8,"userid":1000,"annotations":{"sched":{"reason_pending":"resources"}}}]}"#).unwrap();
    assert_eq!(jobs[0].id, "18446744073709551614");
    assert_eq!(jobs[0].state, "SCHED");
    assert_eq!(jobs[0].reason, "resources");
    assert_eq!(jobs[0].runtime, "—");
    assert!(parse_flux(r#"{"jobs":[]}"#).unwrap().is_empty());
    assert!(parse_flux("{}").is_err());
  }
  #[test]
  fn rejects_option_and_multi_job_injection() {
    for id in ["", "--all", "1,2", "1;echo", "1\n2"] {
      assert!(validate_id(id).is_err());
    }
    for id in ["123", "123_4", "123+1", "ƒabc"] {
      assert!(validate_id(id).is_ok());
    }
  }
}

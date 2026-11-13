//! Select between real scheduler commands and an isolated in-memory simulation.

use crate::{
  config::Scheduler,
  metadata::Metadata,
  output::{self, Output, Reader},
  scheduler::{Backend, Job},
};
use anyhow::{Result, bail, ensure};
use std::time::Instant;

/// The demo variant contains no live backend, so even cancellation cannot invoke
/// a scheduler command. The worker uses the same interface for both modes.
pub enum Source {
  Live(Backend),
  Demo(Demo),
}

impl Source {
  pub async fn resources(
    &mut self,
    id: &str,
    cache: &mut Metadata,
  ) -> Result<crate::resources::Resources> {
    match self {
      Self::Live(backend) => cache.resources(backend, id).await,
      Self::Demo(demo) => {
        demo.update_runtimes();
        let job = demo
          .jobs
          .iter()
          .find(|job| job.id == id)
          .ok_or_else(|| anyhow::anyhow!("Unknown demo job: {id}"))?;
        let requested = crate::resources::Amounts {
          nodes: Some(job.nodes.clone()),
          cpus: Some(job.cores.clone()),
          tasks: Some(
            if job.nodes == "4" {
              "32"
            } else if job.queue == "gpu" {
              "1"
            } else {
              "8"
            }
            .into(),
          ),
          gpus: Some(if job.queue == "gpu" { "2" } else { "0" }.into()),
          memory: Some("64 GiB/node".into()),
        };
        let allocated = if matches!(job.state.as_str(), "RUN" | "RUNNING") {
          requested.clone()
        } else {
          crate::resources::Amounts::default()
        };
        Ok(crate::resources::Resources {
          requested,
          allocated,
          elapsed: Some(job.runtime.clone()),
          limit: Some("02:00:00".into()),
          hosts: if matches!(job.state.as_str(), "RUN" | "RUNNING") {
            Some(job.reason.clone())
          } else {
            None
          },
        })
      }
    }
  }

  /// Read local stdout only for Slurm. Flux output can live in its KVS and
  /// needs a separate implementation; do not guess a Slurm filename for it.
  pub async fn output(
    &mut self,
    id: &str,
    reader: &mut Reader,
    position: output::Position,
    cache: &mut Metadata,
  ) -> Result<Output> {
    match self {
      Self::Live(backend) if backend.kind == Scheduler::Slurm => {
        let path = match reader.path(id) {
          Some(path) => path,
          None => {
            let path = output::slurm_path(&cache.details(backend, id, false).await?)?;
            reader.remember(id, path.clone());
            path
          }
        };
        reader.read(path, position).await
      }
      Self::Live(_) => bail!("Stdout preview currently supports Slurm only"),
      Self::Demo(demo) => {
        let job = demo
          .jobs
          .iter()
          .find(|job| job.id == id)
          .ok_or_else(|| anyhow::anyhow!("Unknown demo job: {id}"))?;
        let mut text = format!(
          "DEMO — simulated stdout; no files are read.\nJob {}: {}\nState: {}\n",
          job.id, job.name, job.state
        );
        if matches!(job.state.as_str(), "PENDING" | "SCHED" | "DEPEND") {
          text.push_str("Waiting to start; stdout has not been created yet.\n");
        } else {
          for step in 1..=demo.started.elapsed().as_secs().min(100) + 5 {
            text.push_str(&format!(
              "step {step:03}: residual = {:.6}\n",
              1.0 / step as f64
            ));
          }
          if matches!(job.state.as_str(), "FAILED" | "CANCELLED" | "CANCELED") {
            text.push_str(&format!("Job stopped: {}\n", job.reason));
          }
        }
        Ok(Output {
          path: format!("DEMO /demo/logs/{}.out", job.id),
          text,
          page: None,
        })
      }
    }
  }

  pub fn demo(kind: Scheduler) -> Self {
    Self::Demo(Demo::new(kind))
  }

  pub fn name(&self) -> &'static str {
    match self {
      Self::Live(backend) => backend.name(),
      Self::Demo(demo) => match demo.kind {
        Scheduler::Flux => "DEMO · Flux (simulated)",
        _ => "DEMO · Slurm (simulated)",
      },
    }
  }

  pub async fn jobs(&mut self) -> Result<Vec<Job>> {
    match self {
      Self::Live(backend) => backend.jobs().await,
      Self::Demo(demo) => {
        demo.update_runtimes();
        Ok(demo.jobs.clone())
      }
    }
  }

  pub async fn details(&mut self, id: &str, cache: &mut Metadata) -> Result<String> {
    let (kind, raw) = match self {
      Self::Live(backend) => (backend.kind, cache.details(backend, id, true).await?),
      Self::Demo(demo) => (demo.kind, demo.details(id)?),
    };
    match kind {
      Scheduler::Flux => crate::details::flux(&raw),
      _ => {
        let formatted = crate::details::slurm(&raw)?;
        Ok(if matches!(self, Self::Demo(_)) {
          format!("DEMO — simulated data; no controller or files accessed.\n\n{formatted}")
        } else {
          formatted
        })
      }
    }
  }

  pub async fn cancel(&mut self, id: &str) -> Result<()> {
    match self {
      Self::Live(backend) => backend.cancel(id).await,
      Self::Demo(demo) => demo.cancel(id),
    }
  }
}

pub struct Demo {
  kind: Scheduler,
  started: Instant,
  jobs: Vec<Job>,
}

impl Demo {
  fn new(kind: Scheduler) -> Self {
    // Auto has a deterministic default and never initiates live detection.
    let kind = if kind == Scheduler::Flux {
      Scheduler::Flux
    } else {
      Scheduler::Slurm
    };
    let states = if kind == Scheduler::Flux {
      ["RUN", "RUN", "SCHED", "DEPEND", "COMPLETED", "FAILED"]
    } else {
      [
        "RUNNING",
        "RUNNING",
        "PENDING",
        "PENDING",
        "COMPLETED",
        "FAILED",
      ]
    };
    let names = [
      "climate-model",
      "gpu-training",
      "mesh-refinement",
      "postprocess",
      "checkpoint-test",
      "memory-test",
    ];
    let jobs = names
      .iter()
      .enumerate()
      .map(|(i, name)| Job {
        id: if kind == Scheduler::Slurm && i == 2 {
          "10423_7".into()
        } else {
          (10421 + i).to_string()
        },
        name: (*name).into(),
        owner: "demo-user".into(),
        state: states[i].into(),
        queue: if i == 1 { "gpu" } else { "batch" }.into(),
        runtime: match i {
          0 => "120s",
          1 => "45s",
          4 => "360s",
          5 => "18s",
          _ => "—",
        }
        .into(),
        nodes: if i == 0 { "4" } else { "1" }.into(),
        cores: if i == 0 { "128" } else { "8" }.into(),
        reason: match i {
          0 => "node[01-04]",
          1 => "gpu01",
          2 => "Resources",
          3 => "Dependency",
          4 => "ExitCode=0",
          _ => "Out of memory",
        }
        .into(),
      })
      .collect();
    Self {
      kind,
      started: Instant::now(),
      jobs,
    }
  }

  fn update_runtimes(&mut self) {
    // Advance only active runtimes on refresh. Terminal states stay visible
    // for inspecting results; all state changes reset when the app restarts.
    let elapsed = self.started.elapsed().as_secs();
    for (i, job) in self.jobs.iter_mut().enumerate() {
      if matches!(job.state.as_str(), "RUN" | "RUNNING") {
        job.runtime = format!("{}s", elapsed + if i == 0 { 120 } else { 45 });
      }
    }
  }

  fn details(&mut self, id: &str) -> Result<String> {
    self.update_runtimes();
    let job = self
      .jobs
      .iter()
      .find(|job| job.id == id)
      .ok_or_else(|| anyhow::anyhow!("Unknown demo job: {id}"))?;
    if self.kind == Scheduler::Flux {
      Ok(serde_json::to_string_pretty(&serde_json::json!({
          "demo": true, "id": job.id, "name": job.name,
          "username": job.owner, "state": job.state, "queue": job.queue,
          "runtime_display": job.runtime, "nnodes": job.nodes,
          "ncores": job.cores, "context": job.reason,
          "note": "Simulated data only. No Flux broker is contacted."
      }))?)
    } else {
      Ok(format!(
        "DEMO — simulated Slurm job; no controller is contacted.\n\nJobId={} JobName={}\nUserId={} JobState={}\nPartition={} RunTime={}\nNumNodes={} NumCPUs={}\nReason={}\nWorkDir=/demo/work/{}\nStdOut=/demo/logs/{}.out\nStdErr=/demo/logs/{}.err\n\nPaths are illustrative; no files are accessed.",
        job.id,
        job.name,
        job.owner,
        job.state,
        job.queue,
        job.runtime,
        job.nodes,
        job.cores,
        job.reason,
        job.name,
        job.id,
        job.id
      ))
    }
  }

  fn cancel(&mut self, id: &str) -> Result<()> {
    // Snapshot the runtime before moving to a terminal state.
    self.update_runtimes();
    let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
      bail!("Unknown demo job: {id}");
    };
    ensure!(
      !matches!(
        job.state.as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED" | "CANCELED"
      ),
      "Demo job {id} is already finished"
    );
    job.state = if self.kind == Scheduler::Flux {
      "CANCELED"
    } else {
      "CANCELLED"
    }
    .into();
    job.reason = "Canceled in demo; no real job was affected".into();
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[tokio::test]
  async fn demo_resources_distinguish_pending_and_running() {
    for kind in [Scheduler::Slurm, Scheduler::Flux] {
      let mut source = Source::demo(kind);
      let jobs = source.jobs().await.unwrap();
      let running = source
        .resources(&jobs[0].id, &mut Metadata::default())
        .await
        .unwrap();
      assert_eq!(running.requested.cpus.as_deref(), Some("128"));
      assert_eq!(running.requested.tasks.as_deref(), Some("32"));
      assert_eq!(running.allocated.tasks.as_deref(), Some("32"));
      assert_eq!(running.allocated.cpus.as_deref(), Some("128"));
      let pending = source
        .resources(&jobs[2].id, &mut Metadata::default())
        .await
        .unwrap();
      assert!(pending.requested.cpus.is_some());
      assert!(pending.allocated.cpus.is_none());
      assert!(pending.allocated.tasks.is_none());
      assert!(pending.hosts.is_none());
      assert!(
        source
          .resources("unknown", &mut Metadata::default())
          .await
          .is_err()
      );
    }
  }

  #[tokio::test]
  async fn demo_stdout_is_simulated_for_both_backends() {
    for kind in [Scheduler::Slurm, Scheduler::Flux] {
      let mut source = Source::demo(kind);
      let jobs = source.jobs().await.unwrap();
      let mut reader = Reader::default();
      let output = source
        .output(
          &jobs[0].id,
          &mut reader,
          output::Position::Tail,
          &mut Metadata::default(),
        )
        .await
        .unwrap();
      assert!(output.path.starts_with("DEMO"));
      assert!(output.text.contains("step 001"));
      assert!(
        source
          .output(
            &jobs[2].id,
            &mut reader,
            output::Position::Tail,
            &mut Metadata::default()
          )
          .await
          .unwrap()
          .text
          .contains("Waiting to start")
      );
      assert!(
        source
          .output(
            "unknown",
            &mut reader,
            output::Position::Tail,
            &mut Metadata::default()
          )
          .await
          .is_err()
      );
    }
  }

  #[tokio::test]
  async fn live_flux_stdout_does_not_invoke_clients() {
    let mut source = Source::Live(Backend {
      kind: Scheduler::Flux,
      timeout: std::time::Duration::from_millis(1),
    });
    let error = source
      .output(
        "1",
        &mut Reader::default(),
        output::Position::Tail,
        &mut Metadata::default(),
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("Slurm only"));
  }

  #[tokio::test]
  async fn demo_auto_defaults_to_slurm_and_exercises_details() {
    let mut source = Source::demo(Scheduler::Auto);
    assert!(source.name().contains("DEMO"));
    assert!(source.name().contains("Slurm"));
    let jobs = source.jobs().await.unwrap();
    assert_eq!(jobs.len(), 6);
    assert_eq!(jobs[0].state, "RUNNING");
    assert!(jobs.iter().any(|job| job.id.contains('_')));
    assert!(
      source
        .details(&jobs[0].id, &mut Metadata::default())
        .await
        .unwrap()
        .contains("Overview")
    );
  }

  #[tokio::test]
  async fn cancellation_is_local_and_persists_across_refreshes() {
    for kind in [Scheduler::Slurm, Scheduler::Flux] {
      let mut source = Source::demo(kind);
      let id = source.jobs().await.unwrap()[0].id.clone();
      source.cancel(&id).await.unwrap();
      let jobs = source.jobs().await.unwrap();
      assert!(matches!(jobs[0].state.as_str(), "CANCELLED" | "CANCELED"));
      assert!(source.cancel(&id).await.is_err());
      assert_eq!(
        Source::demo(kind).jobs().await.unwrap()[0].state,
        if kind == Scheduler::Flux {
          "RUN"
        } else {
          "RUNNING"
        }
      );
    }
  }

  #[tokio::test]
  async fn flux_demo_details_are_formatted() {
    let mut source = Source::demo(Scheduler::Flux);
    let jobs = source.jobs().await.unwrap();
    assert_eq!(jobs[2].state, "SCHED");
    let details = source
      .details(&jobs[0].id, &mut Metadata::default())
      .await
      .unwrap();
    assert!(details.starts_with("Overview\n"));
    assert!(details.contains("Demo\n"));
    assert!(details.contains("Simulated"));
    assert!(!details.contains("{\""));
  }

  #[tokio::test]
  async fn unknown_and_finished_jobs_cannot_be_canceled() {
    let mut source = Source::demo(Scheduler::Slurm);
    assert!(source.cancel("unknown").await.is_err());
    assert!(
      source
        .details("unknown", &mut Metadata::default())
        .await
        .is_err()
    );
    let jobs = source.jobs().await.unwrap();
    assert!(source.cancel(&jobs[4].id).await.is_err());
  }

  #[test]
  fn active_runtimes_advance_but_finished_runtimes_do_not() {
    let mut demo = Demo::new(Scheduler::Slurm);
    demo.started = Instant::now() - std::time::Duration::from_secs(30);
    demo.update_runtimes();
    assert_eq!(demo.jobs[0].runtime, "150s");
    assert_eq!(demo.jobs[4].runtime, "360s");
  }
}

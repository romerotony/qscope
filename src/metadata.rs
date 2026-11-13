//! Worker-owned, bounded metadata cache shared by resources and stdout lookup.

use crate::{
  config::Scheduler,
  resources::Resources,
  scheduler::{Backend, Job},
};
use anyhow::Result;
use std::{
  collections::{HashMap, HashSet, VecDeque},
  future::Future,
  time::{Duration, Instant},
};

const CAPACITY: usize = 128;
const MAX_BYTES: usize = 8 * 1024 * 1024;

struct Entry {
  id: String,
  kind: &'static str,
  value: Result<String, String>,
  expires: Instant,
  sampled: Instant,
}

type Signature = (String, String, String, String);

pub struct Metadata {
  entries: VecDeque<Entry>,
  signatures: HashMap<String, Option<Signature>>,
  pub dynamic_ttl: Duration,
}

impl Default for Metadata {
  fn default() -> Self {
    Self::new(Duration::from_secs(30))
  }
}

impl Metadata {
  pub fn new(dynamic_ttl: Duration) -> Self {
    Self {
      entries: VecDeque::new(),
      signatures: HashMap::new(),
      dynamic_ttl,
    }
  }

  pub fn clear(&mut self) {
    self.entries.clear();
    self.signatures.clear();
  }

  pub fn invalidate(&mut self, id: &str) {
    self.entries.retain(|entry| entry.id != id);
    self.signatures.remove(id);
  }

  /// Track only cached jobs, not another full copy of a potentially huge array.
  /// State, allocation-size, and name changes invalidate reusable metadata.
  pub fn observe(&mut self, jobs: &[Job]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut changed = Vec::new();
    for job in jobs {
      if let Some(previous) = self.signatures.get_mut(&job.id) {
        seen.insert(job.id.clone());
        let current = (
          job.state.clone(),
          job.nodes.clone(),
          job.cores.clone(),
          job.name.clone(),
        );
        if previous.as_ref() != Some(&current) {
          changed.push(job.id.clone());
        }
        *previous = Some(current);
      }
    }
    let missing: Vec<_> = self
      .signatures
      .keys()
      .filter(|id| !seen.contains(*id))
      .cloned()
      .collect();
    for id in &missing {
      self.invalidate(id);
    }
    for id in &changed {
      self.entries.retain(|entry| &entry.id != id);
    }
    changed.extend(missing);
    changed
  }

  async fn fetch<F, Fut>(
    &mut self,
    id: &str,
    kind: &'static str,
    ttl: Duration,
    fetch: F,
  ) -> Result<String>
  where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
  {
    if let Some(index) = self
      .entries
      .iter()
      .position(|entry| entry.id == id && entry.kind == kind)
    {
      let entry = self.entries.remove(index).unwrap();
      if entry.expires > Instant::now() {
        let value = entry.value.clone();
        self.entries.push_back(entry);
        return value.map_err(anyhow::Error::msg);
      }
    }
    let value = fetch().await.map_err(|error| format!("{error:#}"));
    let lifetime = if value.is_ok() {
      ttl
    } else {
      Duration::from_secs(5)
    };
    let sampled = Instant::now();
    let entry = Entry {
      id: id.into(),
      kind,
      value: value.clone(),
      expires: sampled + lifetime,
      sampled,
    };
    let bytes = |entry: &Entry| match &entry.value {
      Ok(value) | Err(value) => value.len(),
    };
    // Oversized individual replies remain usable, but are never retained.
    if bytes(&entry) <= MAX_BYTES {
      let mut used: usize = self.entries.iter().map(bytes).sum();
      while self.entries.len() >= CAPACITY || used + bytes(&entry) > MAX_BYTES {
        if let Some(old) = self.entries.pop_front() {
          used -= bytes(&old);
        } else {
          break;
        }
      }
      self.entries.push_back(entry);
      self
        .signatures
        .retain(|id, _| self.entries.iter().any(|entry| &entry.id == id));
      self.signatures.entry(id.into()).or_insert(None);
    }
    value.map_err(anyhow::Error::msg)
  }

  pub async fn details(&mut self, backend: &Backend, id: &str, fresh: bool) -> Result<String> {
    if fresh {
      self
        .entries
        .retain(|entry| entry.id != id || entry.kind != "details");
    }
    self
      .fetch(id, "details", self.dynamic_ttl, || backend.details(id))
      .await
  }

  pub async fn resources(&mut self, backend: &Backend, id: &str) -> Result<Resources> {
    let details = self.details(backend, id, false).await?;
    if backend.kind == Scheduler::Slurm {
      let mut resources = crate::resources::slurm(&details)?;
      let state = details
        .split_whitespace()
        .find_map(|part| part.strip_prefix("JobState="))
        .unwrap_or("");
      self.advance_elapsed(id, state, &mut resources);
      return Ok(resources);
    }
    let spec = self
      .fetch(id, "jobspec", Duration::from_secs(60), || {
        backend.info(id, "jobspec")
      })
      .await?;
    let job: serde_json::Value = serde_json::from_str(&details)?;
    let allocation = if matches!(job["state"].as_str(), Some("RUN" | "CLEANUP" | "INACTIVE")) {
      Some(
        self
          .fetch(id, "allocation", Duration::from_secs(60), || {
            backend.info(id, "R")
          })
          .await,
      )
    } else {
      None
    };
    let mut resources = crate::resources::flux(
      &details,
      &spec,
      allocation
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(String::as_str),
    )?;
    if let Some(Err(error)) = allocation {
      resources.note = format!("Allocation unavailable: {error:#}");
    }
    self.advance_elapsed(id, job["state"].as_str().unwrap_or(""), &mut resources);
    Ok(resources)
  }

  fn advance_elapsed(&self, id: &str, state: &str, resources: &mut Resources) {
    // A resource response can reuse details fetched by the inspector or stdout
    // resolver. Account for that sample's age instead of resetting its clock.
    let age = self
      .entries
      .iter()
      .find(|entry| entry.id == id && entry.kind == "details")
      .map_or(0, |entry| entry.sampled.elapsed().as_secs());
    if let Some(elapsed) = &mut resources.elapsed {
      *elapsed = crate::duration::elapsed_since(elapsed, state, age);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  fn job(state: &str) -> Job {
    Job {
      id: "1".into(),
      name: "sim".into(),
      owner: "alice".into(),
      state: state.into(),
      queue: "batch".into(),
      runtime: "10s".into(),
      nodes: "1".into(),
      cores: "8".into(),
      reason: String::new(),
    }
  }

  #[tokio::test]
  async fn shared_metadata_ttl_errors_and_explicit_invalidation_bound_fetches() {
    let mut cache = Metadata::default();
    let calls = AtomicUsize::new(0);
    for _ in 0..20 {
      let value = cache
        .fetch("1", "details", Duration::from_secs(30), || async {
          calls.fetch_add(1, Ordering::SeqCst);
          Ok("shared resource and stdout metadata".into())
        })
        .await
        .unwrap();
      assert!(value.contains("shared"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cache.entries.front_mut().unwrap().expires = Instant::now() - Duration::from_secs(1);
    cache
      .fetch("1", "details", Duration::from_secs(30), || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok("refreshed".into())
      })
      .await
      .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for _ in 0..10 {
      assert!(
        cache
          .fetch("2", "details", Duration::from_secs(30), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("unavailable")
          })
          .await
          .is_err()
      );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    cache.clear();
    assert!(cache.entries.is_empty() && cache.signatures.is_empty());
  }

  #[tokio::test]
  async fn metadata_is_bounded_and_job_state_changes_invalidate_cached_attributes() {
    let mut cache = Metadata::default();
    for i in 0..200 {
      cache
        .fetch(
          &i.to_string(),
          "details",
          Duration::from_secs(30),
          || async { Ok("value".into()) },
        )
        .await
        .unwrap();
    }
    assert_eq!(cache.entries.len(), CAPACITY);
    assert!(cache.signatures.len() <= CAPACITY);
    cache.clear();
    cache
      .fetch("1", "details", Duration::from_secs(30), || async {
        Ok("value".into())
      })
      .await
      .unwrap();
    assert_eq!(cache.observe(&[job("RUNNING")]), ["1"]);
    cache
      .fetch("1", "jobspec", Duration::from_secs(60), || async {
        Ok("spec".into())
      })
      .await
      .unwrap();
    assert!(cache.observe(&[job("RUNNING")]).is_empty());
    assert_eq!(cache.observe(&[job("COMPLETED")]), ["1"]);
    assert!(cache.entries.is_empty());
    assert_eq!(cache.observe(&[]), ["1"]);
    assert!(cache.signatures.is_empty());
  }

  #[tokio::test]
  async fn stable_attributes_survive_expiry_of_dynamic_details_and_clock_keeps_sample_age() {
    let mut cache = Metadata::default();
    cache
      .fetch("1", "jobspec", Duration::from_secs(60), || async {
        Ok("spec".into())
      })
      .await
      .unwrap();
    cache
      .fetch("1", "details", Duration::from_secs(30), || async {
        Ok("details".into())
      })
      .await
      .unwrap();
    let details = cache
      .entries
      .iter_mut()
      .find(|entry| entry.kind == "details")
      .unwrap();
    details.sampled = Instant::now() - Duration::from_secs(10);
    details.expires = Instant::now() - Duration::from_secs(1);
    cache
      .fetch("1", "details", Duration::from_secs(30), || async {
        Ok("new details".into())
      })
      .await
      .unwrap();
    assert_eq!(
      cache
        .entries
        .iter()
        .find(|entry| entry.kind == "jobspec")
        .unwrap()
        .value
        .as_ref()
        .unwrap(),
      "spec"
    );
    cache
      .entries
      .iter_mut()
      .find(|entry| entry.kind == "details")
      .unwrap()
      .sampled = Instant::now() - Duration::from_secs(10);
    let mut resources = Resources {
      elapsed: Some("10s".into()),
      ..Default::default()
    };
    cache.advance_elapsed("1", "RUNNING", &mut resources);
    assert_eq!(resources.elapsed.as_deref(), Some("00:20"));
  }

  #[tokio::test]
  async fn slurm_resources_and_stdout_share_one_client_query() {
    // Child-process PATH isolation avoids mutating the test runner's environment.
    const CHILD: &str = "QSCOPE_METADATA_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
      let backend = Backend {
        kind: Scheduler::Slurm,
        timeout: Duration::from_secs(2),
      };
      let mut source = crate::source::Source::Live(backend);
      let mut cache = Metadata::default();
      let mut reader = crate::output::Reader::default();
      for _ in 0..2 {
        let resources = source.resources("1", &mut cache).await.unwrap();
        assert_eq!(resources.requested.cpus.as_deref(), Some("8"));
        source
          .output("1", &mut reader, crate::output::Position::Tail, &mut cache)
          .await
          .unwrap();
      }
      let count_path = std::env::var("QSCOPE_COUNT").unwrap();
      assert_eq!(
        std::fs::read_to_string(&count_path)
          .unwrap()
          .lines()
          .count(),
        1
      );
      cache.clear();
      reader.clear_paths();
      source.resources("1", &mut cache).await.unwrap();
      source
        .output("1", &mut reader, crate::output::Position::Tail, &mut cache)
        .await
        .unwrap();
      assert_eq!(
        std::fs::read_to_string(count_path).unwrap().lines().count(),
        2
      );
      return;
    }
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("scontrol");
    std::fs::write(&script, "#!/bin/sh\necho called >> \"$QSCOPE_COUNT\"\nprintf 'JobId=1 JobState=RUNNING ReqTRES=cpu=8,node=1 RunTime=00:00:10\\n StdOut=%s\\n WorkDir=/\\n' \"$QSCOPE_LOG\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = directory.path().join("job.out");
    std::fs::write(&log, "hello\n").unwrap();
    let result = tokio::process::Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "metadata::tests::slurm_resources_and_stdout_share_one_client_query",
        "--nocapture",
      ])
      .env(CHILD, "1")
      .env("PATH", directory.path())
      .env("QSCOPE_LOG", log)
      .env("QSCOPE_COUNT", directory.path().join("count"))
      .output()
      .await
      .unwrap();
    assert!(
      result.status.success(),
      "{}",
      String::from_utf8_lossy(&result.stdout)
    );
  }

  #[tokio::test]
  async fn flux_reuses_jobspec_and_allocation_between_resource_queries() {
    const CHILD: &str = "QSCOPE_FLUX_CACHE_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
      let backend = Backend {
        kind: Scheduler::Flux,
        timeout: Duration::from_secs(2),
      };
      let mut cache = Metadata::default();
      for _ in 0..2 {
        let resources = cache.resources(&backend, "1").await.unwrap();
        assert_eq!(resources.allocated.cpus.as_deref(), Some("8"));
      }
      let count_path = std::env::var("QSCOPE_COUNT").unwrap();
      assert_eq!(
        std::fs::read_to_string(&count_path)
          .unwrap()
          .lines()
          .count(),
        3
      );
      cache
        .entries
        .iter_mut()
        .find(|entry| entry.kind == "details")
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
      cache.resources(&backend, "1").await.unwrap();
      assert_eq!(
        std::fs::read_to_string(&count_path)
          .unwrap()
          .lines()
          .count(),
        4
      );
      cache.clear();
      cache.resources(&backend, "1").await.unwrap();
      assert_eq!(
        std::fs::read_to_string(count_path).unwrap().lines().count(),
        7
      );
      return;
    }
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("flux");
    std::fs::write(&script, r#"#!/bin/sh
echo "$*" >> "$QSCOPE_COUNT"
if [ "$1" = jobs ]; then
  echo '{"id":1,"state":"RUN","runtime":10}'
elif [ "$4" = jobspec ]; then
  echo '{"resources":[{"type":"slot","label":"worker","count":1,"with":[{"type":"core","count":8}]}],"tasks":[{"slot":"worker","count":{"per_slot":1}}]}'
elif [ "$4" = R ]; then
  echo '{"execution":{"R_lite":[{"rank":"0","children":{"core":"0-7"}}]}}'
else
  exit 1
fi
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = tokio::process::Command::new(std::env::current_exe().unwrap())
      .args([
        "--exact",
        "metadata::tests::flux_reuses_jobspec_and_allocation_between_resource_queries",
        "--nocapture",
      ])
      .env(CHILD, "1")
      .env("PATH", directory.path())
      .env("QSCOPE_COUNT", directory.path().join("count"))
      .output()
      .await
      .unwrap();
    assert!(
      result.status.success(),
      "{}",
      String::from_utf8_lossy(&result.stdout)
    );
  }
}

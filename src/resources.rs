//! Requested and allocated resources, deliberately distinct from measured usage.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Default)]
pub struct Hardware {
  pub cpu_model: Option<String>,
  pub gpu_model: Option<String>,
  pub architecture: Option<String>,
  /// Site-defined labels are not necessarily hardware model names.
  pub features: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Amounts {
  pub nodes: Option<String>,
  pub cpus: Option<String>,
  pub tasks: Option<String>,
  pub gpus: Option<String>,
  pub memory: Option<String>,
}

#[derive(Debug, Default)]
pub struct Resources {
  pub requested: Amounts,
  pub allocated: Amounts,
  pub elapsed: Option<String>,
  pub limit: Option<String>,
  pub hosts: Option<String>,
  pub hardware: Hardware,
}

fn token<'a>(text: &'a str, key: &str) -> Option<&'a str> {
  text
    .split_whitespace()
    .find_map(|s| s.strip_prefix(key))
    .filter(|s| !s.is_empty() && !matches!(*s, "(null)" | "N/A" | "Unknown"))
}

fn tres(text: &str) -> Amounts {
  let fields: Vec<_> = text.split(',').filter_map(|s| s.split_once('=')).collect();
  let get = |name| {
    fields
      .iter()
      .find(|(k, _)| *k == name)
      .map(|(_, v)| v.to_string())
  };
  let gpus = get("gres/gpu").or_else(|| {
    let typed: Vec<_> = fields
      .iter()
      .filter(|(k, _)| k.starts_with("gres/gpu:"))
      .collect();
    if typed.is_empty() {
      return None;
    }
    typed
      .iter()
      .map(|(_, v)| v.parse::<u64>().ok())
      .collect::<Option<Vec<_>>>()
      .and_then(|counts| counts.into_iter().try_fold(0u64, u64::checked_add))
      .map(|n| n.to_string())
  });
  Amounts {
    nodes: get("node"),
    cpus: get("cpu"),
    tasks: get("task"),
    gpus,
    memory: get("mem").map(|s| format!("{s} total")),
  }
}

pub fn slurm(text: &str) -> Result<Resources> {
  ensure!(
    token(text, "JobId=").is_some(),
    "Slurm resource response has no JobId"
  );
  let mut requested = tres(token(text, "ReqTRES=").unwrap_or(""));
  let allocated = tres(token(text, "AllocTRES=").unwrap_or(""));
  // NumTasks describes the submitted layout, not currently live
  // processes. Do not infer allocated tasks by dividing allocated CPU counts.
  requested.tasks = requested
    .tasks
    .or_else(|| positive_token(text, "NumTasks="));
  // NumCPUs/NumNodes may describe an allocation once running. Only use them as
  // requested values while pending, never manufacture a request from allocation.
  if token(text, "JobState=") == Some("PENDING") {
    requested.nodes = requested
      .nodes
      .or_else(|| token(text, "NumNodes=").map(str::to_owned));
    requested.cpus = requested
      .cpus
      .or_else(|| token(text, "NumCPUs=").map(str::to_owned));
  }
  if requested.memory.is_none() {
    requested.memory = token(text, "MinMemoryCPU=")
      .map(|s| format!("{s}/CPU"))
      .or_else(|| token(text, "MinMemoryNode=").map(|s| format!("{s}/node")));
  }
  Ok(Resources {
    requested,
    allocated,
    elapsed: token(text, "RunTime=").map(str::to_owned),
    limit: token(text, "TimeLimit=").map(str::to_owned),
    hosts: token(text, "NodeList=").map(str::to_owned),
    hardware: Hardware {
      // Only allocated typed TRES establishes the job's GPU model. Requests
      // and a node's complete GPU inventory do not identify assigned devices.
      gpu_model: token(text, "AllocTRES=").and_then(gpu_models),
      ..Default::default()
    },
  })
}

fn joined(values: BTreeSet<String>) -> Option<String> {
  (!values.is_empty()).then(|| values.into_iter().collect::<Vec<_>>().join(", "))
}

fn gpu_models(tres: &str) -> Option<String> {
  joined(
    tres
      .split(',')
      .filter_map(|entry| {
        let (kind, count) = entry.split_once('=')?;
        let model = kind.strip_prefix("gres/gpu:")?;
        (!model.is_empty() && count.parse::<u64>().ok()? > 0).then(|| model.to_string())
      })
      .collect(),
  )
}

/// Aggregate reported node metadata without guessing models from architecture
/// or arbitrary site features. Missing fields on any node remain explicit.
pub fn slurm_hardware(text: &str) -> Hardware {
  let nodes: Vec<_> = text.split("NodeName=").skip(1).collect();
  let collect = |key| {
    joined(
      nodes
        .iter()
        .map(|node| token(node, key).unwrap_or("Unknown").to_string())
        .collect(),
    )
  };
  let features = joined(
    nodes
      .iter()
      .flat_map(|node| {
        token(node, "ActiveFeatures=")
          .or_else(|| token(node, "AvailableFeatures="))
          .into_iter()
          .flat_map(|value| value.split(','))
          .map(str::to_owned)
      })
      .collect(),
  );
  Hardware {
    cpu_model: collect("CPUModel="),
    architecture: collect("Arch="),
    features,
    ..Default::default()
  }
}

fn positive_token(text: &str, name: &str) -> Option<String> {
  token(text, name)?
    .parse::<u64>()
    .ok()
    .filter(|n| *n > 0)
    .map(|n| n.to_string())
}

fn count_idset(text: &str) -> Option<u64> {
  text
    .trim_matches(['[', ']'])
    .split(',')
    .try_fold(0u64, |sum, part| {
      let count = if let Some((start, end)) = part.split_once('-') {
        end
          .parse::<u64>()
          .ok()?
          .checked_sub(start.parse().ok()?)?
          .checked_add(1)?
      } else {
        part.parse::<u64>().ok()?;
        1
      };
      sum.checked_add(count)
    })
}

// Jobspec resource counts are hierarchical: e.g. 2 nodes * 4 slots * 8 cores.
// Unsupported count constraints are left unknown rather than guessed.
fn requested_count(items: &[Value], kind: &str, multiplier: u64) -> Option<u64> {
  let mut total = 0u64;
  for item in items {
    let count = item["count"].as_u64()?.checked_mul(multiplier)?;
    if item["type"] == kind {
      total = total.checked_add(count)?;
    } else if let Some(children) = item["with"].as_array() {
      total = total.checked_add(requested_count(children, kind, count)?)?;
    }
  }
  Some(total)
}

/// Count only the slots named by a task group; slot count is not task count.
fn labeled_slots(items: &[Value], label: &str, multiplier: u64) -> Option<u64> {
  let mut total = 0u64;
  for item in items {
    let count = item["count"].as_u64()?.checked_mul(multiplier)?;
    if item["type"] == "slot" {
      if item["label"] == label {
        total = total.checked_add(count)?;
      }
    } else if let Some(children) = item["with"].as_array() {
      total = total.checked_add(labeled_slots(children, label, count)?)?;
    }
  }
  Some(total)
}

fn task_count(tasks: &[Value], items: &[Value]) -> Option<u64> {
  if tasks.is_empty() {
    return None;
  }
  tasks.iter().try_fold(0u64, |total, task| {
    let count = &task["count"];
    let n = match (count.get("total"), count.get("per_slot")) {
      (Some(n), None) => n.as_u64()?,
      (None, Some(n)) => {
        let slots = labeled_slots(items, task["slot"].as_str()?, 1)?;
        if slots == 0 {
          return None;
        }
        n.as_u64()?.checked_mul(slots)?
      }
      _ => return None,
    };
    if n == 0 {
      return None;
    }
    total.checked_add(n)
  })
}

pub fn flux(job: &str, jobspec: &str, allocation: Option<&str>) -> Result<Resources> {
  let job: Value = serde_json::from_str(job).context("Invalid Flux job JSON")?;
  let jobspec: Value = serde_json::from_str(jobspec).context("Invalid Flux jobspec")?;
  let items = jobspec["resources"]
    .as_array()
    .context("Flux jobspec has no resources")?;
  let nodes = requested_count(items, "node", 1).filter(|n| *n > 0);
  let tasks = jobspec["tasks"].as_array();
  let ntasks = tasks.and_then(|tasks| task_count(tasks, items));
  let requested = Amounts {
    nodes: nodes.map(|n| n.to_string()),
    cpus: requested_count(items, "core", 1).map(|n| n.to_string()),
    tasks: ntasks.map(|n| n.to_string()),
    gpus: requested_count(items, "gpu", 1).map(|n| n.to_string()),
    memory: None,
  };
  let mut allocated = Amounts::default();
  let mut hardware = Hardware::default();
  if let Some(text) = allocation {
    let r: Value = serde_json::from_str(text).context("Invalid Flux allocation JSON")?;
    let ranks = r
      .pointer("/execution/R_lite")
      .and_then(Value::as_array)
      .context("Flux allocation has no R_lite")?;
    let mut counts = [0u64; 3];
    for rank in ranks {
      let nodes = count_idset(rank["rank"].as_str().context("Invalid Flux rank set")?)
        .context("Invalid Flux rank set")?;
      counts[0] = counts[0]
        .checked_add(nodes)
        .context("Node count overflow")?;
      for (i, kind) in [(1, "core"), (2, "gpu")] {
        let per_node = match rank["children"].get(kind) {
          Some(v) => count_idset(v.as_str().context("Invalid Flux child set")?)
            .context("Invalid Flux child set")?,
          None => 0,
        };
        counts[i] = counts[i]
          .checked_add(
            per_node
              .checked_mul(nodes)
              .context("Resource count overflow")?,
          )
          .context("Resource count overflow")?;
      }
    }
    allocated.nodes = Some(counts[0].to_string());
    allocated.cpus = Some(counts[1].to_string());
    allocated.gpus = Some(counts[2].to_string());
    // RFC 20 properties are site-defined node labels, not confirmed models.
    // Use only the concrete allocation, never requested jobspec constraints.
    hardware.features = r
      .pointer("/execution/properties")
      .and_then(Value::as_object)
      .and_then(|properties| {
        joined(
          properties
            .iter()
            .filter(|(_, ranks)| {
              ranks
                .as_str()
                .and_then(count_idset)
                .is_some_and(|count| count > 0)
            })
            .map(|(name, _)| name.clone())
            .collect(),
        )
      });
  }
  let duration = jobspec
    .pointer("/attributes/system/duration")
    .and_then(Value::as_f64);
  Ok(Resources {
    requested,
    allocated,
    elapsed: job["runtime"].as_f64().map(|s| format!("{s:.0}s")),
    limit: duration.map(|s| {
      if s == 0.0 {
        "Unlimited".into()
      } else {
        format!("{s:.0}s")
      }
    }),
    hosts: job["nodelist"].as_str().map(str::to_owned),
    hardware,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hardware_models_do_not_come_from_requests_or_generic_features() {
    let pending = slurm("JobId=1 JobState=PENDING ReqTRES=gres/gpu:h100=2").unwrap();
    assert!(pending.hardware.gpu_model.is_none());
    let running =
      slurm("JobId=1 AllocTRES=gres/gpu:h100=2,gres/gpu:a100=1,gres/gpu:l40=0").unwrap();
    assert_eq!(running.hardware.gpu_model.as_deref(), Some("a100, h100"));
    let hardware = slurm_hardware(
      "NodeName=n1 Arch=x86_64 ActiveFeatures=zen4,gpu CPUModel=EPYC_9654\nNodeName=n2 Arch=aarch64 ActiveFeatures=arm,gpu\n",
    );
    assert_eq!(hardware.cpu_model.as_deref(), Some("EPYC_9654, Unknown"));
    assert_eq!(hardware.architecture.as_deref(), Some("aarch64, x86_64"));
    assert_eq!(hardware.features.as_deref(), Some("arm, gpu, zen4"));
    assert!(hardware.gpu_model.is_none());
    assert!(slurm_hardware("permission denied").cpu_model.is_none());
  }

  #[test]
  fn flux_reports_allocated_properties_without_claiming_they_are_models() {
    let spec = r#"{"resources":[{"type":"core","count":2}],"attributes":{"system":{"constraints":{"properties":["requested-model"]}}}}"#;
    let allocation = r#"{"execution":{"R_lite":[{"rank":"0","children":{"core":"0-1"}}],"properties":{"zen4":"0","batch":"0","empty":"","invalid":"x"}}}"#;
    let r = flux("{}", spec, Some(allocation)).unwrap();
    assert_eq!(r.hardware.features.as_deref(), Some("batch, zen4"));
    assert!(r.hardware.cpu_model.is_none() && r.hardware.gpu_model.is_none());
    assert!(flux("{}", spec, None).unwrap().hardware.features.is_none());
  }
  #[test]
  fn slurm_tasks_are_declared_not_inferred_from_allocated_cpus() {
    let r = slurm("JobId=1 JobState=RUNNING NumTasks=32 CPUs/Task=4 ReqTRES=cpu=128,node=4 AllocTRES=cpu=128,node=4").unwrap();
    assert_eq!(r.requested.tasks.as_deref(), Some("32"));
    assert!(r.allocated.tasks.is_none());
    let r = slurm("JobId=2 NumTasks=0 CPUs/Task=N/A").unwrap();
    assert!(r.requested.tasks.is_none());
    let r = slurm("JobId=3 NumTasks=9 ReqTRES=task=8 AllocTRES=task=6").unwrap();
    assert_eq!(r.requested.tasks.as_deref(), Some("8"));
    assert_eq!(r.allocated.tasks.as_deref(), Some("6"));
  }

  #[test]
  fn flux_tasks_support_total_and_per_slot_counts() {
    let mut spec = serde_json::json!({
      "resources": [{"type": "node", "count": 2, "with": [
        {"type": "slot", "label": "workers", "count": 4, "with": [
          {"type": "core", "count": 8}
        ]}
      ]}],
      "tasks": [{"slot": "workers", "count": {"per_slot": 2}}]
    });
    for count in [
      serde_json::json!({"per_slot":2}),
      serde_json::json!({"total":16}),
    ] {
      spec["tasks"][0]["count"] = count;
      let r = flux("{}", &spec.to_string(), None).unwrap();
      assert_eq!(r.requested.tasks.as_deref(), Some("16"));
      assert!(r.allocated.tasks.is_none());
    }
    spec["tasks"][0]["count"] = serde_json::json!({"total":7});
    let r = flux("{}", &spec.to_string(), None).unwrap();
    assert_eq!(r.requested.tasks.as_deref(), Some("7"));
    spec["tasks"][0]["count"] = serde_json::json!({"per_slot":1});
    spec["tasks"][0]["slot"] = "missing".into();
    assert!(
      flux("{}", &spec.to_string(), None)
        .unwrap()
        .requested
        .tasks
        .is_none()
    );
  }

  #[test]
  fn flux_multiple_groups_sum_task_counts() {
    let spec = serde_json::json!({
      "resources": [{"type": "slot", "label": "worker", "count": 4,
        "with": [{"type":"core", "count":8}]}],
      "tasks": [{"slot":"worker", "count":{"total":2}},
        {"slot":"worker", "count":{"per_slot":1}}]
    });
    let r = flux("{}", &spec.to_string(), None).unwrap();
    assert_eq!(r.requested.tasks.as_deref(), Some("6"));
  }
  #[test]
  fn slurm_keeps_requests_and_allocations_distinct() {
    let r = slurm("JobId=1 JobState=RUNNING ReqTRES=cpu=8,node=1,mem=16G,gres/gpu=2,gres/gpu:a100=2 AllocTRES=cpu=16,node=2,mem=32G,gres/gpu=2 RunTime=00:10:00 TimeLimit=01:00:00 NodeList=n[1-2]").unwrap();
    assert_eq!(r.requested.cpus.as_deref(), Some("8"));
    assert_eq!(r.allocated.cpus.as_deref(), Some("16"));
    assert_eq!(r.requested.gpus.as_deref(), Some("2"));
    assert_eq!(r.requested.memory.as_deref(), Some("16G total"));
  }
  #[test]
  fn slurm_pending_missing_and_per_cpu_memory() {
    let r = slurm("JobId=1 JobState=PENDING NumCPUs=8 NumNodes=1 MinMemoryCPU=2G").unwrap();
    assert_eq!(r.requested.memory.as_deref(), Some("2G/CPU"));
    assert!(r.allocated.cpus.is_none());
    assert!(slurm("unavailable").is_err());
  }
  #[test]
  fn flux_hierarchical_requests_and_rank_sets() {
    let spec = r#"{"resources":[{"type":"node","count":2,"with":[{"type":"slot","count":4,"with":[{"type":"core","count":8},{"type":"gpu","count":1}]}]}],"attributes":{"system":{"duration":3600}}}"#;
    let allocation =
      r#"{"execution":{"R_lite":[{"rank":"0-1","children":{"core":"0-31","gpu":"0-3"}}]}}"#;
    let r = flux(
      r#"{"runtime":10,"nodelist":"n[0-1]"}"#,
      spec,
      Some(allocation),
    )
    .unwrap();
    assert_eq!(r.requested.cpus.as_deref(), Some("64"));
    assert_eq!(r.allocated.cpus.as_deref(), Some("64"));
    assert_eq!(r.allocated.gpus.as_deref(), Some("8"));
    assert!(r.requested.memory.is_none());
    assert!(flux("{}", spec, None).unwrap().allocated.nodes.is_none());
    assert!(flux("{}", spec, Some("{}")).is_err());
  }
}

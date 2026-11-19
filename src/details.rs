//! A presentation-only inspector. Raw scheduler output stays in the backend for
//! path/resource parsing; users see labeled sections rather than CLI/JSON dumps.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

type Fields = BTreeMap<String, String>;
const LABEL_WIDTH: usize = 16;

fn available(text: &str) -> String {
  if text.is_empty() || matches!(text, "(null)" | "N/A" | "Unknown") {
    "—".into()
  } else {
    text.into()
  }
}

struct Inspector {
  fields: Fields,
  text: String,
}

impl Inspector {
  fn new(fields: Fields) -> Self {
    Self {
      fields,
      text: String::new(),
    }
  }

  fn row(&mut self, label: &str, value: &str) {
    let value = available(value);
    for (i, line) in value.lines().enumerate() {
      self.text.push_str(&format!(
        "  {:LABEL_WIDTH$}  {}\n",
        if i == 0 { label } else { "" },
        line
      ));
    }
  }

  fn section(&mut self, title: &str, rows: &[(&str, &str)]) {
    if !self.text.is_empty() {
      self.text.push('\n');
    }
    self.text.push_str(title);
    self.text.push('\n');
    for &(label, key) in rows {
      let value = self.fields.remove(key).unwrap_or_default();
      self.row(label, &value);
    }
  }

  fn optional_section(&mut self, title: &str, rows: &[(&str, &str)]) {
    if rows.iter().any(|(_, key)| self.fields.contains_key(*key)) {
      self.section(title, rows);
    }
  }

  fn cpu_allocation(&mut self, allocations: &[(String, String)], pending: bool) {
    if allocations.is_empty() {
      self.row(
        "CPU/core IDs",
        if pending {
          "Not allocated"
        } else {
          "Unavailable"
        },
      );
    } else {
      let lines = allocations
        .iter()
        .map(|(node, ids)| format!("{node}: {ids}"))
        .collect::<Vec<_>>()
        .join("\n");
      self.row("CPU/core IDs", &lines);
    }
  }

  fn finish(mut self) -> String {
    if !self.fields.is_empty() {
      self.text.push_str("\nAdditional information\n");
      for (key, value) in std::mem::take(&mut self.fields) {
        // Keep unknown fields accessible rather than silently discarding newer
        // scheduler attributes. They still use the formatted label/value layout.
        self.row(&key.replace('_', " "), &value);
      }
    }
    self.text
  }
}

/// Recognize `Key=value` boundaries without splitting job names at spaces.
/// Command and path fields occupy their own scontrol lines and may contain '='.
fn slurm_fields(text: &str) -> Fields {
  let mut fields = Fields::new();
  for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
    if let Some((key, value)) = line.split_once('=')
      && matches!(
        key,
        "Command"
          | "WorkDir"
          | "StdOut"
          | "StdErr"
          | "StdIn"
          | "Comment"
          | "AdminComment"
          | "SystemComment"
      )
    {
      fields.insert(key.into(), value.into());
      continue;
    }
    let mut boundaries = Vec::new();
    let mut offset = 0;
    for token in line.split_whitespace() {
      let start = offset + line[offset..].find(token).unwrap_or(0);
      if let Some((key, _)) = token.split_once('=')
        && key.starts_with(|c: char| c.is_ascii_alphabetic())
        && key
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/'))
      {
        boundaries.push((start, key));
      }
      offset = start + token.len();
    }
    for (i, &(start, key)) in boundaries.iter().enumerate() {
      let end = boundaries
        .get(i + 1)
        .map_or(line.len(), |(start, _)| *start);
      fields.insert(key.into(), line[start + key.len() + 1..end].trim().into());
    }
  }
  fields
}

fn slurm_cpu_allocations(text: &str) -> Vec<(String, String)> {
  let mut allocations = Vec::new();
  let mut node = None;
  for line in text.lines().map(str::trim) {
    // Do not interpret CPU_IDs embedded in commands, comments, or other values.
    if !line.starts_with("Nodes=") && !line.starts_with("CPU_IDs=") {
      continue;
    }
    let mut fields = slurm_fields(line);
    if let Some(nodes) = fields.remove("Nodes") {
      node = Some(nodes);
    }
    if let Some(ids) = fields.remove("CPU_IDs")
      && !ids.is_empty()
      && !matches!(ids.as_str(), "(null)" | "N/A" | "Unknown")
      && let Some(node) = &node
    {
      allocations.push((node.clone(), ids));
    }
  }
  allocations
}

pub fn slurm(text: &str) -> Result<String> {
  let mut fields = slurm_fields(text);
  let allocations = slurm_cpu_allocations(text);
  let pending = fields
    .get("JobState")
    .is_some_and(|state| matches!(state.as_str(), "PENDING" | "CONFIGURING"));
  // Repeated allocation records must not collapse into unrelated last-node IDs.
  fields.remove("Nodes");
  fields.remove("CPU_IDs");
  ensure!(
    fields.contains_key("JobId"),
    "Slurm details did not include JobId"
  );
  if let Some(value) = fields.get_mut("RunTime") {
    *value = crate::duration::elapsed(value, "");
  }
  let mut view = Inspector::new(fields);
  view.section(
    "Overview",
    &[
      ("Name", "JobName"),
      ("ID", "JobId"),
      ("State", "JobState"),
      ("Owner", "UserId"),
      ("Queue", "Partition"),
      ("Priority", "Priority"),
    ],
  );
  view.section(
    "Timing",
    &[
      ("Submitted", "SubmitTime"),
      ("Eligible", "EligibleTime"),
      ("Started", "StartTime"),
      ("Ends / ended", "EndTime"),
      ("Elapsed", "RunTime"),
      ("Time limit", "TimeLimit"),
    ],
  );
  view.section(
    "Execution",
    &[
      ("Command", "Command"),
      ("Directory", "WorkDir"),
      ("Nodes", "NodeList"),
      ("Batch host", "BatchHost"),
      ("Exit code", "ExitCode"),
      ("Restarts", "Restarts"),
    ],
  );
  view.section(
    "Files",
    &[
      ("Stdout", "StdOut"),
      ("Stderr", "StdErr"),
      ("Stdin", "StdIn"),
    ],
  );
  view.optional_section(
    "Scheduling",
    &[
      ("Reason", "Reason"),
      ("Dependencies", "Dependency"),
      ("Account", "Account"),
      ("QoS", "QOS"),
      ("Constraints", "Features"),
      ("Reservation", "Reservation"),
      ("Nice", "Nice"),
    ],
  );
  view.section(
    "Resources",
    &[
      ("Requested", "ReqTRES"),
      ("Allocated", "AllocTRES"),
      ("Node count", "NumNodes"),
      ("CPUs", "NumCPUs"),
      ("Tasks", "NumTasks"),
      ("CPUs / task", "CPUs/Task"),
    ],
  );
  view.cpu_allocation(&allocations, pending);
  Ok(view.finish())
}

fn value_text(value: &Value) -> String {
  match value {
    Value::Null => "—".into(),
    Value::String(text) => available(text),
    Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(", "),
    Value::Object(items) => items
      .iter()
      .map(|(key, value)| format!("{key}: {}", value_text(value)))
      .collect::<Vec<_>>()
      .join("\n"),
    _ => value.to_string(),
  }
}

fn timestamp(value: &Value) -> String {
  let Some(seconds) = value.as_f64().filter(|n| n.is_finite() && *n > 0.0) else {
    return "—".into();
  };
  chrono::DateTime::from_timestamp(seconds as i64, 0)
    .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
    .unwrap_or_else(|| "—".into())
}

pub fn flux(text: &str, allocation: Option<&str>) -> Result<String> {
  let job: Value = serde_json::from_str(text).context("Invalid Flux details JSON")?;
  let object = job.as_object().context("Flux details must be an object")?;
  ensure!(
    object.contains_key("id"),
    "Flux details did not include a job ID"
  );
  let mut fields: Fields = object
    .iter()
    .map(|(key, value)| {
      let text = if matches!(
        key.as_str(),
        "t_submit"
          | "t_depend"
          | "t_priority"
          | "t_sched"
          | "t_run"
          | "t_cleanup"
          | "t_inactive"
          | "expiration"
      ) {
        timestamp(value)
      } else if key == "runtime" || key == "duration" {
        value
          .as_f64()
          .filter(|n| n.is_finite() && *n >= 0.0)
          .map(|seconds| {
            if key == "duration" && seconds == 0.0 {
              "Unlimited".into()
            } else {
              crate::duration::elapsed(&format!("{seconds}s"), "")
            }
          })
          .unwrap_or_else(|| "—".into())
      } else {
        value_text(value)
      };
      (key.clone(), text)
    })
    .collect();
  if !fields.contains_key("username")
    && let Some(uid) = fields.get("userid").cloned()
  {
    fields.insert("username".into(), uid);
  }
  if !fields.contains_key("runtime")
    && let Some(runtime) = fields.remove("runtime_display")
  {
    fields.insert("runtime".into(), crate::duration::elapsed(&runtime, ""));
  }
  let mut view = Inspector::new(fields);
  view.section(
    "Overview",
    &[
      ("Name", "name"),
      ("ID", "id"),
      ("State", "state"),
      ("Status", "status"),
      ("Owner", "username"),
      ("Queue", "queue"),
      ("Priority", "priority"),
      ("Urgency", "urgency"),
    ],
  );
  view.section(
    "Timing",
    &[
      ("Submitted", "t_submit"),
      ("Started", "t_run"),
      ("Finished", "t_inactive"),
      ("Elapsed", "runtime"),
      ("Time limit", "duration"),
      ("Expires", "expiration"),
    ],
  );
  view.section(
    "Execution",
    &[
      ("Command", "command"),
      ("Directory", "cwd"),
      ("Nodes", "nodelist"),
      ("Ranks", "ranks"),
      ("Result", "result"),
      ("Return code", "returncode"),
    ],
  );
  let pending = matches!(
    job["state"].as_str(),
    Some("NEW" | "DEPEND" | "PRIORITY" | "SCHED")
  );
  let allocation = allocation.and_then(|text| serde_json::from_str::<Value>(text).ok());
  let allocations: Vec<_> = allocation
    .as_ref()
    .and_then(|value| value.pointer("/execution/R_lite"))
    .and_then(Value::as_array)
    .map(|ranks| {
      ranks
        .iter()
        .map(|rank| {
          (
            format!("Ranks {}", rank["rank"].as_str().unwrap_or("Unknown")),
            rank
              .pointer("/children/core")
              .and_then(Value::as_str)
              .filter(|ids| !ids.is_empty())
              .unwrap_or("Unavailable")
              .to_string(),
          )
        })
        .collect()
    })
    .unwrap_or_default();
  view.section(
    "Files",
    &[
      ("Stdout", "stdout"),
      ("Stderr", "stderr"),
      ("Stdin", "stdin"),
    ],
  );
  view.optional_section(
    "Scheduling",
    &[
      ("Dependencies", "dependencies"),
      ("Annotations", "annotations"),
      ("Context", "context"),
    ],
  );
  view.optional_section("Exception", &[("Exception", "exception")]);
  view.section(
    "Resources",
    &[
      ("Nodes", "nnodes"),
      ("Cores", "ncores"),
      ("Tasks", "ntasks"),
    ],
  );
  view.cpu_allocation(&allocations, pending);
  view.optional_section("Demo", &[("Simulated", "demo"), ("Note", "note")]);
  Ok(view.finish())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn slurm_cpu_ids_keep_each_node_group_including_wrapped_records() {
    let text = "JobId=1 JobState=RUNNING NumCPUs=96\n \
Nodes=n[01-02] CPU_IDs=0-31 Mem=4096\n \
Nodes=n03 CPU_IDs=32-47,64-79 Mem=4096\n \
Nodes=n04\n CPU_IDs=8-15\n Command=echo CPU_IDs=not-an-allocation";
    let formatted = slurm(text).unwrap();
    for value in [
      "CPU/core IDs",
      "n[01-02]: 0-31",
      "n03: 32-47,64-79",
      "n04: 8-15",
    ] {
      assert!(formatted.contains(value), "{value}");
    }
    assert_eq!(formatted.matches("CPU/core IDs").count(), 1);
    let resources = formatted.split_once("\nResources\n").unwrap().1;
    assert!(resources.contains("CPU/core IDs"));
    assert!(!formatted.contains("CPU/core allocation"));
    assert!(!formatted.contains("Scope"));
    assert!(!formatted.contains("Meaning"));
    assert!(!formatted.contains("CPU_IDs           "));
    assert!(
      slurm("JobId=1 JobState=PENDING NumCPUs=96")
        .unwrap()
        .contains("Not allocated")
    );
    let missing = slurm("JobId=1 JobState=RUNNING NumCPUs=96").unwrap();
    assert!(missing.contains("Unavailable"));
    assert!(!missing.contains("0-95")); // Never infer CPU IDs from a count.
  }

  #[test]
  fn flux_cpu_ids_are_grouped_by_rank_and_missing_allocations_do_not_break_details() {
    let raw = r#"{"id":1,"state":"RUN","ncores":8}"#;
    let allocation = r#"{"execution":{"R_lite":[{"rank":"0-1","children":{"core":"0-3"}},{"rank":"2","children":{"core":"8,10-12"}}]}}"#;
    let formatted = flux(raw, Some(allocation)).unwrap();
    for value in ["Ranks 0-1: 0-3", "Ranks 2: 8,10-12"] {
      assert!(formatted.contains(value), "{value}");
    }
    assert!(
      formatted
        .split_once("\nResources\n")
        .unwrap()
        .1
        .contains("CPU/core IDs")
    );
    assert!(!formatted.contains("CPU/core allocation"));
    assert!(flux(raw, None).unwrap().contains("Unavailable"));
    assert!(flux(raw, Some("not JSON")).unwrap().contains("Unavailable"));
    assert!(
      flux(r#"{"id":1,"state":"SCHED"}"#, None)
        .unwrap()
        .contains("Not allocated")
    );
  }

  #[test]
  fn slurm_preserves_spaces_paths_and_unknown_fields() {
    let formatted = slurm("JobId=123_7 JobName=my simulation\n UserId=alice(1000) JobState=RUNNING Priority=42\n Command=/work/my script.sh --input=a=b FOO=bar\n WorkDir=/work/my directory\n StdOut=/logs/job output.out\n RunTime=120s ExtraField=retained").unwrap();
    assert!(formatted.starts_with("Overview\n"));
    for value in [
      "my simulation",
      "123_7",
      "/work/my directory",
      "/logs/job output.out",
      "/work/my script.sh --input=a=b FOO=bar",
      "02:00",
      "Additional information",
      "retained",
    ] {
      assert!(formatted.contains(value), "{value}");
    }
    assert!(!formatted.contains("JobId="));
    assert!(formatted.contains("Stderr            —"));
    assert!(slurm("error").is_err());
  }

  #[test]
  fn flux_formats_timestamps_failures_and_large_ids_without_json_dump() {
    let formatted = flux(r#"{"id":18446744073709551614,"name":"sim","state":"INACTIVE","result":"FAILED","t_submit":1,"runtime":750,"userid":1000,"exception":{"occurred":true,"type":"exec","note":"not found"},"extra":{"custom":"retained"}}"#, None).unwrap();
    for value in [
      "Overview",
      "Timing",
      "Files",
      "Exception",
      "FAILED",
      "1970-01-01 00:00:01 UTC",
      "12:30",
      "18446744073709551614",
      "not found",
      "retained",
    ] {
      assert!(formatted.contains(value), "{value}");
    }
    assert!(!formatted.contains("{\""));
    assert!(flux("{}", None).is_err());
  }
}

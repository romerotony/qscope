//! Bounded, read-only stdout previews. Never execute file contents or read a whole
//! potentially multi-gigabyte job log into memory.

use anyhow::{Context, Result, ensure};
use std::{
  fs::File,
  io::{Read, Seek, SeekFrom},
  path::{Path, PathBuf},
  time::Duration,
};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub struct Output {
  pub path: String,
  pub text: String,
}

#[derive(Default)]
pub struct Reader {
  // Cache only successful resolutions for the current job; retries can recover
  // when a pending job gains output metadata. Files are reopened on each read
  // to handle replacement, truncation, and creation after the job starts.
  cached: Option<(String, PathBuf)>,
}

impl Reader {
  pub fn path(&self, id: &str) -> Option<PathBuf> {
    self
      .cached
      .as_ref()
      .filter(|(job, _)| job == id)
      .map(|(_, path)| path.clone())
  }

  pub fn remember(&mut self, id: &str, path: PathBuf) {
    self.cached = Some((id.into(), path));
  }
}

pub async fn read(path: PathBuf) -> Result<Output> {
  let display = path.display().to_string();
  // Filesystem calls (including network-mounted paths) must not block drawing.
  // A timed-out OS read can remain in the blocking pool until the OS returns.
  let text = tokio::time::timeout(
    Duration::from_secs(3),
    tokio::task::spawn_blocking(move || tail(&path)),
  )
  .await
  .context("Output filesystem read timed out")??
  .with_context(|| {
    format!("Cannot read {display}; it may not exist yet or be accessible on this node")
  })?;
  Ok(Output {
    path: display,
    text,
  })
}

fn tail(path: &Path) -> Result<String> {
  ensure!(
    std::fs::metadata(path)?.is_file(),
    "Output path is not a regular file"
  );
  let mut file = File::open(path)?;
  let length = file.metadata()?.len();
  let start = length.saturating_sub(MAX_BYTES);
  file.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::new();
  file.take(MAX_BYTES).read_to_end(&mut bytes)?;
  // A tail may start in the middle of a line or UTF-8 character. Drop that
  // partial line, and replace malformed bytes instead of failing the preview.
  if start > 0
    && let Some(newline) = bytes.iter().position(|&b| b == b'\n')
  {
    bytes.drain(..=newline);
  }
  let text: String = String::from_utf8_lossy(&bytes)
    .chars()
    .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
    .collect();
  Ok(if text.is_empty() {
    "Output file is empty.".into()
  } else {
    text
  })
}

/// scontrol's normal multi-line output places StdOut and WorkDir on their own
/// lines. Preserve spaces in these paths instead of splitting on whitespace.
pub fn slurm_path(details: &str) -> Result<PathBuf> {
  let line_field = |name: &str| {
    details
      .lines()
      .find_map(|line| line.trim().strip_prefix(name))
  };
  let token_field = |name: &str| {
    details
      .split_whitespace()
      .find_map(|token| token.strip_prefix(name))
  };
  let template = line_field("StdOut=").context("Slurm did not report a StdOut path")?;
  ensure!(
    !template.is_empty() && template != "(null)",
    "No stdout file is configured for this job"
  );
  let mut expanded = String::new();
  let mut chars = template.chars();
  while let Some(c) = chars.next() {
    if c != '%' {
      expanded.push(c);
      continue;
    }
    let value = match chars.next() {
      Some('%') => "%",
      Some('j') => token_field("JobId=").context("Missing JobId for %j")?,
      Some('A') => token_field("ArrayJobId=")
        .or_else(|| token_field("JobId="))
        .context("Missing job ID for %A")?,
      Some('a') => token_field("ArrayTaskId=").unwrap_or("4294967294"),
      Some(pattern) => anyhow::bail!(
        "Unresolved Slurm stdout pattern %{pattern}; cannot determine a single output file"
      ),
      None => anyhow::bail!("Incomplete stdout filename pattern"),
    };
    expanded.push_str(value);
  }
  let path = PathBuf::from(expanded);
  if path.is_absolute() {
    return Ok(path);
  }
  let workdir =
    PathBuf::from(line_field("WorkDir=").context("Relative stdout path without WorkDir")?);
  ensure!(workdir.is_absolute(), "Slurm WorkDir is not absolute");
  Ok(workdir.join(path))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolves_paths_with_spaces_and_array_patterns() {
    let details = "JobId=123 JobName=sim\n ArrayJobId=100 ArrayTaskId=7\n WorkDir=/shared/my work\n StdOut=logs/slurm-%A_%a-%j-%%.out\n StdErr=/other";
    assert_eq!(
      slurm_path(details).unwrap(),
      PathBuf::from("/shared/my work/logs/slurm-100_7-123-%.out")
    );
    assert_eq!(
      slurm_path("StdOut=/shared/path with spaces.out").unwrap(),
      PathBuf::from("/shared/path with spaces.out")
    );
    assert!(slurm_path("StdOut=slurm-%N.out\nWorkDir=/work").is_err());
    assert!(slurm_path("StdOut=(null)").is_err());
    assert!(slurm_path("JobId=1").is_err());
  }

  #[tokio::test]
  async fn reads_bounded_tail_and_handles_empty_missing_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("job.out");
    std::fs::write(&path, "").unwrap();
    assert!(read(path.clone()).await.unwrap().text.contains("empty"));
    std::fs::write(&path, format!("{}\nlast line\n", "x".repeat(100_000))).unwrap();
    assert_eq!(read(path.clone()).await.unwrap().text, "last line\n");
    std::fs::write(&path, b"new\n\xff\x1btext\n").unwrap();
    let output = read(path).await.unwrap();
    assert!(output.text.contains("new"));
    assert!(!output.text.contains('\x1b'));
    assert!(read(directory.path().join("missing")).await.is_err());
    assert!(read(directory.path().to_path_buf()).await.is_err());
  }

  #[test]
  fn cache_is_specific_to_job() {
    let mut reader = Reader::default();
    reader.remember("1", PathBuf::from("/one"));
    assert!(reader.path("1").is_some());
    assert!(reader.path("2").is_none());
  }
}

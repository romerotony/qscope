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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
  pub start: u64,
  pub end: u64,
  pub length: u64,
  device: u64,
  inode: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
  #[default]
  Tail,
  Start,
  Before(Page),
  After(Page),
}

#[derive(Debug)]
pub struct Output {
  pub path: String,
  pub text: String,
  pub page: Option<Page>,
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
  read_page(path, Position::Tail).await
}

pub async fn read_page(path: PathBuf, position: Position) -> Result<Output> {
  let display = path.display().to_string();
  // Filesystem calls (including network-mounted paths) must not block drawing.
  // A timed-out OS read can remain in the blocking pool until the OS returns.
  let (text, page) = tokio::time::timeout(
    Duration::from_secs(3),
    tokio::task::spawn_blocking(move || page(&path, position)),
  )
  .await
  .context("Output filesystem read timed out")??
  .with_context(|| {
    format!("Cannot read {display}; it may not exist yet or be accessible on this node")
  })?;
  Ok(Output {
    path: display,
    text,
    page: Some(page),
  })
}

fn page(path: &Path, position: Position) -> Result<(String, Page)> {
  use std::os::unix::fs::MetadataExt;
  ensure!(
    std::fs::metadata(path)?.is_file(),
    "Output path is not a regular file"
  );
  let mut file = File::open(path)?;
  let metadata = file.metadata()?;
  let length = metadata.len();
  if let Position::Before(previous) | Position::After(previous) = position {
    ensure!(
      metadata.dev() == previous.device
        && metadata.ino() == previous.inode
        && length >= previous.end,
      "Output file was replaced or truncated; press G to resume from the current file"
    );
  }
  let (mut start, mut end) = match position {
    Position::Tail => (length.saturating_sub(MAX_BYTES), length),
    Position::Start => (0, length.min(MAX_BYTES)),
    Position::Before(previous) => (previous.start.saturating_sub(MAX_BYTES), previous.start),
    Position::After(previous) => (
      previous.end,
      previous.end.saturating_add(MAX_BYTES).min(length),
    ),
  };
  file.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::new();
  file.take(end - start).read_to_end(&mut bytes)?;
  end = start + bytes.len() as u64;
  // Align pages with whole lines when possible. Keep giant line fragments if
  // removing them would empty a page, ensuring navigation always makes progress.
  if matches!(position, Position::Tail | Position::Before(_))
    && start > 0
    && let Some(newline) = bytes.iter().position(|&b| b == b'\n')
    && newline + 1 < bytes.len()
  {
    bytes.drain(..=newline);
    start += newline as u64 + 1;
  }
  if matches!(position, Position::Start | Position::After(_))
    && end < length
    && let Some(newline) = bytes.iter().rposition(|&b| b == b'\n')
  {
    bytes.truncate(newline + 1);
    end = start + bytes.len() as u64;
  }
  let text: String = String::from_utf8_lossy(&bytes)
    .chars()
    .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
    .collect();
  let page = Page {
    start,
    end,
    length,
    device: metadata.dev(),
    inode: metadata.ino(),
  };
  Ok((
    if text.is_empty() {
      "Output file is empty.".into()
    } else {
      text
    },
    page,
  ))
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

  #[tokio::test]
  async fn pages_cover_entire_large_file_in_both_directions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.out");
    let expected: String = (0..25_000)
      .map(|i| format!("line {i:06}: output\n"))
      .collect();
    std::fs::write(&path, &expected).unwrap();
    let mut output = read_page(path.clone(), Position::Start).await.unwrap();
    let mut forward = String::new();
    loop {
      let info = output.page.unwrap();
      assert!(info.end - info.start <= MAX_BYTES);
      forward.push_str(&output.text);
      if info.end == info.length {
        break;
      }
      output = read_page(path.clone(), Position::After(info))
        .await
        .unwrap();
      assert_eq!(output.page.unwrap().start, info.end);
      assert!(output.page.unwrap().end > info.end);
    }
    assert_eq!(forward, expected);
    let mut output = read(path.clone()).await.unwrap();
    let mut backward = vec![];
    loop {
      let info = output.page.unwrap();
      assert!(info.end - info.start <= MAX_BYTES);
      backward.push(output.text);
      if info.start == 0 {
        break;
      }
      output = read_page(path.clone(), Position::Before(info))
        .await
        .unwrap();
      assert_eq!(output.page.unwrap().end, info.start);
      assert!(output.page.unwrap().start < info.start);
    }
    backward.reverse();
    assert_eq!(backward.concat(), expected);
  }

  #[tokio::test]
  async fn giant_lines_make_progress_and_changed_files_are_detected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.out");
    let expected = format!("{}\nfinal line\n", "x".repeat(MAX_BYTES as usize * 3));
    std::fs::write(&path, &expected).unwrap();
    let mut output = read(path.clone()).await.unwrap();
    let original = output.page.unwrap();
    let mut pieces = vec![];
    loop {
      let info = output.page.unwrap();
      pieces.push(output.text);
      if info.start == 0 {
        break;
      }
      output = read_page(path.clone(), Position::Before(info))
        .await
        .unwrap();
      assert!(output.page.unwrap().start < info.start);
    }
    pieces.reverse();
    assert_eq!(pieces.concat(), expected);
    std::fs::write(&path, "truncated\n").unwrap();
    assert!(
      read_page(path.clone(), Position::Before(original))
        .await
        .is_err()
    );
    std::fs::rename(&path, directory.path().join("old.out")).unwrap();
    std::fs::write(&path, expected).unwrap();
    assert!(
      read_page(path.clone(), Position::Before(original))
        .await
        .is_err()
    );
    assert!(read(path).await.is_ok());
  }

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

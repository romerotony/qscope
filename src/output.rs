//! Bounded, read-only stdout/stderr previews. Never execute file contents or read a whole
//! potentially multi-gigabyte job log into memory.

use anyhow::{Context, Result, ensure};
use std::{
  collections::VecDeque,
  fs::File,
  io::{Read, Seek, SeekFrom},
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stream {
  #[default]
  Stdout,
  Stderr,
}

impl Stream {
  pub fn name(self) -> &'static str {
    match self {
      Self::Stdout => "stdout",
      Self::Stderr => "stderr",
    }
  }

  pub fn title(self) -> &'static str {
    match self {
      Self::Stdout => "Stdout",
      Self::Stderr => "Stderr",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
  pub start: u64,
  pub end: u64,
  pub length: u64,
  device: u64,
  inode: u64,
  modified: (i64, i64),
  changed: (i64, i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
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

pub struct Reader {
  cached: VecDeque<CachedPath>,
  in_flight: Option<tokio::task::JoinHandle<Result<(String, Page)>>>,
  timeout: Duration,
  failures: u32,
  retry_at: Option<Instant>,
  last_path: Option<PathBuf>,
  // Retain only one decoded tail from a bounded raw page. Explicit pages never reuse it.
  tail: Option<(PathBuf, String, Page)>,
}

struct CachedPath {
  id: String,
  stream: Stream,
  path: PathBuf,
  sampled: Instant,
}

impl Default for Reader {
  fn default() -> Self {
    Self {
      cached: VecDeque::new(),
      in_flight: None,
      timeout: Duration::from_secs(3),
      failures: 0,
      retry_at: None,
      last_path: None,
      tail: None,
    }
  }
}

impl Reader {
  pub fn path(&self, id: &str, stream: Stream) -> Option<PathBuf> {
    self
      .cached
      .iter()
      .find(|entry| {
        entry.id == id
          && entry.stream == stream
          && entry.sampled.elapsed() < Duration::from_secs(60)
      })
      .map(|entry| entry.path.clone())
  }

  pub fn remember(&mut self, id: &str, stream: Stream, path: PathBuf) {
    self
      .cached
      .retain(|entry| entry.id != id || entry.stream != stream);
    if self.cached.len() == 32 {
      self.cached.pop_front();
    }
    self.cached.push_back(CachedPath {
      id: id.into(),
      stream,
      path,
      sampled: Instant::now(),
    });
  }

  pub fn invalidate(&mut self, id: &str) {
    self.cached.retain(|entry| entry.id != id);
    self.tail = None;
  }

  pub fn clear_paths(&mut self) {
    self.cached.clear();
    self.tail = None;
    // Never discard the outstanding filesystem task, even on manual refresh.
  }

  pub async fn read(&mut self, path: PathBuf, position: Position) -> Result<Output> {
    let source = path.clone();
    let previous = self
      .tail
      .as_ref()
      .filter(|(cached, _, _)| cached == &path && position == Position::Tail)
      .map(|(_, text, page)| (text.clone(), *page));
    let result = self
      .read_task(path.clone(), move || page(&source, position, previous))
      .await;
    if position == Position::Tail
      && let Ok(output) = &result
    {
      self.tail = Some((path, output.text.clone(), output.page.unwrap()));
    }
    result
  }

  async fn read_task<F>(&mut self, path: PathBuf, task: F) -> Result<Output>
  where
    F: FnOnce() -> Result<(String, Page)> + Send + 'static,
  {
    if let Some(pending) = &self.in_flight {
      ensure!(
        pending.is_finished(),
        "A previous output read is still blocked; waiting without starting another"
      );
      // The timed-out result belongs to an old request. Drain it, never display it.
      let _ = self.in_flight.take().unwrap().await;
    }
    if self.last_path.as_ref() != Some(&path) {
      self.failures = 0;
      self.retry_at = None;
      self.last_path = Some(path.clone());
    }
    ensure!(
      self.retry_at.is_none_or(|at| Instant::now() >= at),
      "Output reads are backing off after a filesystem failure"
    );
    self.in_flight = Some(tokio::task::spawn_blocking(task));
    let result = tokio::time::timeout(self.timeout, self.in_flight.as_mut().unwrap()).await;
    let result = match result {
      Ok(result) => {
        self.in_flight.take();
        result
          .context("Output read task failed")
          .and_then(|value| value)
      }
      Err(_) => Err(anyhow::anyhow!(
        "Output filesystem read timed out; the outstanding read is still tracked"
      )),
    };
    match result {
      Ok((text, page)) => {
        self.failures = 0;
        self.retry_at = None;
        Ok(Output {
          path: path.display().to_string(),
          text,
          page: Some(page),
        })
      }
      Err(error) => {
        self.failures = self.failures.saturating_add(1);
        let delay = (1u64 << self.failures.min(6)).min(60);
        self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
        Err(error).with_context(|| format!("Cannot read {}; backing off {delay}s", path.display()))
      }
    }
  }
}

#[cfg(test)]
pub async fn read(path: PathBuf) -> Result<Output> {
  read_page(path, Position::Tail).await
}

#[cfg(test)]
pub async fn read_page(path: PathBuf, position: Position) -> Result<Output> {
  Reader::default().read(path, position).await
}

fn page(
  path: &Path,
  position: Position,
  previous: Option<(String, Page)>,
) -> Result<(String, Page)> {
  use std::os::unix::fs::MetadataExt;
  let stat = std::fs::metadata(path)?;
  ensure!(stat.is_file(), "Output path is not a regular file");
  if position == Position::Tail
    && let Some((text, previous)) = previous
    && previous.device == stat.dev()
    && previous.inode == stat.ino()
    && previous.length == stat.len()
    && previous.modified == (stat.mtime(), stat.mtime_nsec())
    && previous.changed == (stat.ctime(), stat.ctime_nsec())
  {
    // Metadata checks still run in the single tracked filesystem task. No open,
    // seek, or content read is necessary for an unchanged file.
    return Ok((text, previous));
  }
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
  let text = crate::preview::render(&bytes);
  let page = Page {
    start,
    end,
    length,
    device: metadata.dev(),
    inode: metadata.ino(),
    modified: (metadata.mtime(), metadata.mtime_nsec()),
    changed: (metadata.ctime(), metadata.ctime_nsec()),
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
pub fn slurm_path(details: &str, stream: Stream) -> Result<PathBuf> {
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
  let field = match stream {
    Stream::Stdout => "StdOut=",
    Stream::Stderr => "StdErr=",
  };
  let template =
    line_field(field).with_context(|| format!("Slurm did not report a {} path", stream.name()))?;
  ensure!(
    !template.is_empty() && template != "(null)",
    "No {} file is configured for this job",
    stream.name()
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
        "Unresolved Slurm output pattern %{pattern}; cannot determine a single output file"
      ),
      None => anyhow::bail!("Incomplete output filename pattern"),
    };
    expanded.push_str(value);
  }
  let path = PathBuf::from(expanded);
  if path.is_absolute() {
    return Ok(path);
  }
  let workdir =
    PathBuf::from(line_field("WorkDir=").context("Relative output path without WorkDir")?);
  ensure!(workdir.is_absolute(), "Slurm WorkDir is not absolute");
  Ok(workdir.join(path))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn progress_updates_replay_on_poll_and_keep_raw_file_page_offsets() {
    use std::io::Write;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("job.err");
    let first = b"log line\n\rprogress 10%\x1b[";
    std::fs::write(&path, first).unwrap();
    let mut reader = Reader::default();
    let output = reader.read(path.clone(), Position::Tail).await.unwrap();
    assert_eq!(output.text, "log line\nprogress 10%");
    assert_eq!(output.page.unwrap().end, first.len() as u64);
    let mut file = std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap();
    file.write_all(b"2K\rprogress 20%\nDone\n").unwrap();
    let output = reader.read(path.clone(), Position::Tail).await.unwrap();
    assert_eq!(output.text, "log line\nprogress 20%\nDone\n");
    assert_eq!(output.page.unwrap().end, file.metadata().unwrap().len());
    assert_eq!(
      reader.read(path, Position::Tail).await.unwrap().text,
      output.text
    );
  }

  #[test]
  fn stderr_uses_its_reported_path_without_guessing_or_falling_back() {
    let details = "JobId=42 ArrayJobId=40 ArrayTaskId=2\nStdOut=/logs/results.log\nStdErr=errors %A_%a_%j_%%.log\nWorkDir=/shared/work";
    assert_eq!(
      slurm_path(details, Stream::Stderr).unwrap(),
      PathBuf::from("/shared/work/errors 40_2_42_%.log")
    );
    assert_eq!(
      slurm_path(details, Stream::Stdout).unwrap(),
      PathBuf::from("/logs/results.log")
    );
    for details in [
      "StdOut=/logs/output.out",
      "StdErr=(null)",
      "StdErr=",
      "StdErr=bad-%N.err",
    ] {
      assert!(slurm_path(details, Stream::Stderr).is_err());
    }
    assert_eq!(
      slurm_path(
        "StdErr=/logs/merged.log\nStdOut=/logs/merged.log",
        Stream::Stderr
      )
      .unwrap(),
      PathBuf::from("/logs/merged.log")
    );
  }

  #[tokio::test]
  async fn unchanged_tail_reuses_content_but_changes_and_paging_read_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("job.out");
    std::fs::write(&path, "first\n").unwrap();
    let mut reader = Reader::default();
    reader.read(path.clone(), Position::Tail).await.unwrap();
    // A marker proves the unchanged branch reused cached content.
    reader.tail.as_mut().unwrap().1 = "cached marker".into();
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "cached marker"
    );
    assert_eq!(
      reader
        .read(path.clone(), Position::Start)
        .await
        .unwrap()
        .text,
      "first\n"
    );
    std::fs::write(&path, "longer output\n").unwrap();
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "longer output\n"
    );
    std::fs::write(&path, "x\n").unwrap();
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "x\n"
    );
    // Same size is insufficient for reuse: timestamp changes must invalidate.
    reader.tail.as_mut().unwrap().1 = "stale".into();
    reader.tail.as_mut().unwrap().2.modified.0 -= 1;
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "x\n"
    );
    reader.tail.as_mut().unwrap().1 = "stale".into();
    reader.tail.as_mut().unwrap().2.changed.0 -= 1;
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "x\n"
    );
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, "y\n").unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "y\n"
    );
    reader.tail.as_mut().unwrap().1 = "stale".into();
    reader.clear_paths();
    assert_eq!(
      reader
        .read(path.clone(), Position::Tail)
        .await
        .unwrap()
        .text,
      "y\n"
    );
    std::fs::remove_file(&path).unwrap();
    assert!(reader.read(path, Position::Tail).await.is_err());
  }

  #[tokio::test]
  async fn timed_out_reads_remain_tracked_and_never_overlap() {
    use std::sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
      mpsc,
    };
    let count = Arc::new(AtomicUsize::new(0));
    let (release, wait) = mpsc::channel();
    let mut reader = Reader {
      timeout: Duration::from_millis(20),
      ..Default::default()
    };
    let calls = count.clone();
    let first = reader
      .read_task(PathBuf::from("/stalled"), move || {
        calls.fetch_add(1, Ordering::SeqCst);
        let _ = wait.recv();
        anyhow::bail!("released simulated stalled read")
      })
      .await;
    let mut retries_failed = true;
    for _ in 0..50 {
      let calls = count.clone();
      retries_failed &= reader
        .read_task(PathBuf::from("/different"), move || {
          calls.fetch_add(1, Ordering::SeqCst);
          anyhow::bail!("unexpected overlapping read")
        })
        .await
        .is_err();
    }
    reader.clear_paths();
    let tracked = reader.in_flight.is_some();
    let count_before_release = count.load(Ordering::SeqCst);
    // Always release before assertions so a failed test cannot hang its runtime.
    release.send(()).unwrap();
    let _ = reader.in_flight.take().unwrap().await;
    assert!(first.is_err());
    assert!(retries_failed && tracked);
    assert_eq!(count_before_release, 1);
    assert_eq!(reader.failures, 1);
    assert!(
      reader
        .read_task(PathBuf::from("/stalled"), || anyhow::bail!("must back off"))
        .await
        .is_err()
    );
    assert!(reader.in_flight.is_none());
    reader.retry_at = None;
    assert!(
      reader
        .read_task(PathBuf::from("/stalled"), || anyhow::bail!(
          "second failure"
        ))
        .await
        .is_err()
    );
    assert_eq!(reader.failures, 2);
    assert!(reader.retry_at.unwrap() > Instant::now() + Duration::from_secs(3));
  }

  #[test]
  fn path_cache_is_bounded_and_invalidated_explicitly() {
    let mut reader = Reader::default();
    for i in 0..40 {
      reader.remember(
        &i.to_string(),
        Stream::Stdout,
        PathBuf::from(format!("/{i}")),
      );
    }
    assert_eq!(reader.cached.len(), 32);
    assert!(reader.path("0", Stream::Stdout).is_none());
    assert!(reader.path("39", Stream::Stdout).is_some());
    reader.invalidate("39");
    assert!(reader.path("39", Stream::Stdout).is_none());
    reader.cached.back_mut().unwrap().sampled = Instant::now() - Duration::from_secs(61);
    assert!(reader.path("38", Stream::Stdout).is_none());
  }

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
      slurm_path(details, Stream::Stdout).unwrap(),
      PathBuf::from("/shared/my work/logs/slurm-100_7-123-%.out")
    );
    assert_eq!(
      slurm_path("StdOut=/shared/path with spaces.out", Stream::Stdout).unwrap(),
      PathBuf::from("/shared/path with spaces.out")
    );
    assert!(slurm_path("StdOut=slurm-%N.out\nWorkDir=/work", Stream::Stdout).is_err());
    assert!(slurm_path("StdOut=(null)", Stream::Stdout).is_err());
    assert!(slurm_path("JobId=1", Stream::Stdout).is_err());
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
    reader.remember("1", Stream::Stdout, PathBuf::from("/one"));
    reader.remember("1", Stream::Stderr, PathBuf::from("/errors"));
    assert_eq!(
      reader.path("1", Stream::Stdout),
      Some(PathBuf::from("/one"))
    );
    assert_eq!(
      reader.path("1", Stream::Stderr),
      Some(PathBuf::from("/errors"))
    );
    assert!(reader.path("2", Stream::Stdout).is_none());
    reader.invalidate("1");
    assert!(reader.path("1", Stream::Stdout).is_none());
    assert!(reader.path("1", Stream::Stderr).is_none());
  }
}

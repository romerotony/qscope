//! Application startup and event loop. Scheduler work runs separately from drawing
//! and keyboard handling so a slow controller does not freeze the terminal.

mod config;
mod details;
mod duration;
mod metadata;
mod output;
mod preview;
mod resources;
mod scheduler;
mod search;
mod source;
mod ui;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use config::{Args, Config};
use crossterm::event::{
  DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use scheduler::Job;
use source::Source;
use std::{
  io::{self, IsTerminal, Write},
  time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Operations sent from the UI to the single scheduler worker.
enum Request {
  Jobs,
  Details(String),
  Cancel(String),
  Output(String, output::Stream, output::Position, u64),
  Resources(String),
}
/// Results include the requested job ID so details remain tied to the right job
/// even if the user moves the table selection while the command is running.
enum Response {
  Jobs(Result<Vec<Job>>),
  Details(String, Result<String>),
  Cancel(String, Result<()>),
  Output(String, output::Position, u64, Result<output::Output>),
  Resources(String, Result<Box<resources::Resources>>),
}

struct SelectionGate {
  settled_at: Instant,
  next_fetch: Instant,
}

impl SelectionGate {
  fn new(now: Instant) -> Self {
    Self {
      settled_at: now,
      next_fetch: now,
    }
  }
  fn changed(&mut self, now: Instant) {
    self.settled_at = now + Duration::from_millis(250);
  }
  fn settled(&self, now: Instant) -> bool {
    now >= self.settled_at
  }
  fn ready(&self, now: Instant) -> bool {
    self.settled(now) && now >= self.next_fetch
  }
  fn dispatched(&mut self, now: Instant) {
    self.next_fetch = now + Duration::from_secs(1);
  }
  fn force(&mut self, now: Instant) {
    self.settled_at = now;
    self.next_fetch = now;
  }
}

fn stdout_interval(state: &str, seconds: u64) -> Duration {
  let terminal = matches!(
    state,
    "INACTIVE"
      | "COMPLETED"
      | "CANCELLED"
      | "CANCELED"
      | "FAILED"
      | "TIMEOUT"
      | "NODE_FAIL"
      | "OUT_OF_MEMORY"
      | "PREEMPTED"
      | "BOOT_FAIL"
      | "DEADLINE"
      | "REVOKED"
  );
  Duration::from_secs(if terminal { seconds.max(30) } else { seconds })
}

/// Restore terminal settings when leaving the UI, including on returned errors.
struct TerminalGuard;
impl Drop for TerminalGuard {
  fn drop(&mut self) {
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
  }
}

fn main() -> Result<()> {
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .worker_threads(2)
    .max_blocking_threads(2)
    .build()?;
  run_with_bounded_shutdown(runtime, |runtime| runtime.block_on(async_main()))
}

fn run_with_bounded_shutdown<T>(
  runtime: tokio::runtime::Runtime,
  run: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> T {
  // A panic must not fall through to Runtime::drop, which can wait forever on
  // a blocked filesystem syscall. Preserve the original panic after cleanup.
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&runtime)));
  // A stalled NFS syscall cannot be canceled. Do not wait indefinitely for the
  // single tracked blocking read after the terminal has been restored.
  runtime.shutdown_timeout(Duration::from_millis(200));
  match result {
    Ok(result) => result,
    Err(panic) => std::panic::resume_unwind(panic),
  }
}

async fn async_main() -> Result<()> {
  let args = Args::parse();
  let config = Config::load(&args)?;
  ensure!(
    io::stdin().is_terminal() && io::stdout().is_terminal(),
    "Run qscope in an interactive terminal"
  );
  // Choose simulation before detection: demo mode must never probe or mutate
  // a real scheduler, even on a machine where clients happen to be installed.
  if args.demo {
    return run(Source::demo(config.scheduler), &config).await;
  }
  eprintln!("Checking scheduler connectivity…");
  let mut backends = scheduler::detect(config.scheduler, config.command_timeout_secs).await?;
  // Resolve ambiguity before entering raw terminal mode, while stdin still
  // supports ordinary line-based input.
  let backend = if backends.len() == 1 {
    backends.remove(0)
  } else {
    eprintln!("Both Slurm and Flux are usable. Select a scheduler:");
    for (index, backend) in backends.iter().enumerate() {
      eprintln!("  {}. {}", index + 1, backend.name());
    }
    eprint!("Selection (or set --scheduler / config): ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input
      .trim()
      .parse()
      .map_err(|_| anyhow::anyhow!("Choose 1 or 2"))?;
    ensure!((1..=backends.len()).contains(&choice), "Choose 1 or 2");
    backends.remove(choice - 1)
  };
  run(Source::Live(backend), &config).await
}

async fn run(mut backend: Source, config: &Config) -> Result<()> {
  // Bounded channels and the `busy` flag prevent overlapping scheduler calls
  // and unbounded queues of refresh requests.
  let (requests, mut work) = mpsc::channel::<(u64, Request)>(1);
  let (results, mut responses) = mpsc::channel(1);
  let name = backend.name();
  let resource_interval = Duration::from_secs(config.resource_refresh_interval_secs);
  let worker = tokio::spawn(async move {
    let mut output_reader = output::Reader::default();
    let mut cache = metadata::Metadata::new(resource_interval);
    let mut generation = 0;
    while let Some((requested_generation, request)) = work.recv().await {
      if generation != requested_generation {
        cache.clear();
        output_reader.clear_paths();
        generation = requested_generation;
      }
      let response = match request {
        Request::Jobs => {
          let result = backend.jobs().await;
          if let Ok(jobs) = &result {
            for id in cache.observe(jobs) {
              output_reader.invalidate(&id);
            }
          }
          Response::Jobs(result)
        }
        Request::Details(id) => {
          let result = backend.details(&id, &mut cache).await;
          Response::Details(id, result)
        }
        Request::Cancel(id) => {
          let result = backend.cancel(&id).await;
          cache.invalidate(&id);
          output_reader.invalidate(&id);
          Response::Cancel(id, result)
        }
        Request::Output(id, stream, position, revision) => {
          let result = backend
            .output(&id, &mut output_reader, position, &mut cache, stream)
            .await;
          Response::Output(id, position, revision, result)
        }
        Request::Resources(id) => {
          let result = backend.resources(&id, &mut cache).await.map(Box::new);
          Response::Resources(id, result)
        }
      };
      if results
        .send((requested_generation, response))
        .await
        .is_err()
      {
        break;
      }
    }
  });
  let mut terminal = ratatui::init();
  let _guard = TerminalGuard;
  crossterm::execute!(terminal.backend_mut(), EnableMouseCapture)?;
  let mut events = EventStream::new();
  // This tick updates the display and checks deadlines; it does not query the
  // scheduler every 200 ms. Scheduler polling uses its own, slower deadline.
  let mut tick = tokio::time::interval(Duration::from_millis(200));
  tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  let mut app = ui::App::new(name);
  let mut rebuilder = ui::Rebuilder::default();
  let mut next_refresh = Instant::now();
  let mut next_output = Instant::now();
  let mut next_resources = Instant::now();
  let mut busy = false;
  let mut refresh_needed = true;
  let mut generation = 0u64;
  let mut selection_gate = SelectionGate::new(Instant::now());
  let mut dirty = true;
  let clock_started = Instant::now();
  let mut last_clock_second = 0;
  let result: Result<()> = async {
        loop {
            let previous = app.selected().map(|job| (job.id.clone(), job.state.clone(), job.nodes.clone(), job.cores.clone()));
            if rebuilder.poll(&mut app).await? {
                dirty = true;
                let current = app.selected().map(|job| (job.id.clone(), job.state.clone(), job.nodes.clone(), job.cores.clone()));
                if current != previous {
                    app.resources = None;
                    app.resources_updated = None;
                    next_resources = Instant::now();
                    next_output = Instant::now();
                }
            }
            let selected_id = app.selected().map(|job| job.id.clone());
            if selected_id != app.output_id {
                app.output_id = selected_id.clone();
                app.reset_output();
                dirty = true;
                next_output = Instant::now();
                app.resources = None;
                app.resources_updated = None;
                if app.details.as_ref().is_some_and(|(id, _)| Some(id) != selected_id.as_ref()) {
                    app.details = None;
                    app.details_focus = false;
                    app.details_request = None;
                }
                next_resources = Instant::now();
                selection_gate.changed(Instant::now());
            }
            if !busy && let Some(id) = app.details_request.take()
                && app.details.as_ref().is_some_and(|(job, _)| job == &id)
                && app.selected().is_some_and(|job| job.id == id) {
                requests.send((generation, Request::Details(id))).await?;
                busy = true;
            }
            if !busy && app.confirm.is_none()
                && let Some(id) = selected_id.clone()
                && let Some(position) = app.output_request.take() {
                requests.send((generation, Request::Output(id, app.output_stream, position, app.output_revision))).await?;
                busy = true;
            }
            // Pause refreshes during confirmation and capture the target ID
            // separately, so a changing job list cannot change what gets canceled.
            if !busy && app.confirm.is_none() && (refresh_needed || Instant::now() >= next_refresh) {
                requests.send((generation, Request::Jobs)).await?;
                busy = true;
                refresh_needed = false;
            }
            if !busy && app.confirm.is_none() && Instant::now() >= next_resources
                && selection_gate.ready(Instant::now())
                && let Some(id) = selected_id.clone() {
                requests.send((generation, Request::Resources(id))).await?;
                busy = true;
                selection_gate.dispatched(Instant::now());
            }
            if !busy && app.confirm.is_none() && Instant::now() >= next_output
                && selection_gate.settled(Instant::now()) && app.resources.is_some()
                && let Some(id) = selected_id {
                requests.send((generation, Request::Output(id, app.output_stream, output::Position::Tail, app.output_revision))).await?;
                busy = true;
            }
            let second = clock_started.elapsed().as_secs();
            if second != last_clock_second {
                last_clock_second = second;
                dirty |= app.has_running_clock();
            }
            if dirty {
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
                dirty = false;
            }
            tokio::select! {
                _ = tick.tick() => {},
                signal = tokio::signal::ctrl_c() => { signal?; break; },
                response = responses.recv() => {
                    dirty = true;
                    busy = false;
                    let (response_generation, response) = response.context("Scheduler worker stopped")?;
                    if response_generation != generation { continue; }
                    match response {
                        Response::Jobs(result) => {
                            // Wait a full interval after completion, including
                            // failures, rather than hammering a slow controller.
                            next_refresh = Instant::now() + Duration::from_secs(config.refresh_interval_secs);
                            match result {
                                Ok(jobs) => {
                                    rebuilder.request(&app, Some(jobs), false);
                                    app.status = "Connected".into();
                                }
                                Err(error) => app.status = format!("Refresh failed; showing last snapshot: {error:#}"),
                            }
                        }
                        Response::Details(id, result) => {
                            if app.selected().is_some_and(|job| job.id == id)
                                && app.details.as_ref().is_some_and(|(job, _)| job == &id) {
                                app.details = Some((id, result.unwrap_or_else(|e| format!("Details failed: {e:#}"))));
                                app.detail_scroll = 0;
                            }
                        }
                        Response::Cancel(id, result) => {
                            app.status = match result { Ok(()) => format!("Cancellation requested for {id}"), Err(e) => format!("Cancellation failed: {e:#}") };
                            refresh_needed = true;
                        }
                        Response::Output(id, position, revision, result) => {
                            let retry_secs = if result.is_err() {
                                config.refresh_interval_secs.max(config.stdout_refresh_interval_secs)
                            } else {
                                config.stdout_refresh_interval_secs
                            };
                            // Ignore results for a selection the user has left.
                            if app.selected().is_some_and(|job| job.id == id) && revision == app.output_revision {
                                match result {
                                    Ok(output) => app.apply_output_page(position, output),
                                    Err(error) => {
                                        app.output_page_failed();
                                        if app.output_follow {
                                            app.output_path.clear();
                                            app.replace_output(format!("{} unavailable: {error:#}", app.output_stream.title()));
                                        } else {
                                            app.status = format!("Output read failed: {error:#}");
                                        }
                                    }
                                }
                            }
                            next_output = Instant::now() + stdout_interval(
                                app.selected().map_or("", |job| job.state.as_str()), retry_secs);
                        }
                        Response::Resources(id, result) => {
                            if app.selected().is_some_and(|job| job.id == id) {
                                app.resources = Some((id, result.map(|resources| *resources).map_err(|error| format!("Resources unavailable: {error:#}"))));
                                app.resources_updated = Some(Instant::now());
                                next_resources = Instant::now() + Duration::from_secs(config.resource_refresh_interval_secs);
                            }
                        }
                    }
                },
                event = events.next() => {
                    let Some(event) = event else { break; };
                    let key = match event? {
                        Event::Key(key) => { dirty = true; key },
                        Event::Mouse(mouse) => {
                            if matches!(mouse.kind, crossterm::event::MouseEventKind::ScrollUp
                                | crossterm::event::MouseEventKind::ScrollDown
                                | crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)) {
                                app.handle_mouse(mouse);
                                dirty = true;
                            }
                            continue;
                        }
                        Event::Resize(_, _) => { dirty = true; continue; }
                        _ => continue,
                    };
                    if key.kind != KeyEventKind::Press { continue; }
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { break; }
                    // Modal input takes precedence over normal shortcuts: typing
                    // in a filter must not accidentally invoke cancellation.
                    if let Some(id) = app.confirm.clone() {
                        match key.code {
                            KeyCode::Char('y') if !busy => {
                                requests.send((generation, Request::Cancel(id))).await?;
                                busy = true;
                                app.confirm = None;
                            }
                            KeyCode::Esc | KeyCode::Char('n') => app.confirm = None,
                            _ => {},
                        }
                        continue;
                    }
                    if app.searching {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => {
                                app.searching = false;
                                rebuilder.request(&app, None, false);
                            },
                            KeyCode::Backspace => { app.query.pop(); rebuilder.request(&app, None, true); },
                            KeyCode::Char(c) => { app.query.push(c); rebuilder.request(&app, None, true); },
                            _ => {},
                        }
                        continue;
                    }
                    if app.handle_help(key) { continue; }
                    if app.navigate(key) { continue; }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc => { app.details = None; app.query.clear(); rebuilder.request(&app, None, false); },
                        KeyCode::Char('/') => app.searching = true,
                        KeyCode::Char('s') => { app.sort = (app.sort + 1) % 3; rebuilder.request(&app, None, false); },
                        KeyCode::Char('r') => {
                            generation = generation.wrapping_add(1);
                            rebuilder.invalidate();
                            refresh_needed = true;
                            next_output = Instant::now();
                            next_resources = Instant::now();
                            selection_gate.force(Instant::now());
                            if app.details_focus && let Some(job) = app.selected() {
                                app.details_request = Some(job.id.clone());
                            }
                        },
                        KeyCode::Enter => app.open_details(),
                        KeyCode::Char('c') if !busy => {
                            if let Some(job) = app.selected() { app.confirm = Some(job.id.clone()); }
                        },
                        _ => {},
                    }
                }
            }
        }
        Ok(())
    }.await;
  // Stop any in-flight command before returning. Backend commands use
  // kill_on_drop so aborting the worker also terminates its direct child.
  worker.abort();
  let _ = worker.await;
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn panic_shutdown_is_bounded_with_an_outstanding_filesystem_task() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
      .worker_threads(2)
      .enable_all()
      .build()
      .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
      let _ = started.send(());
      let _ = wait.recv();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      run_with_bounded_shutdown(runtime, |_| panic!("simulated UI failure"));
    }));
    let elapsed = start.elapsed();
    let _ = release.send(());
    assert!(result.is_err());
    assert!(elapsed < Duration::from_secs(2));
  }

  #[test]
  fn terminal_stdout_polling_is_slower_without_overriding_slower_configuration() {
    for state in ["COMPLETED", "FAILED", "CANCELLED", "TIMEOUT", "INACTIVE"] {
      assert_eq!(stdout_interval(state, 1), Duration::from_secs(30));
      assert_eq!(stdout_interval(state, 60), Duration::from_secs(60));
    }
    for state in [
      "RUNNING",
      "RUN",
      "PENDING",
      "SCHED",
      "COMPLETING",
      "CLEANUP",
      "UNKNOWN",
    ] {
      assert_eq!(stdout_interval(state, 1), Duration::from_secs(1));
    }
  }

  #[test]
  fn selection_requests_debounce_and_rate_limit_without_sleeping() {
    let now = Instant::now();
    let mut gate = SelectionGate::new(now);
    for i in 0..20 {
      let at = now + Duration::from_millis(i * 50);
      gate.changed(at);
      assert!(!gate.ready(at + Duration::from_millis(249)));
    }
    let settled = now + Duration::from_millis(1200);
    assert!(gate.ready(settled));
    gate.dispatched(settled);
    gate.changed(settled + Duration::from_millis(10));
    assert!(!gate.ready(settled + Duration::from_millis(300)));
    assert!(gate.ready(settled + Duration::from_secs(1)));
    gate.force(settled);
    assert!(gate.ready(settled));
  }

  #[test]
  fn shutdown_is_bounded_even_with_an_uncancelable_blocking_task() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
      let _ = started.send(());
      let _ = wait.recv();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    runtime.shutdown_timeout(Duration::from_millis(20));
    let elapsed = start.elapsed();
    let _ = release.send(());
    assert!(elapsed < Duration::from_secs(1));
  }
}

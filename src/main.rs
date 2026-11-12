//! Application startup and event loop. Scheduler work runs separately from drawing
//! and keyboard handling so a slow controller does not freeze the terminal.

mod config;
mod details;
mod duration;
mod output;
mod resources;
mod scheduler;
mod search;
mod source;
mod ui;

use anyhow::{Result, ensure};
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
  Output(String),
  Resources(String),
}
/// Results include the requested job ID so details remain tied to the right job
/// even if the user moves the table selection while the command is running.
enum Response {
  Jobs(Result<Vec<Job>>),
  Details(String, Result<String>),
  Cancel(String, Result<()>),
  Output(String, Result<output::Output>),
  Resources(String, Result<Box<resources::Resources>>),
}

/// Restore terminal settings when leaving the UI, including on returned errors.
struct TerminalGuard;
impl Drop for TerminalGuard {
  fn drop(&mut self) {
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
  }
}

#[tokio::main]
async fn main() -> Result<()> {
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
  let (requests, mut work) = mpsc::channel::<Request>(1);
  let (results, mut responses) = mpsc::channel(1);
  let name = backend.name();
  let worker = tokio::spawn(async move {
    let mut output_reader = output::Reader::default();
    while let Some(request) = work.recv().await {
      let response = match request {
        Request::Jobs => Response::Jobs(backend.jobs().await),
        Request::Details(id) => {
          let result = backend.details(&id).await;
          Response::Details(id, result)
        }
        Request::Cancel(id) => {
          let result = backend.cancel(&id).await;
          Response::Cancel(id, result)
        }
        Request::Output(id) => {
          let result = backend.output(&id, &mut output_reader).await;
          Response::Output(id, result)
        }
        Request::Resources(id) => {
          let result = backend.resources(&id).await.map(Box::new);
          Response::Resources(id, result)
        }
      };
      if results.send(response).await.is_err() {
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
  let mut next_refresh = Instant::now();
  let mut next_output = Instant::now();
  let mut next_resources = Instant::now();
  let mut busy = false;
  let mut refresh_needed = true;
  let result: Result<()> = async {
        loop {
            let selected_id = app.selected().map(|job| job.id.clone());
            if selected_id != app.output_id {
                app.output_id = selected_id.clone();
                app.reset_output();
                next_output = Instant::now();
                app.resources = None;
                if app.details.as_ref().is_some_and(|(id, _)| Some(id) != selected_id.as_ref()) {
                    app.details = None;
                    app.details_focus = false;
                    app.details_request = None;
                }
                next_resources = Instant::now();
            }
            if !busy && let Some(id) = app.details_request.take()
                && app.details.as_ref().is_some_and(|(job, _)| job == &id)
                && app.selected().is_some_and(|job| job.id == id) {
                requests.send(Request::Details(id)).await?;
                busy = true;
            }
            // Pause refreshes during confirmation and capture the target ID
            // separately, so a changing job list cannot change what gets canceled.
            if !busy && app.confirm.is_none() && (refresh_needed || Instant::now() >= next_refresh) {
                requests.send(Request::Jobs).await?;
                busy = true;
                refresh_needed = false;
            }
            if !busy && app.confirm.is_none() && Instant::now() >= next_resources
                && let Some(id) = selected_id.clone() {
                requests.send(Request::Resources(id)).await?;
                busy = true;
            }
            if !busy && app.confirm.is_none() && Instant::now() >= next_output
                && let Some(id) = selected_id {
                requests.send(Request::Output(id)).await?;
                busy = true;
                // File polling is independent of scheduler polling. The resolved
                // path is cached so this does not query Slurm every two seconds.
                next_output = Instant::now() + Duration::from_secs(2);
            }
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            tokio::select! {
                _ = tick.tick() => {},
                signal = tokio::signal::ctrl_c() => { signal?; break; },
                response = responses.recv() => {
                    busy = false;
                    match response.context("Scheduler worker stopped")? {
                        Response::Jobs(result) => {
                            // Wait a full interval after completion, including
                            // failures, rather than hammering a slow controller.
                            next_refresh = Instant::now() + Duration::from_secs(config.refresh_interval_secs);
                            match result {
                                Ok(jobs) => { app.replace_jobs(jobs); app.status = "Connected".into(); }
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
                        Response::Output(id, result) => {
                            // Failed metadata lookups should not hit the
                            // controller at the faster file-polling cadence.
                            let retry_secs = if result.is_err() { config.refresh_interval_secs } else { 2 };
                            // Ignore results for a selection the user has left.
                            if app.selected().is_some_and(|job| job.id == id) {
                                match result {
                                    Ok(output) => { app.output_path = output.path; app.replace_output(output.text); }
                                    Err(error) => { app.output_path.clear(); app.replace_output(format!("Stdout unavailable: {error:#}")); }
                                }
                            }
                            next_output = Instant::now() + Duration::from_secs(retry_secs);
                        }
                        Response::Resources(id, result) => {
                            if app.selected().is_some_and(|job| job.id == id) {
                                app.resources = Some((id, result.map(|resources| *resources).map_err(|error| format!("Resources unavailable: {error:#}"))));
                                next_resources = Instant::now() + Duration::from_secs(config.refresh_interval_secs);
                            }
                        }
                    }
                },
                event = events.next() => {
                    let Some(event) = event else { break; };
                    let key = match event? {
                        Event::Key(key) => key,
                        Event::Mouse(mouse) => { app.handle_mouse(mouse); continue; }
                        _ => continue,
                    };
                    if key.kind != KeyEventKind::Press { continue; }
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { break; }
                    // Modal input takes precedence over normal shortcuts: typing
                    // in a filter must not accidentally invoke cancellation.
                    if let Some(id) = app.confirm.clone() {
                        match key.code {
                            KeyCode::Char('y') if !busy => {
                                requests.send(Request::Cancel(id)).await?;
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
                            KeyCode::Esc | KeyCode::Enter => app.searching = false,
                            KeyCode::Backspace => { app.query.pop(); app.rebuild(None); },
                            KeyCode::Char(c) => { app.query.push(c); app.rebuild(None); },
                            _ => {},
                        }
                        continue;
                    }
                    if app.handle_help(key) { continue; }
                    if app.navigate(key) { continue; }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc => { app.details = None; app.query.clear(); app.rebuild(None); },
                        KeyCode::Char('/') => app.searching = true,
                        KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(10),
                        KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
                        KeyCode::Char('s') => { app.sort = (app.sort + 1) % 3; app.rebuild(None); },
                        KeyCode::Char('r') => { refresh_needed = true; next_output = Instant::now(); next_resources = Instant::now(); },
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

use anyhow::Context;

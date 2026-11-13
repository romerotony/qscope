//! UI state and rendering. Filtering and sorting operate on the cached snapshot;
//! drawing never invokes scheduler commands.

use crate::scheduler::Job;
use crate::search::{JobMatch, Search};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
  Frame,
  layout::{Alignment, Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span, Text},
  widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

/// Mutable UI state, owned by the event loop rather than the scheduler worker.
pub struct App {
  /// Hit targets are recorded during rendering so wheel routing follows resize.
  panes: PaneAreas,
  pub backend: &'static str,
  pub jobs: Vec<Job>,
  /// Indices into `jobs` in current filtered/sorted display order.
  pub visible: Vec<usize>,
  search: Search,
  matches: Vec<Option<JobMatch>>,
  pub table: TableState,
  pub query: String,
  pub searching: bool,
  /// Cycles through ID (0), name (1), and native state (2).
  pub sort: usize,
  pub status: String,
  /// Job ID and fetched text; independent of the current table selection.
  pub details: Option<(String, String)>,
  pub detail_scroll: u16,
  pub details_focus: bool,
  /// Shared by Enter and double-click; the worker dispatches this when idle.
  pub details_request: Option<String>,
  last_job_click: Option<(String, Instant)>,
  last_output_click: Option<(u16, u16, Instant)>,
  /// Selected job's resource snapshot, or a diagnostic from its last query.
  pub resources: Option<(String, Result<crate::resources::Resources, String>)>,
  /// Stdout follows selection. Pausing retains a stable snapshot and cursor;
  /// refreshed output is buffered until the user explicitly resumes following.
  pub output_id: Option<String>,
  pub output_path: String,
  pub output_text: String,
  pub output_cursor: usize,
  pub output_top: usize,
  pub output_follow: bool,
  output_pending: Option<String>,
  output_page: Option<crate::output::Page>,
  output_pending_page: Option<crate::output::Page>,
  output_page_loading: bool,
  pub output_request: Option<crate::output::Position>,
  pub output_revision: u64,
  pub output_focus: bool,
  pub output_maximized: bool,
  /// Tracks the first `g` of Vim's `gg` shortcut while stdout is focused.
  output_g_pending: bool,
  /// The captured cancellation target, not a row number that could change.
  pub confirm: Option<String>,
  pub help_open: bool,
  help_scroll: u16,
}

#[derive(Default)]
struct PaneAreas {
  jobs: Rect,
  job_rows: Rect,
  resources: Rect,
  details: Rect,
  output: Rect,
  output_text: Rect,
  help: Rect,
}

impl App {
  pub fn new(backend: &'static str) -> Self {
    Self {
      panes: PaneAreas::default(),
      backend,
      jobs: vec![],
      visible: vec![],
      search: Search::default(),
      matches: vec![],
      table: TableState::default(),
      query: String::new(),
      searching: false,
      sort: 2,
      status: "Connecting…".into(),
      details: None,
      detail_scroll: 0,
      details_focus: false,
      details_request: None,
      last_job_click: None,
      last_output_click: None,
      resources: None,
      output_id: None,
      output_path: String::new(),
      output_text: String::new(),
      output_cursor: 0,
      output_top: 0,
      output_follow: true,
      output_pending: None,
      output_page: None,
      output_pending_page: None,
      output_page_loading: false,
      output_request: None,
      output_revision: 0,
      output_focus: false,
      output_maximized: false,
      output_g_pending: false,
      confirm: None,
      help_open: false,
      help_scroll: 0,
    }
  }
  /// Translate the selected display row back to the underlying snapshot.
  pub fn selected(&self) -> Option<&Job> {
    self
      .table
      .selected()
      .and_then(|i| self.visible.get(i))
      .map(|&i| &self.jobs[i])
  }
  /// Preserve selection by identity when a refresh inserts or removes rows.
  pub fn replace_jobs(&mut self, jobs: Vec<Job>) {
    let id = self.selected().map(|j| j.id.clone());
    self.jobs = jobs;
    self.rebuild(id);
  }
  /// Recompute visible indices; select the first match if no ID is preserved.
  pub fn rebuild(&mut self, preserve: Option<String>) {
    self.matches = self.search.rank(&self.jobs, &self.query);
    self.visible = (0..self.jobs.len())
      .filter(|&i| self.matches[i].is_some())
      .collect();
    self.visible.sort_by(|&a, &b| {
      let relevance = self.matches[b]
        .as_ref()
        .unwrap()
        .score
        .cmp(&self.matches[a].as_ref().unwrap().score);
      if !self.query.trim().is_empty() && relevance != std::cmp::Ordering::Equal {
        return relevance;
      }
      let a = &self.jobs[a];
      let b = &self.jobs[b];
      match self.sort {
        1 => a.name.cmp(&b.name).then(a.id.cmp(&b.id)),
        2 => state_order(&a.state)
          .cmp(&state_order(&b.state))
          .then(a.state.cmp(&b.state))
          .then(a.id.cmp(&b.id)),
        // Numeric IDs sort naturally (3 before 20); array and other
        // compound IDs fall back to string ordering without parsing loss.
        _ => match (a.id.parse::<u64>(), b.id.parse::<u64>()) {
          (Ok(a), Ok(b)) => a.cmp(&b),
          _ => a.id.cmp(&b.id),
        },
      }
    });
    let selected = preserve
      .and_then(|id| self.visible.iter().position(|&i| self.jobs[i].id == id))
      .unwrap_or(0);
    self.table.select(if self.visible.is_empty() {
      None
    } else {
      Some(selected)
    });
  }
  /// Clamp navigation at table boundaries, including an empty filtered list.
  pub fn move_selection(&mut self, delta: isize) {
    if self.visible.is_empty() {
      return;
    }
    let index = self
      .table
      .selected()
      .unwrap_or(0)
      .saturating_add_signed(delta)
      .min(self.visible.len() - 1);
    self.table.select(Some(index));
  }

  pub fn reset_output(&mut self) {
    self.last_output_click = None;
    self.output_text = "Loading stdout…".into();
    self.output_path.clear();
    self.output_cursor = 0;
    self.output_top = 0;
    self.output_follow = true;
    self.output_pending = None;
    self.output_page = None;
    self.output_pending_page = None;
    self.output_page_loading = false;
    self.output_request = None;
    self.output_revision = self.output_revision.wrapping_add(1);
    self.output_g_pending = false;
  }

  pub fn replace_output(&mut self, text: String) {
    if self.output_follow {
      self.output_text = text;
      self.output_cursor = self.output_text.lines().count().saturating_sub(1);
    } else {
      // Keep at most one pending tail, bounded by the reader's page size.
      // Holding the displayed snapshot also survives file rotation/truncation.
      self.output_pending = Some(text);
    }
  }

  fn move_output(&mut self, delta: isize) {
    let last = self.output_text.lines().count().saturating_sub(1);
    if self.output_follow {
      self.output_cursor = last;
    }
    self.output_follow = false;
    if let Some(page) = self.output_page {
      if delta < 0 && self.output_cursor == 0 && page.start > 0 {
        self.request_output_page(crate::output::Position::Before(page));
      } else if delta > 0 && self.output_cursor == last && page.end < page.length {
        self.request_output_page(crate::output::Position::After(page));
      }
    }
    self.output_cursor = self.output_cursor.saturating_add_signed(delta).min(last);
  }

  fn output_start(&mut self) {
    self.output_follow = false;
    self.output_cursor = 0;
    self.output_top = 0;
    if self.output_page.is_some_and(|page| page.start > 0) {
      self.output_page_loading = false;
      self.request_output_page(crate::output::Position::Start);
    }
  }

  fn follow_output(&mut self) {
    self.output_follow = true;
    if let Some(text) = self.output_pending.take() {
      self.output_text = text;
      self.output_page = self.output_pending_page.take();
    }
    self.output_cursor = self.output_text.lines().count().saturating_sub(1);
    // Invalidate an in-flight history page and fetch the current tail.
    self.output_revision = self.output_revision.wrapping_add(1);
    self.output_request = Some(crate::output::Position::Tail);
    self.output_page_loading = false;
  }

  fn request_output_page(&mut self, position: crate::output::Position) {
    if !self.output_page_loading {
      self.output_revision = self.output_revision.wrapping_add(1);
      self.output_request = Some(position);
      self.output_page_loading = true;
    }
  }

  pub fn apply_output_page(
    &mut self,
    position: crate::output::Position,
    output: crate::output::Output,
  ) {
    self.output_path = output.path;
    self.output_page_loading = false;
    if position == crate::output::Position::Tail {
      if self.output_follow {
        self.output_page = output.page;
      } else {
        self.output_pending_page = output.page;
      }
      self.replace_output(output.text);
    } else if !self.output_follow {
      self.output_page = output.page;
      self.output_text = output.text;
      if matches!(position, crate::output::Position::Before(_)) {
        self.output_cursor = self.output_text.lines().count().saturating_sub(1);
        self.output_top = usize::MAX;
      } else {
        self.output_cursor = 0;
        self.output_top = 0;
      }
    }
  }

  pub fn output_page_failed(&mut self) {
    self.output_page_loading = false;
    if self.output_follow {
      self.output_page = None;
    }
  }

  pub fn set_output_focus(&mut self, focused: bool) {
    if self.output_focus && !focused {
      self.follow_output();
      self.output_maximized = false;
      self.last_output_click = None;
    }
    self.output_focus = focused;
  }

  fn click_output(&mut self, column: u16, row: u16) {
    self.set_output_focus(true);
    let now = Instant::now();
    let double_click = self.last_output_click.is_some_and(|(x, y, at)| {
      column.abs_diff(x) <= 1
        && row.abs_diff(y) <= 1
        && now.duration_since(at) <= Duration::from_millis(400)
    });
    if double_click {
      self.output_maximized = !self.output_maximized;
      self.last_output_click = None;
    } else {
      self.last_output_click = Some((column, row, now));
    }
  }

  fn select_job_at(&mut self, column: u16, row: u16) {
    if self.panes.job_rows.contains((column, row).into()) {
      // TableState's offset is the first visible filtered/sorted row, not a
      // source job index. Borders, column headings, and blank space aren't jobs.
      let index = self.table.offset() + usize::from(row - self.panes.job_rows.y);
      if index < self.visible.len() {
        self.table.select(Some(index));
        let id = self.jobs[self.visible[index]].id.clone();
        let now = Instant::now();
        let double_click = self.last_job_click.as_ref().is_some_and(|(previous, at)| {
          previous == &id && now.duration_since(*at) <= Duration::from_millis(400)
        });
        if double_click {
          self.open_details();
        } else {
          self.last_job_click = Some((id, now));
        }
        return;
      }
    }
    self.last_job_click = None;
  }

  pub fn open_details(&mut self) {
    if let Some(job) = self.selected() {
      let id = job.id.clone();
      self.set_output_focus(false);
      self.details = Some((id.clone(), "Loading details…".into()));
      self.details_focus = true;
      self.detail_scroll = 0;
      self.details_request = Some(id);
    }
    self.last_job_click = None;
  }

  pub fn handle_mouse(&mut self, event: MouseEvent) {
    let clicked = event.kind == MouseEventKind::Down(MouseButton::Left);
    let delta: isize = match event.kind {
      MouseEventKind::ScrollUp => -1,
      MouseEventKind::ScrollDown => 1,
      MouseEventKind::Down(MouseButton::Left) => 0,
      _ => return,
    };
    let position = (event.column, event.row).into();
    if self.confirm.is_some() || self.searching {
      self.last_job_click = None;
      self.last_output_click = None;
      return;
    }
    if self.help_open {
      self.last_job_click = None;
      self.last_output_click = None;
      if self.panes.help.contains(position) {
        self.help_scroll = self.help_scroll.saturating_add_signed(delta as i16);
      }
      return;
    }
    // A click on another pane dismisses Details; wheel events elsewhere cannot
    // accidentally move the selected job while Details is open.
    if self.details_focus {
      if clicked && (self.panes.jobs.contains(position) || self.panes.output.contains(position)) {
        self.details_focus = false;
        self.details = None;
        self.output_g_pending = false;
        self.set_output_focus(self.panes.output.contains(position));
        if self.output_focus {
          self.click_output(event.column, event.row);
        }
        self.select_job_at(event.column, event.row);
      } else if self.panes.details.contains(position) {
        self.detail_scroll = self.detail_scroll.saturating_add_signed(delta as i16);
      }
      return;
    }
    self.output_g_pending = false;
    if !clicked {
      self.last_job_click = None;
      self.last_output_click = None;
    }
    if clicked {
      if self.panes.output.contains(position) {
        self.click_output(event.column, event.row);
        self.last_job_click = None;
      } else if self.panes.jobs.contains(position) || self.panes.resources.contains(position) {
        self.set_output_focus(false);
        self.select_job_at(event.column, event.row);
      } else {
        self.last_job_click = None;
        self.last_output_click = None;
      }
      return;
    }
    if self.panes.output.contains(position) {
      self.set_output_focus(true);
      let count = self.output_text.lines().count();
      let height = usize::from(self.panes.output_text.height);
      if count == 0 || height == 0 {
        return;
      }
      self.output_follow = false;
      if let Some(page) = self.output_page {
        if delta < 0 && self.output_top == 0 && page.start > 0 {
          self.request_output_page(crate::output::Position::Before(page));
        } else if delta > 0
          && self.output_top >= count.saturating_sub(height)
          && page.end < page.length
        {
          self.request_output_page(crate::output::Position::After(page));
        }
      }
      let top = self
        .output_top
        .saturating_add_signed(delta)
        .min(count.saturating_sub(height));
      self.output_cursor = self
        .output_cursor
        .saturating_add_signed(delta)
        .clamp(top, (top + height - 1).min(count - 1));
      self.output_top = top;
    } else if self.panes.jobs.contains(position) {
      self.set_output_focus(false);
      self.move_selection(delta);
    }
  }

  /// Handle pane focus and navigation after modal/search input is processed.
  /// Returning false lets the event loop handle non-navigation shortcuts.
  pub fn navigate(&mut self, key: KeyEvent) -> bool {
    self.last_job_click = None;
    self.last_output_click = None;
    let second_g = self.output_g_pending;
    self.output_g_pending = false;
    if self.output_focus {
      match key.code {
        KeyCode::Enter => {
          self.output_maximized = !self.output_maximized;
          return true;
        }
        KeyCode::Esc | KeyCode::Char('q') if self.output_maximized => {
          self.output_maximized = false;
          return true;
        }
        _ => {}
      }
    }
    // Keyboard pane navigation cannot dismiss Details; use Enter, Esc, q, or a click.
    if self.details_focus
      && matches!(
        key.code,
        KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab
      )
    {
      return true;
    }
    // Focus changes precede scroll handling so j/k never move jobs while the
    // details pane owns keyboard navigation.
    match key.code {
      KeyCode::Char('h') | KeyCode::Left => {
        self.set_output_focus(false);
        self.details_focus = false;
        return true;
      }
      KeyCode::Char('l') | KeyCode::Right => {
        self.set_output_focus(true);
        self.details_focus = false;
        return true;
      }
      KeyCode::Tab | KeyCode::BackTab => {
        let current = if self.output_focus {
          2
        } else if self.details_focus {
          1
        } else {
          0
        };
        let panes: &[u8] = if self.details.is_some() {
          &[0, 1, 2]
        } else {
          &[0, 2]
        };
        let position = panes.iter().position(|&pane| pane == current).unwrap_or(0);
        let next = if key.code == KeyCode::BackTab {
          (position + panes.len() - 1) % panes.len()
        } else {
          (position + 1) % panes.len()
        };
        self.set_output_focus(panes[next] == 2);
        self.details_focus = panes[next] == 1;
        return true;
      }
      KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') if self.details_focus => {
        self.details_focus = false;
        self.details = None;
        self.details_request = None;
        return true;
      }
      _ => {}
    }
    if self.details_focus {
      match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
          self.detail_scroll = self.detail_scroll.saturating_add(1)
        }
        KeyCode::Char('k') | KeyCode::Up => {
          self.detail_scroll = self.detail_scroll.saturating_sub(1)
        }
        KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
        KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
        KeyCode::Char('d' | 'f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
          self.detail_scroll = self.detail_scroll.saturating_add(10)
        }
        KeyCode::Char('u' | 'b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
          self.detail_scroll = self.detail_scroll.saturating_sub(10)
        }
        KeyCode::Home => self.detail_scroll = 0,
        KeyCode::Char('?' | 'r') => return false,
        _ => {}
      }
      return true;
    }
    match key.code {
      KeyCode::Esc if self.output_focus => self.set_output_focus(false),
      KeyCode::Char('j') | KeyCode::Down if self.output_focus => {
        self.move_output(1);
      }
      KeyCode::Char('k') | KeyCode::Up if self.output_focus => {
        self.move_output(-1);
      }
      KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
      KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
      KeyCode::PageDown if self.output_focus => self.move_output(10),
      KeyCode::PageUp if self.output_focus => self.move_output(-10),
      KeyCode::Char('d' | 'f')
        if self.output_focus && key.modifiers.contains(KeyModifiers::CONTROL) =>
      {
        self.move_output(10);
      }
      KeyCode::Char('u' | 'b')
        if self.output_focus && key.modifiers.contains(KeyModifiers::CONTROL) =>
      {
        self.move_output(-10);
      }
      KeyCode::Char('g') if self.output_focus => {
        if second_g {
          self.output_start();
        } else {
          self.output_g_pending = true;
        }
      }
      KeyCode::Home if self.output_focus => self.output_start(),
      KeyCode::Char('G') | KeyCode::End if self.output_focus => self.follow_output(),
      _ => return false,
    }
    true
  }

  /// Consume all input while help is open, so shortcuts cannot affect jobs
  /// behind the popup. Search/confirmation modes are handled before this method.
  pub fn handle_help(&mut self, key: KeyEvent) -> bool {
    if !self.help_open {
      if key.code != KeyCode::Char('?') {
        return false;
      }
      self.help_open = true;
      self.help_scroll = 0;
      return true;
    }
    match key.code {
      KeyCode::Esc | KeyCode::Char('?' | 'q') => self.help_open = false,
      KeyCode::Char('j') | KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
      KeyCode::Char('k') | KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
      KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
      KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
      KeyCode::Home => self.help_scroll = 0,
      KeyCode::End => self.help_scroll = u16::MAX,
      _ => {}
    }
    true
  }
}

/// Group native Slurm/Flux states in lifecycle order. Other states remain visible
/// after the known groups rather than being mistaken for completed jobs.
fn state_order(state: &str) -> u8 {
  match state {
    "RUNNING" | "RUN" | "COMPLETING" | "CLEANUP" | "SUSPENDED" => 0,
    "PENDING" | "DEPEND" | "PRIORITY" | "SCHED" | "CONFIGURING" | "NEW" => 1,
    "FAILED" | "CANCELLED" | "CANCELED" | "TIMEOUT" | "NODE_FAIL" | "BOOT_FAIL"
    | "OUT_OF_MEMORY" | "PREEMPTED" | "DEADLINE" => 2,
    "COMPLETED" => 3,
    _ => 4,
  }
}

/// Render the job table, optional details, bottom status bar, and modal overlay.
/// Ratatui updates table scroll state when rendering the selected row.
pub fn draw(frame: &mut Frame, app: &mut App) {
  let areas = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(frame.area());
  let status_block = panel(false);
  let status_inner = status_block.inner(areas[1]);
  frame.render_widget(status_block, areas[1]);
  let status_width = Line::from(app.status.as_str())
    .width()
    .min(usize::from(status_inner.width / 2));
  let status_columns =
    Layout::horizontal([Constraint::Min(0), Constraint::Length(status_width as u16)])
      .spacing(1)
      .split(status_inner);
  frame.render_widget(
    Paragraph::new(Line::from(vec![
      Span::raw(" "),
      Span::styled("qscope", Style::default().add_modifier(Modifier::BOLD)),
      Span::raw(format!(
        " · {} · Jobs: {}/{} · Sort: {}",
        app.backend,
        app.visible.len(),
        app.jobs.len(),
        if app.query.trim().is_empty() {
          ["ID", "Name", "State"][app.sort]
        } else {
          "relevance"
        }
      )),
    ])),
    status_columns[0],
  );
  frame.render_widget(
    Paragraph::new(app.status.as_str())
      .alignment(Alignment::Right)
      .wrap(Wrap { trim: false }),
    status_columns[1],
  );
  if app.output_maximized {
    // Hidden panes must not retain clickable hit targets from the split layout.
    app.panes = PaneAreas::default();
    app.panes.output = areas[0];
    draw_output(frame, app, areas[0]);
    draw_overlays(frame, app);
    return;
  }
  let columns =
    Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(areas[0]);
  let lower_panel = if app.details.is_some() {
    Constraint::Percentage(60)
  } else {
    Constraint::Length(13)
  };
  let panels = Layout::vertical([Constraint::Min(4), lower_panel]).split(columns[0]);
  app.panes.jobs = panels[0];
  let jobs_inner = panel(false).inner(panels[0]);
  app.panes.job_rows = Rect::new(
    jobs_inner.x,
    jobs_inner.y.saturating_add(1),
    jobs_inner.width,
    jobs_inner.height.saturating_sub(1),
  );
  app.panes.resources = if app.details.is_none() {
    panels[1]
  } else {
    Rect::default()
  };
  app.panes.details = if app.details.is_some() {
    panels[1]
  } else {
    Rect::default()
  };
  app.panes.output = columns[1];
  // Keep the primary fields legible when stdout takes half the screen.
  let compact = columns[0].width < 100;
  // Day-prefixed clocks need more than the usual eight characters.
  let elapsed_width = app
    .visible
    .iter()
    .map(|&i| {
      crate::duration::elapsed(&app.jobs[i].runtime, &app.jobs[i].state)
        .chars()
        .count()
    })
    .max()
    .unwrap_or(8)
    .max(8) as u16;
  let rows: Vec<Row> = app
    .visible
    .iter()
    .map(|&i| {
      let j = &app.jobs[i];
      let elapsed = crate::duration::elapsed(&j.runtime, &j.state);
      let color = match j.state.as_str() {
        "RUNNING" | "RUN" => Color::Green,
        "PENDING" | "SCHED" | "DEPEND" | "PRIORITY" => Color::Yellow,
        "FAILED" | "CANCELLED" | "CANCELED" => Color::Red,
        _ => Color::White,
      };
      let cells = if compact {
        vec![
          (&j.id, Some(0)),
          (&j.name, Some(1)),
          (&j.state, Some(2)),
          (&elapsed, None),
        ]
      } else {
        vec![
          (&j.id, Some(0)),
          (&j.name, Some(1)),
          (&j.state, Some(2)),
          (&j.queue, Some(3)),
          (&elapsed, None),
          (&j.reason, Some(5)),
        ]
      };
      // Keep fuzzy filtering/ranking after Enter, but only emphasize matching
      // letters while the search field is actively being edited.
      let matched = app
        .matches
        .get(i)
        .and_then(Option::as_ref)
        .filter(|_| app.searching);
      Row::new(cells.into_iter().map(|(text, field)| {
        let indices = field
          .and_then(|field| matched.map(|m| m.indices[field].as_slice()))
          .unwrap_or(&[]);
        fuzzy_cell(text, indices)
      }))
      .style(Style::default().fg(color))
    })
    .collect();
  let table = Table::new(
    rows,
    if compact {
      vec![
        Constraint::Percentage(18),
        Constraint::Min(6),
        Constraint::Length(10),
        Constraint::Length(elapsed_width),
      ]
    } else {
      vec![
        Constraint::Length(12),
        Constraint::Percentage(25),
        Constraint::Length(12),
        Constraint::Length(16),
        Constraint::Length(elapsed_width.max(10)),
        Constraint::Min(10),
      ]
    },
  )
  .header(
    Row::new(if compact {
      vec!["ID", "NAME", "STATE", "ELAPSED"]
    } else {
      vec!["ID", "NAME", "STATE", "QUEUE", "ELAPSED", "REASON / NODES"]
    })
    .style(Style::default().add_modifier(Modifier::BOLD)),
  )
  .block(
    panel(!app.output_focus && !app.details_focus)
      .title(if app.visible.is_empty() {
        " Jobs — no matches "
      } else {
        " Jobs "
      })
      .title(
        Line::from(format!(
          " Filter: {}{} ",
          app.query,
          if app.searching { "▏" } else { "" }
        ))
        .right_aligned(),
      ),
  )
  .row_highlight_style(Style::default().add_modifier(Modifier::BOLD))
  .highlight_symbol(Text::styled(
    "▌ ",
    Style::default()
      .fg(Color::Cyan)
      .add_modifier(Modifier::BOLD),
  ));
  frame.render_stateful_widget(table, panels[0], &mut app.table);
  if let Some((id, text)) = &app.details {
    frame.render_widget(
      Paragraph::new(Text::from(
        text
          .lines()
          .map(|line| {
            if !line.is_empty() && !line.starts_with(char::is_whitespace) {
              Line::styled(
                line,
                Style::default()
                  .fg(Color::White)
                  .add_modifier(Modifier::BOLD),
              )
            } else {
              Line::from(line)
            }
          })
          .collect::<Vec<_>>(),
      ))
      .wrap(Wrap { trim: false })
      .scroll((app.detail_scroll, 0))
      .block(panel(app.details_focus).title(format!(" Details: {id} "))),
      panels[1],
    );
  } else {
    draw_resources(frame, app, panels[1]);
  }
  draw_output(frame, app, columns[1]);
  draw_overlays(frame, app);
}

fn draw_overlays(frame: &mut Frame, app: &mut App) {
  if let Some(id) = &app.confirm {
    let area = centered(frame.area());
    // Erase underlying table cells before drawing the confirmation overlay.
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(format!("Cancel {} job {id}?\nThis requests termination of the job.\n\ny: confirm    n / Esc: keep job", app.backend)).wrap(Wrap { trim: false }).block(panel(true).title(" Confirm cancellation ")), area);
  }
  if app.help_open {
    draw_help(frame, app);
  }
}

const HELP: &str = "Navigation
  h / Left          Focus jobs
  l / Right         Focus stdout
  Tab / Shift-Tab   Switch jobs / stdout (disabled in details)
  j/k or Down/Up    Select job / scroll focused pane
  Mouse wheel       Scroll pane under pointer (1 line)
  Left click        Select job / focus pane; closes details when leaving it
  Double click      Open job details / maximize or restore stdout
  Enter in stdout   Maximize / restore stdout
  Esc / q in maximized stdout Restore split layout

Jobs
  /                 Edit fuzzy filter (ranked by relevance)
  Enter / Esc       Finish filter editing
  s                 Cycle ID / name / state sort
  Enter             Open and focus selected job details
  Enter / Esc / q in details Close details and return to jobs
  Esc               Return from stdout, or close details / clear filter
  c                 Cancel selected job (confirmation required)
  y / n / Esc       Confirm / dismiss cancellation
  r                 Refresh jobs, resources, and stdout

Scrolling
  PgUp / PgDn       Scroll stdout or job details
  Ctrl-u / Ctrl-b   Scroll focused text pane back 10 lines
  Ctrl-d / Ctrl-f   Scroll focused text pane forward 10 lines
  gg / Home         Beginning of stdout file (stdout focused)
  G / End           Follow latest stdout (stdout focused)

General
  ?                 Open / close this menu
  q                 Close details/help if focused; otherwise quit
  Ctrl-c            Quit immediately

Help menu: j/k, arrows, PgUp/PgDn scroll; Esc / ? / q closes.";

fn draw_help(frame: &mut Frame, app: &mut App) {
  let screen = frame.area();
  let width = screen.width.min(80);
  let height = screen.height.min(HELP.lines().count() as u16 + 2);
  let area = Rect::new(
    screen.x + (screen.width - width) / 2,
    screen.y + (screen.height - height) / 2,
    width,
    height,
  );
  let block = panel(true).title(" Hotkeys · Esc / ? closes ");
  app.panes.help = area;
  let inner = block.inner(area);
  // The help text is ASCII. Wrap it explicitly so scroll limits also account
  // for narrow terminals rather than counting only the original source lines.
  let width = usize::from(inner.width).max(1);
  let lines: Vec<_> = HELP
    .lines()
    .flat_map(|line| {
      (0..line.len().max(1))
        .step_by(width)
        .map(move |start| &line[start..(start + width).min(line.len())])
    })
    .collect();
  app.help_scroll = app
    .help_scroll
    .min(lines.len().saturating_sub(inner.height as usize) as u16);
  frame.render_widget(Clear, area);
  frame.render_widget(
    Paragraph::new(lines.join("\n"))
      .scroll((app.help_scroll, 0))
      .block(block),
    area,
  );
}

/// Match offsets refer to graphemes. Splitting UTF-8 by byte or scalar offsets
/// would mis-highlight accented characters and multi-codepoint emoji.
fn fuzzy_cell(text: &str, indices: &[u32]) -> Cell<'static> {
  let spans: Vec<_> = text
    .graphemes(true)
    .enumerate()
    .map(|(i, grapheme)| {
      if indices.binary_search(&(i as u32)).is_ok() {
        Span::styled(
          grapheme.to_owned(),
          Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
      } else {
        Span::raw(grapheme.to_owned())
      }
    })
    .collect();
  Cell::from(Line::from(spans))
}

fn draw_resources(frame: &mut Frame, app: &App, area: Rect) {
  let id = app.selected().map(|job| job.id.as_str());
  let block = panel(false).title(match id {
    Some(id) => format!(" Resources: {id} "),
    None => " Resources ".into(),
  });
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let Some(id) = id else {
    frame.render_widget(Paragraph::new("Select a job to view its resources."), inner);
    return;
  };
  let Some((_, result)) = app.resources.as_ref().filter(|(job, _)| job == id) else {
    frame.render_widget(Paragraph::new("Loading resources…"), inner);
    return;
  };
  let resources = match result {
    Ok(resources) => resources,
    Err(error) => {
      frame.render_widget(
        Paragraph::new(error.as_str()).wrap(Wrap { trim: false }),
        inner,
      );
      return;
    }
  };
  let show = |value: &Option<String>| value.clone().unwrap_or_else(|| "—".into());
  let parts = Layout::vertical([Constraint::Length(6), Constraint::Min(0)])
    .spacing(1)
    .split(inner);
  let rows = [
    (
      "Nodes",
      &resources.requested.nodes,
      &resources.allocated.nodes,
    ),
    (
      "Tasks",
      &resources.requested.tasks,
      &resources.allocated.tasks,
    ),
    ("CPUs", &resources.requested.cpus, &resources.allocated.cpus),
    ("GPUs", &resources.requested.gpus, &resources.allocated.gpus),
    (
      "Memory",
      &resources.requested.memory,
      &resources.allocated.memory,
    ),
  ]
  .map(|(label, requested, allocated)| {
    Row::new(vec![label.to_string(), show(requested), show(allocated)])
  });
  frame.render_widget(
    Table::new(
      rows,
      [
        Constraint::Length(13),
        Constraint::Length(16),
        Constraint::Min(5),
      ],
    )
    .header(
      Row::new(["Resource", "Requested", "Allocated"]).style(
        Style::default()
          .fg(Color::White)
          .add_modifier(Modifier::BOLD),
      ),
    ),
    parts[0],
  );
  frame.render_widget(
    Paragraph::new(format!(
      "Elapsed: {} · Limit: {}\n\nNodes: {}",
      crate::duration::elapsed(
        resources.elapsed.as_deref().unwrap_or("—"),
        app.selected().map_or("", |job| job.state.as_str()),
      ),
      show(&resources.limit),
      show(&resources.hosts)
    ))
    .wrap(Wrap { trim: false }),
    parts[1],
  );
}

/// Keep inactive borders gray; white identifies the focused pane.
fn panel(focused: bool) -> Block<'static> {
  Block::bordered()
    .border_type(BorderType::Rounded)
    .title_style(Style::default().fg(Color::White))
    .border_style(if focused {
      Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::DarkGray)
    })
}

fn draw_output(frame: &mut Frame, app: &mut App, area: Rect) {
  let path = std::path::Path::new(&app.output_path);
  let filename = path
    .file_name()
    .map(|name| {
      let display = path
        .parent()
        .and_then(std::path::Path::file_name)
        .map(|parent| std::path::Path::new(parent).join(name))
        .unwrap_or_else(|| std::path::PathBuf::from(name));
      format!(" · {}", display.display())
    })
    .unwrap_or_default();
  let title = match &app.output_id {
    Some(id) => format!(
      " Stdout: {id} · {}{filename} ",
      if app.output_follow {
        "following"
      } else {
        "paused"
      }
    ),
    None => " Stdout ".into(),
  };
  let block = panel(app.output_focus).title(title);
  let inner = block.inner(area);
  frame.render_widget(block, area);
  app.panes.output_text = inner;
  if app.output_id.is_none() {
    frame.render_widget(Paragraph::new("Select a job to preview its stdout."), inner);
    return;
  }
  // Slice by lines rather than Paragraph's u16 scroll offset. Reads are bounded,
  // but a file containing many short lines can still exceed that offset range.
  let lines: Vec<_> = app.output_text.lines().collect();
  let height = inner.height as usize;
  app.output_cursor = app.output_cursor.min(lines.len().saturating_sub(1));
  if app.output_follow {
    app.output_cursor = lines.len().saturating_sub(1);
    app.output_top = lines.len().saturating_sub(height);
  } else {
    app.output_top = app.output_top.min(lines.len().saturating_sub(height));
    if app.output_cursor < app.output_top {
      app.output_top = app.output_cursor;
    } else if height > 0 && app.output_cursor >= app.output_top + height {
      app.output_top = app.output_cursor + 1 - height;
    }
  }
  let start = app.output_top;
  let end = (start + height).min(lines.len());
  frame.render_widget(Paragraph::new(lines[start..end].join("\n")), inner);
}

// Cap the modal dimensions to the available space to support small terminals.
fn centered(area: Rect) -> Rect {
  let width = area.width.min(64);
  let height = area.height.min(7);
  Rect::new(
    area.x + (area.width - width) / 2,
    area.y + (area.height - height) / 2,
    width,
    height,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  #[tokio::test]
  async fn stdout_can_page_before_tail_and_jump_to_start_of_large_file() {
    use crate::output::{self, Position};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.out");
    let text: String = (0..20_000).map(|i| format!("line {i:06}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let mut app = App::new("Slurm");
    app.apply_output_page(Position::Tail, output::read(path.clone()).await.unwrap());
    assert!(app.output_page.unwrap().start > 0);
    app.output_follow = false;
    app.output_cursor = 0;
    let previous_start = app.output_page.unwrap().start;
    app.move_output(-1);
    let request = app.output_request.take().unwrap();
    assert!(matches!(request, Position::Before(_)));
    app.apply_output_page(
      request,
      output::read_page(path.clone(), request).await.unwrap(),
    );
    assert!(app.output_page.unwrap().start < previous_start);
    app.output_start();
    let request = app.output_request.take().unwrap();
    assert_eq!(request, Position::Start);
    app.apply_output_page(
      request,
      output::read_page(path.clone(), request).await.unwrap(),
    );
    assert!(app.output_text.starts_with("line 000000\n"));
    app.output_cursor = app.output_text.lines().count() - 1;
    app.move_output(1);
    let request = app.output_request.take().unwrap();
    assert!(matches!(request, Position::After(_)));
    let end = app.output_page.unwrap().end;
    app.apply_output_page(
      request,
      output::read_page(path.clone(), request).await.unwrap(),
    );
    assert_eq!(app.output_page.unwrap().start, end);
    let frozen = app.output_text.clone();
    app.apply_output_page(Position::Tail, output::read(path.clone()).await.unwrap());
    assert_eq!(app.output_text, frozen);
    app.follow_output();
    assert!(app.output_text.contains("line 019999"));
    assert!(app.output_follow);
    assert_eq!(app.output_request, Some(Position::Tail));
  }
  #[test]
  fn double_click_stdout_toggles_maximize_without_changing_pause() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.output_id = Some("1".into());
    app.replace_output("one\ntwo\nthree\n".into());
    app.move_output(-1);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let click = wheel(MouseEventKind::Down(MouseButton::Left), app.panes.output);
    app.handle_mouse(click);
    assert!(app.output_focus);
    assert!(!app.output_maximized);
    app.handle_mouse(click);
    assert!(app.output_maximized);
    assert!(!app.output_follow);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.handle_mouse(click);
    app.handle_mouse(click);
    assert!(!app.output_maximized);
    assert!(app.output_focus);
    assert!(!app.output_follow);
  }

  #[test]
  fn stdout_double_click_requires_nearby_recent_clicks() {
    let mut app = App::new("Slurm");
    app.click_output(60, 4);
    app.click_output(90, 10);
    assert!(!app.output_maximized);
    app.last_output_click = Some((90, 10, Instant::now() - Duration::from_millis(500)));
    app.click_output(90, 10);
    assert!(!app.output_maximized);
    app.navigate(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    app.click_output(90, 10);
    assert!(!app.output_maximized);
  }
  #[test]
  fn enter_maximizes_stdout_and_escape_restores_without_losing_pause() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    app.output_id = Some("1".into());
    app.set_output_focus(true);
    app.replace_output("line\n".repeat(100));
    app.move_output(-5);
    app.replace_output("new lines\n".repeat(110));
    let cursor = app.output_cursor;
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.navigate(enter));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.output_maximized);
    assert_eq!(app.panes.output, Rect::new(0, 0, 120, 27));
    assert_eq!(app.panes.jobs, Rect::default());
    assert!(app.details.is_none());
    assert_eq!(app.output_cursor, cursor);
    assert!(!app.output_follow);
    app.help_open = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.panes.help.width > 0);
    app.help_open = false;
    assert!(app.navigate(escape));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!app.output_maximized);
    assert!(app.output_focus);
    assert_eq!(app.panes.output.width, 66);
    assert_eq!(app.selected().unwrap().id, "1");
    assert!(!app.output_follow);
    app.navigate(enter);
    assert!(app.output_maximized);
    assert!(app.navigate(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert!(!app.output_maximized);
    assert!(app.output_focus);
    app.navigate(enter);
    assert!(app.output_maximized);
    app.navigate(enter);
    assert!(!app.output_maximized);
    app.navigate(enter);
    app.navigate(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(!app.output_maximized);
    assert!(!app.output_focus);
    assert!(app.output_follow);
  }
  #[test]
  fn stdout_starts_at_top_and_filename_is_next_to_follow_status() {
    let backend = ratatui::backend::TestBackend::new(100, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.output_id = Some("123".into());
    app.output_path = "/shared/logs/slurm-123.out".into();
    app.replace_output("first line\nsecond line\n".into());
    terminal
      .draw(|frame| draw_output(frame, &mut app, frame.area()))
      .unwrap();
    let buffer = terminal.backend().buffer();
    let title: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
    let first: String = (1..99).map(|x| buffer[(x, 1)].symbol()).collect();
    assert!(title.contains("following · logs/slurm-123.out"));
    assert!(first.starts_with("first line"));
    assert!(!title.contains("/shared/logs"));
    assert_eq!(app.panes.output_text, Rect::new(1, 1, 98, 8));
    app.output_start();
    terminal
      .draw(|frame| draw_output(frame, &mut app, frame.area()))
      .unwrap();
    let title: String = (0..100)
      .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
      .collect();
    assert!(title.contains("paused · logs/slurm-123.out"));
  }
  #[test]
  fn q_closes_focused_details_but_remains_global_elsewhere() {
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1")]);
    app.open_details();
    let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(app.navigate(quit));
    assert!(app.details.is_none());
    assert!(!app.details_focus);
    assert!(app.details_request.is_none());
    assert_eq!(app.selected().unwrap().id, "1");
    assert!(!app.navigate(quit));
    app.set_output_focus(true);
    assert!(!app.navigate(quit));
  }
  #[test]
  fn double_click_opens_details_but_single_click_only_selects() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let click = MouseEvent {
      kind: MouseEventKind::Down(MouseButton::Left),
      column: app.panes.job_rows.x,
      row: app.panes.job_rows.y + 1,
      modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click);
    assert_eq!(app.selected().unwrap().id, "2");
    assert!(app.details.is_none());
    app.handle_mouse(click);
    assert!(app.details_focus);
    assert_eq!(app.details_request.as_deref(), Some("2"));
    assert_eq!(app.details.as_ref().unwrap().0, "2");
  }

  #[test]
  fn separate_rows_and_expired_clicks_do_not_open_details() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let mut click = MouseEvent {
      kind: MouseEventKind::Down(MouseButton::Left),
      column: app.panes.job_rows.x,
      row: app.panes.job_rows.y,
      modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(click);
    click.row += 1;
    app.handle_mouse(click);
    assert!(app.details.is_none());
    app.last_job_click = Some(("2".into(), Instant::now() - Duration::from_millis(500)));
    app.handle_mouse(click);
    assert!(app.details.is_none());
    assert!(app.details_request.is_none());
  }
  #[test]
  fn clicking_selects_correct_filtered_and_scrolled_job_row() {
    let backend = ratatui::backend::TestBackend::new(120, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs((1..=30).map(|id| job(&id.to_string())).collect());
    app.table.select(Some(29));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.table.offset() > 0);
    let target = app.table.offset() + 1;
    let expected = app.jobs[app.visible[target]].id.clone();
    app.handle_mouse(wheel(
      MouseEventKind::Down(MouseButton::Left),
      app.panes.job_rows,
    ));
    assert_eq!(app.selected().unwrap().id, expected);
    // Column headings must not select a different job.
    app.handle_mouse(wheel(
      MouseEventKind::Down(MouseButton::Left),
      app.panes.jobs,
    ));
    assert_eq!(app.selected().unwrap().id, expected);
    app.query = "30".into();
    app.rebuild(None);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let row = app.panes.job_rows;
    app.handle_mouse(MouseEvent {
      kind: MouseEventKind::Down(MouseButton::Left),
      column: row.x,
      row: row.y,
      modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.selected().unwrap().id, "30");
    // A blank row below the filtered result is not a new selection.
    app.handle_mouse(wheel(MouseEventKind::Down(MouseButton::Left), row));
    assert_eq!(app.selected().unwrap().id, "30");
  }
  #[test]
  fn clicking_switches_panes_without_scrolling_and_resumes_following_on_exit() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    app.output_id = Some("1".into());
    app.replace_output("one\ntwo\n".into());
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let click = MouseEventKind::Down(MouseButton::Left);
    app.handle_mouse(wheel(click, app.panes.output));
    assert!(app.output_focus);
    assert!(app.output_follow);
    app.move_output(-1);
    app.replace_output("one\ntwo\nthree\n".into());
    app.handle_mouse(wheel(click, app.panes.jobs));
    assert!(!app.output_focus);
    assert!(app.output_follow);
    assert!(app.output_text.contains("three"));
    assert_eq!(app.selected().unwrap().id, "1");
    app.handle_mouse(wheel(click, app.panes.output));
    app.handle_mouse(wheel(click, app.panes.resources));
    assert!(!app.output_focus);
    app.details = Some(("1".into(), "details".into()));
    app.details_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.handle_mouse(wheel(click, app.panes.output));
    assert!(!app.details_focus);
    assert!(app.details.is_none());
    assert!(app.output_focus);
    app.set_output_focus(false);
    app.details = Some(("1".into(), "details".into()));
    app.details_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.handle_mouse(wheel(click, app.panes.details));
    assert!(app.details_focus);
    app.handle_mouse(wheel(click, app.panes.jobs));
    assert!(!app.details_focus);
    assert!(app.details.is_none());
    assert!(!app.output_focus);
  }
  fn wheel(kind: MouseEventKind, area: Rect) -> MouseEvent {
    MouseEvent {
      kind,
      column: area.x + 1,
      row: area.y + 1,
      modifiers: KeyModifiers::NONE,
    }
  }

  #[test]
  fn mouse_wheel_routes_to_hovered_pane_and_pauses_stdout() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    app.output_id = Some("1".into());
    app.replace_output("line\n".repeat(100));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let top = app.output_top;
    app.handle_mouse(wheel(MouseEventKind::ScrollUp, app.panes.output));
    assert!(app.output_focus);
    assert!(!app.output_follow);
    assert_eq!(app.output_top, top - 1);
    assert_eq!(app.selected().unwrap().id, "1");
    app.replace_output("line\n".repeat(110));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.output_top, top - 1);
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.jobs));
    assert!(!app.output_focus);
    assert!(app.output_follow);
    assert_eq!(app.output_text.lines().count(), 110);
    assert_eq!(app.selected().unwrap().id, "2");
  }

  #[test]
  fn mouse_wheel_respects_details_help_and_confirmation_modals() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    app.details = Some(("1".into(), "details\n".repeat(100)));
    app.details_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.jobs));
    assert_eq!(app.selected().unwrap().id, "1");
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.output));
    assert!(!app.output_focus);
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.details));
    assert_eq!(app.detail_scroll, 1);
    app.help_open = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.help));
    assert_eq!(app.help_scroll, 1);
    assert_eq!(app.detail_scroll, 1);
    app.help_open = false;
    app.confirm = Some("1".into());
    app.handle_mouse(wheel(MouseEventKind::ScrollDown, app.panes.details));
    assert_eq!(app.detail_scroll, 1);
  }
  #[test]
  fn leaving_stdout_resumes_following_and_loads_pending_output() {
    for code in [
      KeyCode::Char('h'),
      KeyCode::Left,
      KeyCode::Tab,
      KeyCode::BackTab,
      KeyCode::Esc,
    ] {
      let mut app = App::new("Slurm");
      app.set_output_focus(true);
      app.replace_output("one\ntwo\n".into());
      app.move_output(-1);
      app.replace_output("one\ntwo\nthree\n".into());
      assert!(!app.output_follow);
      app.navigate(KeyEvent::new(code, KeyModifiers::NONE));
      assert!(!app.output_focus);
      assert!(app.output_follow);
      assert_eq!(app.output_cursor, 2);
      assert!(app.output_text.contains("three"));
      assert!(app.output_pending.is_none());
    }
  }
  #[test]
  fn stdout_cursor_moves_and_paused_snapshot_stays_put_on_updates() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.output_id = Some("1".into());
    app.output_focus = true;
    let log = |count| {
      (0..count)
        .map(|i| format!("line {i:03}\n"))
        .collect::<String>()
    };
    app.replace_output(log(100));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let following_line = app.output_cursor;
    let top = app.output_top;
    app.navigate(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.output_cursor, following_line - 1);
    assert_eq!(app.output_top, top);
    assert!(!app.output_follow);
    let cursor = app.output_cursor;
    app.replace_output(log(105));
    app.replace_output(log(110));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.output_cursor, cursor);
    assert_eq!(app.output_top, top);
    assert_eq!(app.output_text.lines().count(), 100);
    app.navigate(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.output_follow);
    assert_eq!(app.output_cursor, 109);
    assert_eq!(app.output_text.lines().count(), 110);
    assert!(app.output_pending.is_none());
  }

  #[test]
  fn stdout_cursor_scrolls_at_viewport_edges_and_reset_discards_pending() {
    let backend = ratatui::backend::TestBackend::new(120, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.output_id = Some("1".into());
    app.replace_output("line\n".repeat(100));
    app.output_start();
    app.output_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.move_output(50);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.output_top > 0);
    app.move_output(1);
    let top = app.output_top;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.output_top, top + 1);
    app.replace_output("rotated file\n".into());
    assert_eq!(app.output_text.lines().count(), 100);
    app.reset_output();
    assert!(app.output_pending.is_none());
    assert!(app.output_follow);
    assert_eq!(app.output_cursor, 0);
    assert_eq!(app.output_top, 0);
  }
  #[test]
  fn jobs_and_resources_show_clock_without_progress_bar() {
    let backend = ratatui::backend::TestBackend::new(150, 36);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    let mut running = job("1");
    running.runtime = "1800s".into();
    app.replace_jobs(vec![running]);
    let resources = crate::resources::Resources {
      elapsed: Some("1800s".into()),
      limit: Some("01:00:00".into()),
      ..Default::default()
    };
    app.resources = Some(("1".into(), Ok(resources)));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text: String = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(text.contains("30:00"));
    assert!(text.contains("Elapsed: 30:00"));
    assert!(!text.contains("Time limit:"));
    app.resources.as_mut().unwrap().1.as_mut().unwrap().limit = Some("UNLIMITED".into());
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text: String = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(|c| c.symbol())
      .collect();
    assert!(text.contains("Elapsed: 30:00 · Limit: UNLIMITED"));
  }
  #[test]
  fn focused_details_scroll_and_enter_returns_to_jobs() {
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    app.details = Some(("1".into(), "details\n".repeat(50)));
    app.details_focus = true;
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.navigate(key(KeyCode::Char('j')));
    assert_eq!(app.detail_scroll, 1);
    assert_eq!(app.selected().unwrap().id, "1");
    app.navigate(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(app.detail_scroll, 11);
    app.navigate(key(KeyCode::Char('k')));
    assert_eq!(app.detail_scroll, 10);
    for code in [
      KeyCode::Tab,
      KeyCode::BackTab,
      KeyCode::Char('h'),
      KeyCode::Char('l'),
      KeyCode::Left,
      KeyCode::Right,
      KeyCode::Char('/'),
      KeyCode::Char('s'),
    ] {
      assert!(app.navigate(key(code)));
      assert!(app.details_focus);
      assert!(!app.output_focus);
      assert_eq!(app.selected().unwrap().id, "1");
    }
    app.navigate(key(KeyCode::Enter));
    assert!(!app.details_focus);
    assert!(app.details.is_none());
    app.navigate(key(KeyCode::Char('j')));
    assert_eq!(app.selected().unwrap().id, "2");
  }
  #[test]
  fn state_sort_uses_lifecycle_order_for_slurm_and_flux() {
    for states in [
      ["COMPLETED", "FAILED", "PENDING", "RUNNING"],
      ["COMPLETED", "FAILED", "SCHED", "RUN"],
    ] {
      let mut app = App::new("Slurm");
      app.replace_jobs(
        states
          .iter()
          .enumerate()
          .map(|(i, state)| {
            let mut job = job(&(i + 1).to_string());
            job.state = (*state).into();
            job
          })
          .collect(),
      );
      let ids: Vec<_> = app
        .visible
        .iter()
        .map(|&i| app.jobs[i].id.as_str())
        .collect();
      assert_eq!(ids, ["4", "3", "2", "1"]);
    }
    assert_eq!(state_order("CANCELED"), state_order("FAILED"));
    assert_eq!(state_order("TIMEOUT"), state_order("FAILED"));
    assert!(state_order("UNKNOWN") > state_order("COMPLETED"));
  }
  #[test]
  fn filter_is_in_jobs_top_border_not_status_bar() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1")]);
    app.query = "sim".into();
    app.searching = true;
    app.rebuild(None);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let top: String = (0..66).map(|x| buffer[(x, 0)].symbol()).collect();
    assert!(top.contains("Filter: sim▏"));
    let bottom: String = (26..30)
      .flat_map(|y| (0..120).map(move |x| buffer[(x, y)].symbol()))
      .collect();
    assert!(!bottom.contains("Filter:"));
  }
  #[test]
  fn fuzzy_ranking_restores_sort_and_preserves_identity_on_refresh() {
    let mut app = App::new("Slurm");
    let mut loose = job("1");
    loose.name = "a very long b very long c".into();
    let mut tight = job("2");
    tight.name = "abc".into();
    app.replace_jobs(vec![loose.clone(), tight.clone()]);
    assert_eq!(app.selected().unwrap().id, "1");
    app.query = "abc".into();
    app.rebuild(None);
    assert_eq!(app.selected().unwrap().id, "2");
    app.move_selection(1);
    app.replace_jobs(vec![tight, loose]);
    assert_eq!(app.selected().unwrap().id, "1");
    app.query.clear();
    app.rebuild(None);
    assert_eq!(app.selected().unwrap().id, "1");
    app.query = "not-a-real-job".into();
    app.rebuild(None);
    assert!(app.selected().is_none());
  }

  #[test]
  fn fuzzy_matches_are_underlined_and_unicode_safe() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    let mut simulation = job("1");
    simulation.name = "cafe\u{301}".into();
    app.replace_jobs(vec![simulation]);
    app.query = "cafe".into();
    app.searching = true;
    app.rebuild(None);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let cell = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .find(|cell| cell.symbol() == "e\u{301}")
      .unwrap();
    assert!(cell.modifier.contains(Modifier::UNDERLINED));
    assert!(cell.modifier.contains(Modifier::BOLD));
    app.searching = false;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let cell = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .find(|cell| cell.symbol() == "e\u{301}")
      .unwrap();
    assert!(!cell.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(cell.fg, Color::Green);
    assert_eq!(app.query, "cafe");
    assert_eq!(app.visible.len(), 1);
  }
  #[test]
  fn help_is_modal_and_scrollable_on_small_terminals() {
    let backend = ratatui::backend::TestBackend::new(32, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert!(!app.handle_help(key(KeyCode::Char('j'))));
    assert!(app.handle_help(key(KeyCode::Char('?'))));
    assert!(app.help_open);
    assert!(app.handle_help(key(KeyCode::Char('c'))));
    assert!(app.confirm.is_none());
    app.handle_help(key(KeyCode::Char('j')));
    assert_eq!(app.selected().unwrap().id, "1");
    app.handle_help(key(KeyCode::End));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.help_scroll > 0);
    assert!(app.help_scroll < u16::MAX);
    app.handle_help(key(KeyCode::Char('q')));
    assert!(!app.help_open);
    app.handle_help(key(KeyCode::Char('?')));
    assert_eq!(app.help_scroll, 0);
    app.handle_help(key(KeyCode::Esc));
    assert!(!app.help_open);
  }
  #[test]
  fn resources_follow_selection_and_details_replace_panel() {
    let backend = ratatui::backend::TestBackend::new(150, 36);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    let mut resources = crate::resources::Resources::default();
    resources.requested.cpus = Some("128".into());
    resources.allocated.cpus = Some("256".into());
    app.resources = Some(("1".into(), Ok(resources)));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| -> String {
      terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
    };
    let text = rendered(&terminal);
    assert!(text.contains("Resources: 1"));
    assert!(text.contains("Requested"));
    assert!(text.contains("Allocated"));
    assert!(text.contains("256"));
    app.move_selection(1);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text = rendered(&terminal);
    assert!(text.contains("Resources: 2"));
    assert!(text.contains("Loading resources"));
    assert!(!text.contains("256"));
    app.details = Some(("2".into(), "raw details".into()));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(rendered(&terminal).contains("raw details"));
    assert!(!rendered(&terminal).contains("Resources: 2"));
  }
  #[test]
  fn boxes_have_rounded_gray_borders_and_bold_white_focus() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "╭");
    assert_eq!(buffer[(0, 27)].symbol(), "╭");
    assert_eq!(buffer[(0, 27)].fg, Color::DarkGray);
    assert_eq!(buffer[(0, 0)].fg, Color::White);
    assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
    app.output_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::DarkGray);
  }
  #[test]
  fn vim_navigation_routes_to_focused_pane() {
    let mut app = App::new("Slurm");
    app.replace_jobs(vec![job("1"), job("2")]);
    let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    app.navigate(key('j'));
    assert_eq!(app.selected().unwrap().id, "2");
    app.navigate(key('l'));
    assert!(app.output_focus);
    app.replace_output((0..20).map(|i| format!("line {i}\n")).collect());
    app.navigate(key('k'));
    assert_eq!(app.output_cursor, 18);
    assert_eq!(app.selected().unwrap().id, "2");
    app.navigate(key('j'));
    assert_eq!(app.output_cursor, 19);
    app.navigate(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.output_cursor, 9);
    app.navigate(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(app.output_cursor, 19);
    app.navigate(key('h'));
    app.navigate(key('k'));
    assert!(!app.output_focus);
    assert_eq!(app.selected().unwrap().id, "1");
  }

  #[test]
  fn vim_top_bottom_and_focus_reset_pending_g() {
    let mut app = App::new("Slurm");
    app.replace_output("one\ntwo\nthree\n".into());
    let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    app.navigate(key('l'));
    app.navigate(key('g'));
    assert!(app.output_follow);
    app.navigate(key('g'));
    assert_eq!(app.output_cursor, 0);
    assert!(!app.output_follow);
    app.navigate(key('G'));
    assert_eq!(app.output_cursor, 2);
    assert!(app.output_follow);
    app.navigate(key('g'));
    app.navigate(key('h'));
    app.navigate(key('l'));
    app.navigate(key('g'));
    assert!(app.output_follow);
    app.navigate(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.output_focus);
  }
  #[test]
  fn stdout_panel_and_colored_selection_bar_render() {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("DEMO · Slurm");
    app.replace_jobs(vec![job("1")]);
    app.output_id = Some("1".into());
    app.output_path = "/demo/slurm-1.out".into();
    app.output_text = "first line\nlatest output\n".into();
    app.output_focus = true;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("Stdout: 1"));
    assert!(text.contains("latest output"));
    let marker = buffer
      .content
      .iter()
      .find(|cell| cell.symbol() == "▌")
      .unwrap();
    assert_eq!(marker.fg, Color::Cyan);
    assert!(marker.modifier.contains(Modifier::BOLD));
  }

  #[test]
  fn stdout_scroll_clamps_and_can_return_to_tail() {
    let backend = ratatui::backend::TestBackend::new(120, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Slurm");
    app.output_id = Some("1".into());
    app.output_text = (0..100).map(|i| format!("line {i:03}\n")).collect();
    app.output_start();
    app.output_top = usize::MAX;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(app.output_top < 100);
    let text: String = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(|cell| cell.symbol())
      .collect();
    assert!(text.contains("line 000"));
    app.follow_output();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let text: String = terminal
      .backend()
      .buffer()
      .content
      .iter()
      .map(|cell| cell.symbol())
      .collect();
    assert!(text.contains("line 099"));
  }
  fn job(id: &str) -> Job {
    Job {
      id: id.into(),
      name: "sim".into(),
      owner: "alice".into(),
      state: "RUNNING".into(),
      queue: "batch".into(),
      runtime: "10s".into(),
      nodes: "1".into(),
      cores: "8".into(),
      reason: String::new(),
    }
  }
  #[test]
  fn refresh_preserves_identity_and_filter_handles_empty() {
    let mut app = App::new("Slurm");
    app.sort = 0;
    app.replace_jobs(vec![job("20"), job("3")]);
    assert_eq!(app.selected().unwrap().id, "3");
    app.move_selection(1);
    app.replace_jobs(vec![job("20"), job("3"), job("4")]);
    assert_eq!(app.selected().unwrap().id, "20");
    app.query = "missing".into();
    app.rebuild(None);
    assert!(app.selected().is_none());
    app.move_selection(1);
  }
  #[test]
  fn renders_small_terminal_and_confirmation() {
    let backend = ratatui::backend::TestBackend::new(30, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = App::new("Flux");
    app.replace_jobs(vec![job("1")]);
    app.confirm = Some("1".into());
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
  }
}

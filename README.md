# qscope

A Rust terminal dashboard for **Slurm** and **Flux Framework (Flux Core)**.
Run it directly on a cluster login node with the relevant client commands in
`PATH`. Flux connects to the broker accessible from the current environment;
it does not discover nested instances or connect over SSH.

## Build and run

Use Rust **1.88 or newer** with the current `Cargo.lock`. The 2024 edition
requires Rust 1.85+, but locked dependencies raise the minimum to 1.88.
This minimum is dependency-derived, not build-verified; a recent stable Rust
toolchain is recommended.

```sh
cargo build --release
./target/release/qscope
./target/release/qscope --scheduler slurm
./target/release/qscope --scheduler flux --refresh 30
```

Build on the HPC (or a compatible Linux build system) for HPC use. A macOS
binary cannot run on Linux. Rust is needed to build, not to run the binary.
System build/linker tools and access to Cargo's dependencies are required.

Automatic detection queries both backends. One usable backend is selected;
if both work, an interactive prompt asks which to use. Missing clients,
connection failures, and query timeouts are reported if neither works.
An explicit selection queries only that backend. No selection is saved silently.

## Demo mode (no scheduler required)

```sh
cargo run -- --demo
cargo run -- --demo --scheduler slurm
cargo run -- --demo --scheduler flux
```

Demo mode bypasses detection and never runs scheduler commands, even if clients
are installed. The status bar is prominently labeled **DEMO**. Auto defaults to a
Slurm-style demo; an explicit CLI/config scheduler selects that demo style.

Sample jobs cover running, pending, completed, and failed states. Running job
timers advance on refresh, and details provide simulated scheduler-specific
output. Normal filtering, sorting, and cancellation confirmation work. Confirmed
cancellation changes only in-memory demo data; completed/failed/canceled jobs
cannot be canceled again. Restarting resets all demo jobs. Sample paths in
details are illustrative and are never accessed.

Demo mode tests the interface, not real scheduler connectivity or compatibility.

## Configuration

Optional file: `$XDG_CONFIG_HOME/qscope/config.toml`, or
`~/.config/qscope/config.toml` if `XDG_CONFIG_HOME` is unset.
Use `--config PATH` to specify a different file. CLI flags override config.

```toml
scheduler = "auto" # auto | slurm | flux
refresh_interval_secs = 5 # job list; minimum 1
resource_refresh_interval_secs = 30 # selected-job resources; minimum 1
stdout_refresh_interval_secs = 1 # file reads; minimum 1
command_timeout_secs = 15
```

## Controls

Press `?` to open the hotkey menu. It is scrollable with `j`/`k`, arrows, or
Page Up/Down; `Esc`, `?`, or `q` closes it without quitting. While it is open,
job-management shortcuts are disabled. Status appears in the bottom bar; the
filter appears in the top border of the Jobs box. Shortcuts are listed in the popup.

| Key | Action |
| --- | --- |
| Mouse wheel | Scroll the pane under the pointer, one line per event |
| Left click | Select a job row or focus a pane; clicking another pane closes Details |
| Double click job | Open and focus Details (two clicks on the same job within 400 ms) |
| Double click Resources / Details | Switch between Resources and Details for the selected job |
| Double click stdout | Toggle maximized stdout / split layout |
| ? | Open / close hotkey menu |
| h / Left | Focus job list |
| l / Right | Focus stdout |
| Up/Down or k/j | Select job, scroll details, or move the stdout line cursor |
| / | Edit case-insensitive fuzzy filter (ID, name, owner, state, queue, reason) |
| Enter / Esc while filtering | Finish editing |
| s | Cycle ascending ID/name/state sort |
| Enter | Fetch selected job details and focus that pane |
| Enter (details focused) | Close Details and return to Jobs |
| Enter (stdout focused) | Toggle maximized stdout / 55-45 split layout |
| Esc / q (stdout maximized) | Restore split layout, keeping stdout focused |
| Page Up / Page Down | Scroll details |
| Tab / Shift-Tab | Switch jobs / stdout; disabled while details is focused |
| Ctrl-u / Ctrl-d (text pane focused) | Scroll back / forward by 10 lines |
| Ctrl-b / Ctrl-f (text pane focused) | Same as Ctrl-u / Ctrl-d |
| gg / Home (stdout focused) | Jump to beginning of the stdout file |
| G / End (stdout focused) | Resume following newest output |
| e / o | Select stderr / stdout in the output panel |
| Page Up / Page Down (stdout focused) | Scroll back / toward newest output |
| End (stdout focused) | Resume following newest output |
| c | Cancel selected job; requires explicit `y` confirmation |
| n / Esc in confirmation | Keep job |
| r | Request refresh |
| Esc | Close details and return to jobs; otherwise return from stdout or clear filter |
| q | Close focused Details/Help; otherwise quit |
| Ctrl-C | Quit |

## Backend behavior and limitations

### Fuzzy job search

Press `/` and type part of a name, ID, state, queue, owner, or reason. For example,
`clmod` matches `climate-model`. Matches are ranked by relevance and matching
characters are bold and underlined only while editing the filter; pressing Enter
removes match highlighting but retains filtering and ranking. Fields are matched independently: a match
cannot span a job's name and queue. Multiple words must match within one field.
`Enter` or `Esc` finishes editing and retains the query; `Esc` outside editing
clears it (return from stdout focus first if needed). An empty query restores
the selected ID/name/state sort; that sort also breaks equal-score ties.
Searching only examines cached jobs and does not issue scheduler requests.

### Resources panel

A panel below the job list follows the selected job, comparing **requested** and
**allocated** nodes, CPUs/cores, GPUs, and memory. It also shows elapsed time,
time limit and assigned hosts.
Jobs and Resources format elapsed time as `MM:SS`, `HH:MM:SS`, or `D-HH:MM:SS`;
pending jobs show `—`. Resources refresh at their own configured interval (30 seconds by default);
`r` also requests a refresh. `Enter` temporarily replaces it with formatted job details;
`Esc` (after returning focus from stdout, if needed) restores the resources view.

Slurm uses `ReqTRES` and `AllocTRES` from `scontrol show job`, retaining memory
scope (total, per CPU, or per node). Flux reads the jobspec hierarchy for requests
and the job's `R` allocation for assigned cores/GPUs/nodes. Flux memory is shown
as unavailable. Missing fields show `—`, not a guessed zero. These are scheduler
allocations, **not** CPU activity, memory consumption, or GPU utilization.
Demo mode provides simulated requested and allocated resources.

Below Nodes, the panel shows CPU/GPU models, architecture, and site-defined
Features. Slurm GPU models come from typed **allocated** TRES. Node metadata is
queried once per hostlist and cached for five minutes (failed lookups for one
minute); `r` clears it. CPU models are shown only when explicitly reported via
`CPUModel`; standard Slurm often exposes only architecture and node features.
Flux displays properties from the concrete job allocation as Features without
guessing hardware models. Unavailable models show `Unknown`. No SSH, remote
hardware probing, or job-side commands are used.

The Tasks row describes declared task counts, not currently active processes.
Slurm uses `NumTasks`, with task TRES where explicitly provided. Flux supports
jobspec total and per-slot task counts. Allocated tasks stay `—`
when the scheduler does not explicitly report them; CPU counts are never used to
guess how many tasks are running. Demo data includes MPI and threaded layouts.

Double-clicking Resources opens Details; double-clicking Details returns to
Resources. Single clicks do not switch views. Details retains keyboard focus until
`Enter`, `Esc`, `q`, a double-click on Details, or a click on Jobs/Stdout closes it. Pane-switching keys and
job/filter actions are ignored there; scrolling, help, refresh, and quitting
remain available.

Details uses bold section headings and aligned label/value rows: Overview,
Timing, Execution, Files, plus scheduling, exception, and resource information
when available. Missing values show `—`, long values wrap, and unknown scheduler
fields remain accessible in Additional information. Flux epoch timestamps are
displayed in UTC; Slurm timestamps retain the scheduler's reported timezone.
Demo mode uses the same formatting. There is no raw-output toggle. Resource and
stdout lookup still use the original backend responses, not the formatted text.

Under Resources, a compact **CPU/core IDs** entry lists compressed ID ranges per node/hostlist for Slurm
(`scontrol --details show job ID`) and per execution-target rank for Flux (the
job's cached `R` allocation). These are allocated resources, not live process
affinity or utilization. Slurm CPU IDs have topology-dependent core/thread
semantics; Flux core IDs are not OS logical CPU numbers. Pending jobs show
`Not allocated`; missing IDs show `Unavailable` rather than being inferred from
CPU counts. Verbose Slurm details are requested only for the inspector, not on
routine resource/stdout polls.

Mouse scrolling over Jobs changes the selection; over Stdout it scrolls and
pauses following. Scrolling back over Jobs leaves Stdout and resumes following.
While Details is focused, wheel input only affects Details; use `Esc` or click
another pane to leave. An open help menu accepts wheel scrolling and blocks changes
to panes behind it. Mouse capture is enabled during the TUI and disabled on exit;
many terminals allow native text selection by holding Shift.

### Slurm stdout / stderr panel

Press `e` to show stderr or `o` to show stdout (the default). The title identifies
the active stream. Switching resumes following and clears the previous stream's
paging history; the choice stays selected when you move between jobs. These keys
work from any pane except while editing a filter or in help/confirmation dialogs.
Each stream uses its scheduler-reported path, not a guessed `.out`/`.err` filename.

The right-hand panel follows the selected job and refreshes its output at the
stdout interval (one second by default) when the worker is available. Set it with
`--stdout-refresh SECONDS` or `stdout_refresh_interval_secs`. Scheduler jobs
refresh every five seconds by default (`--refresh SECONDS` or `refresh_interval_secs`).
Resources refresh every 30 seconds (`--resource-refresh SECONDS` or
`resource_refresh_interval_secs`). Running elapsed clocks advance locally each second
between snapshots without querying the scheduler. `r` requests an immediate
refresh of all three; slow reads remain serialized and may stretch the cadence.
The viewer resolves `StdOut` / `StdErr` and `WorkDir` from
`scontrol show job` and shares its metadata with Resources. It keeps up to 32
resolved paths for 60 seconds, rather than resolving again on each selection. It does not assume
the default filename. Paths with spaces, relative paths, and unresolved `%j`,
`%A`, `%a`, and `%%` patterns are supported; other unresolved patterns show a
diagnostic instead of guessing. The file must be readable from the node running
the TUI (typically a shared filesystem). Missing, empty, and inaccessible files
are reported in the panel; no files are created or modified.

When stdout is focused, `j`/`k` moves an internal line position without showing
a terminal cursor; the viewport scrolls when that position reaches its edge.
Navigating pauses following and holds a stable snapshot: arriving output is
buffered without shifting the view, even if the file rotates. `G` or
End loads the latest buffered output and resumes following.
Leaving the stdout pane automatically loads buffered output and resumes following.

Following reads the last 64 KiB. Navigating past a page boundary loads an earlier
or later 64 KiB page on demand, so the entire file is accessible without loading
it all into memory. `gg`/Home loads the beginning directly; `G`/End returns to the
latest tail. Pages align to line boundaries where possible; exceptionally long
lines may span pages. The current page and a single pending tail are retained.
If a file is replaced or truncated during history browsing, a diagnostic asks
you to resume following rather than mixing unrelated file contents.
The view follows the newest lines until you navigate within stdout. Long lines are clipped
to panel width. Invalid UTF-8 is replaced. A bounded, presentation-only decoder
handles carriage-return overwrites, backspace, common ANSI cursor movement, and
erase-line sequences, so tqdm progress bars update in place (including nested
bars). Colors and unsupported escape commands are discarded, never executed by
your terminal. Each 64 KiB page is decoded independently; cursor movements cannot
reach earlier pages, and a page starting mid-update may show partial context.
The original log files and byte-based paging offsets remain unchanged.
File operations have a UI timeout, though a stalled network-filesystem operation
may remain in the OS blocking pool until it returns. Only one filesystem task is
allowed in flight, even after a timeout; retries wait for it and back off up to
60 seconds. Normal shutdown waits at most 200 ms for blocking work after terminal
cleanup; this does not cancel a stalled OS syscall. Live Flux stdout/stderr is not yet
supported; demo mode provides simulated logs for both scheduler styles.

The job table uses fewer columns in narrow layouts; full fields remain available
through job details.

- **Slurm:** `squeue --local --me --array --noheader --states=all` with an explicit
  unit-separator format. Lists only jobs currently retained by the controller,
  not historical accounting (`sacct`). Array elements are separate rows.
  Details use `scontrol --details show job ID`; cancellation uses `scancel`.
- **Flux:** `flux jobs --json --count=0` lists the current user's pending and
  running jobs without the default 1,000-job cap. IDs are retained as decimal
  strings without floating-point conversion. Details use `flux jobs --json ID`;
  cancellation uses `flux cancel ID`. Native states are preserved. Owner may
  appear as a numeric UID, and unavailable fields show `—`.
- Queries are serialized off the UI loop, with timeouts. Slow queries do not
  pile up. Failed refreshes retain the last successful snapshot with an error.
  Details are fetched snapshots, not live streams. Scheduler permissions apply
  to every operation. Cancellation is not retried automatically.
- Choose an interval appropriate to your site's scheduler policy; `--refresh 30`
  or longer is preferable for large installations. Manual refreshes are coalesced.
- No submission, historical accounting, resource-utilization
  sampling, or combined scheduler view in this initial version.

## HPC load controls

- Automatic selection requests wait for 250 ms of stable selection and are
  dispatched no faster than once per second. Explicit Details and paging actions
  remain responsive. Resources resolve first so stdout can reuse the same details.
- Metadata has a 128-entry / 8 MiB cache budget. Dynamic job details expire at the
  resource interval; Flux jobspec/allocation entries expire after 60 seconds.
  Observed state, node/CPU-count, and name changes invalidate tracked jobs.
  Cancellation invalidates the target; `r` bypasses metadata and path caches.
  Failed metadata lookups are briefly cached to avoid repeated command failures.
- Rendering formats only visible job rows, caches their formatted cells and clock
  widths, and updates running clocks once per second. Idle polling ticks no longer
  force five redraws per second. Input/resize/results still redraw promptly.
- Snapshot filtering, sorting, and width scans run on one background rebuild task.
  Search edits are debounced for 150 ms; only the latest pending request is kept,
  and superseded results are discarded. The previous complete view remains usable
  until its replacement is ready. Completed work is applied on the next UI event/tick.
- Unchanged stdout tails reuse a single bounded content cache after checking file
  identity, size, modification time, and change time. This avoids repeated opens
  and content reads, but still performs a metadata check. Non-tail paging and `r`
  bypass content reuse. Finished jobs poll stdout no faster than every 30 seconds
  (slower configured intervals are preserved); state changes trigger a fresh poll.
- Tokio uses two async worker threads and at most two blocking threads, independent
  of the login node's CPU count. Filesystem work and view rebuilding remain bounded
  to one outstanding task each.

Tests cover stalled-read tracking, bounded shutdown, cache expiry/invalidation,
mock Slurm/Flux command counts, selection debounce, and a 100,000-job rendering
case. These are functional/load-regression tests, not measured HPC benchmarks.
Use your site's scheduler polling policy; `--refresh 30` is still recommended
for large installations.

## Troubleshooting crashes or apparent freezes

`Ctrl-C` exits regardless of the focused pane or filter mode; `q` can instead
close Details/help or be entered into the filter. Rebuild-worker failures now
exit rather than continuing after the panic hook has restored terminal settings.
Runtime shutdown is bounded on both normal exits and unwinding panics.

To capture a failure, run with `RUST_BACKTRACE=1` and redirect stderr to a file
on a writable local filesystem, for example:

```sh
RUST_BACKTRACE=1 ./target/release/qscope --scheduler slurm 2>qscope-error.log
```

Share the error log, scheduler, version/commit, and whether `Ctrl-C` responded.
If the shell remains garbled after the process exits, run `stty sane`.
Startup at “Checking scheduler connectivity…” waits for scheduler probes up to
the configured command timeout. Setting `--scheduler slurm` or `--scheduler flux`
avoids probing the other backend; it does not eliminate controller delays.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# Optional synthetic 100,000-job rebuild/search latency measurement:
cargo test --release large_snapshot_latency -- --ignored --nocapture
```

Parser and UI tests do not require installed schedulers. Live integration
testing must be performed on a cluster with the corresponding clients/broker.

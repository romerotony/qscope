# qscope

A Rust terminal dashboard for **Slurm** and **Flux Framework (Flux Core)**.
Run it directly on a cluster login node with the relevant client commands in
`PATH`. Flux connects to the broker accessible from the current environment;
it does not discover nested instances or connect over SSH.

## Build and run

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
refresh_interval_secs = 10 # minimum 5
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
| ? | Open / close hotkey menu |
| h / Left | Focus job list |
| l / Right | Focus stdout |
| Up/Down or k/j | Select job, scroll details, or move the stdout line cursor |
| / | Edit case-insensitive fuzzy filter (ID, name, owner, state, queue, reason) |
| Enter / Esc while filtering | Finish editing |
| s | Cycle ascending ID/name/state sort |
| Enter | Fetch selected job details and focus that pane |
| Page Up / Page Down | Scroll details |
| Tab / Shift-Tab | Switch jobs / stdout; disabled while details is focused |
| Ctrl-u / Ctrl-d (text pane focused) | Scroll back / forward by 10 lines |
| Ctrl-b / Ctrl-f (text pane focused) | Same as Ctrl-u / Ctrl-d |
| gg / Home (stdout focused) | Jump to top of retained output |
| G / End (stdout focused) | Resume following newest output |
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
pending jobs show `—`. It refreshes at the configured scheduler interval;
`r` also requests a refresh. `Enter` temporarily replaces it with raw job details;
`Esc` (after returning focus from stdout, if needed) restores the resources view.

Slurm uses `ReqTRES` and `AllocTRES` from `scontrol show job`, retaining memory
scope (total, per CPU, or per node). Flux reads the jobspec hierarchy for requests
and the job's `R` allocation for assigned cores/GPUs/nodes. Flux memory is shown
as unavailable. Missing fields show `—`, not a guessed zero. These are scheduler
allocations, **not** CPU activity, memory consumption, or GPU utilization.
Demo mode provides simulated requested and allocated resources.

The Tasks row describes declared task counts, not currently active processes.
Slurm uses `NumTasks`, with task TRES where explicitly provided. Flux supports
jobspec total and per-slot task counts. Allocated tasks stay `—`
when the scheduler does not explicitly report them; CPU counts are never used to
guess how many tasks are running. Demo data includes MPI and threaded layouts.

Details retains keyboard focus until `Esc`, `q`, or a click on Jobs/Stdout closes it. Pane-switching keys and
job/filter actions are ignored there; scrolling, help, refresh, and quitting
remain available.

Mouse scrolling over Jobs changes the selection; over Stdout it scrolls and
pauses following. Scrolling back over Jobs leaves Stdout and resumes following.
While Details is focused, wheel input only affects Details; use `Esc` or click
another pane to leave. An open help menu accepts wheel scrolling and blocks changes
to panes behind it. Mouse capture is enabled during the TUI and disabled on exit;
many terminals allow native text selection by holding Shift.

### Slurm stdout panel

The right-hand panel follows the selected job and refreshes its output every two
seconds when the worker is available. It resolves `StdOut` and `WorkDir` from
`scontrol show job` and caches the path for the current job. It does not assume
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

Only the last 64 KiB is read, with partial first lines discarded when possible.
The view follows the newest lines until you navigate within stdout. Long lines are clipped
to panel width. Invalid UTF-8 is replaced and control characters are stripped.
File operations have a UI timeout, though a stalled network-filesystem operation
may remain in the OS blocking pool until it returns. Live Flux stdout is not yet
supported; demo mode provides simulated logs for both scheduler styles.

The job table uses fewer columns in narrow layouts; full fields remain available
through job details.

- **Slurm:** `squeue --local --me --array --noheader --states=all` with an explicit
  unit-separator format. Lists only jobs currently retained by the controller,
  not historical accounting (`sacct`). Array elements are separate rows.
  Details use `scontrol show job`; cancellation uses `scancel`.
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
- No submission, historical accounting, stderr preview, resource-utilization
  sampling, or combined scheduler view in this initial version.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Parser and UI tests do not require installed schedulers. Live integration
testing must be performed on a cluster with the corresponding clients/broker.

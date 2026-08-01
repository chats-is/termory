//! Central subprocess management.
//!
//! Every subprocess Termory starts goes through this module. Before it
//! existed each call site hand-rolled its own spawn/kill/timeout, and the
//! three rules that matter were applied unevenly: two `security` probes had
//! no timeout at all, `terminal::spawn` never reaped its children (a zombie
//! per terminal launch, forever), and every kill path signalled the DIRECT
//! child only — which is the wrong process whenever the thing we spawned is
//! a wrapper (an npm shim, `$SHELL -l -i -c`).
//!
//! # Three kinds of child, deliberately different
//!
//! | Kind | Used for | Killed on app exit? | Reaped? |
//! |---|---|---|---|
//! | [`probe`] | `--version`, `security`, `which` | n/a (synchronous) | yes |
//! | [`spawn_managed`] | `codex login`, `claude update` | **yes** | yes |
//! | [`spawn_detached`] | the user's terminal | **never** | yes |
//!
//! The third row is the load-bearing one. `terminal::open` launches a
//! terminal the USER is about to work in; quitting Termory must not take it
//! down with us. So detached children are deliberately NOT registered for
//! shutdown — they are only handed to the reaper so they don't linger as
//! zombies. Do not "unify" them with the managed kind.
//!
//! # Killing the whole tree, per platform
//!
//! `Child::kill` sends SIGKILL to the direct child. When that child is a
//! wrapper the real work is a GRANDCHILD, which survives and is re-parented
//! to init — measured on a real `codex login`, which left a live OAuth
//! server holding port 1455 after its npm wrapper was killed. So a child is
//! put in a container the OS can tear down as a unit:
//!
//! - **Unix** — `process_group(0)` before spawn makes the child a process
//!   group leader (its pgid equals its pid), so every descendant that does
//!   not deliberately call `setpgid` inherits the group. `killpg` then
//!   signals all of them. Only `terminate(grace)` does SIGTERM-then-
//!   SIGKILL; every other path (probe timeout, `wait`, `Drop`) sends
//!   SIGKILL outright — see the grace-period notes on those.
//! - **Windows** — the child is assigned to a Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Descendants inherit the job
//!   automatically, `TerminateJobObject` kills the set, and — the reason
//!   this beats the Unix side — closing the handle kills it too, so a
//!   Termory that dies without running any cleanup still takes its managed
//!   children with it.
//!
//! # Known limits (do not read this module as airtight)
//!
//! - **Unix has no equivalent of KILL_ON_JOB_CLOSE.** If Termory is
//!   SIGKILLed, [`shutdown_all`] never runs and managed children survive.
//!   (`PR_SET_PDEATHSIG` is Linux-only and fires on the direct child only,
//!   so it would not cover the grandchild case this module exists for.)
//! - **Windows has a spawn/assign race.** The job can only be assigned
//!   after `CreateProcess` returns, so a grandchild forked in that window
//!   escapes. Closing it needs `CREATE_SUSPENDED` + `ResumeThread`, which
//!   neither `std` nor `tokio` expose.
//! - A child that deliberately breaks away (`setsid`, `CREATE_BREAKAWAY_
//!   FROM_JOB`) leaves the container by design. None of the CLIs we spawn
//!   do this today.

use std::io;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Budget for a silent probe (`--version`, a Keychain read, `which`).
///
/// The risky one is not the version query but `security(1)`: with a LOCKED
/// login keychain it puts up an unlock dialog and blocks until the user
/// answers it, so an untimed call pins the calling thread indefinitely.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace for a child that may need to clean up after itself — an
/// installer mid-download, which should get the chance to remove a
/// partial file rather than be cut off.
pub(crate) const INSTALL_GRACE: Duration = Duration::from_secs(2);

/// Grace for a cancel the USER just asked for. They clicked the button
/// and are watching the UI, so the priority is that it stops NOW; a
/// login has no half-written state worth protecting (the credential
/// rollback is ours, not the child's).
///
/// **This is deliberately not the same number as [`INSTALL_GRACE`].** A
/// single blanket 2s made cancelling a codex login take up to FOUR
/// seconds — 2s waiting for the HTTP `/cancel` to take effect, then 2s
/// again here — where the pre-existing behaviour was 2s and then an
/// immediate kill. Measured: a child that ignores SIGTERM costs the full
/// grace every time (2.05s), one that handles it costs 0.03s.
pub(crate) const CANCEL_GRACE: Duration = Duration::from_millis(300);

/// Probe polling bounds. Starts tight so a fast child (a `--version`
/// that exits in ~1ms) is noticed almost immediately, then backs off so a
/// slow one costs nearly nothing to wait on. A flat 50ms — what this
/// replaced — put a 50ms floor under every single probe.
const POLL_MIN: Duration = Duration::from_millis(1);
const POLL_MAX: Duration = Duration::from_millis(50);

/// Shorter grace on app exit — the user asked to quit, and a wedged child
/// must not hold the process open. Long enough for a shell to pass the
/// signal on, short enough to be invisible.
const EXIT_GRACE: Duration = Duration::from_millis(300);

/// Windows `CREATE_NO_WINDOW` process-creation flag. A GUI-subsystem
/// parent flashes a console WINDOW for every console child, and `.cmd`
/// shims (every npm-installed CLI) are additionally routed through
/// `cmd.exe` by the Rust runtime — so even `codex --version` pops a
/// window without this.
///
/// Applied automatically by [`probe`] and [`spawn_managed`]. It must NEVER
/// reach a terminal launch the user is meant to see, which is why
/// [`spawn_detached`] does not apply it and `terminal.rs` opts in per
/// branch via [`hide_console`].
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Apply the no-console-window creation flag on Windows; no-op elsewhere.
pub(crate) fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

// ===================================================================
// Platform layer — one container per managed child
// ===================================================================

/// A killable container holding a child and everything it spawns.
///
/// Unix: the process-group id (equal to the child's pid, since the child
/// is made group leader). Windows: a Job Object handle, stored as `isize`
/// so the value is `Send` without a wrapper.
#[derive(Clone, Copy)]
struct Group(isize);

#[cfg(unix)]
impl Group {
    /// Ask for a new process group at spawn time. Must be called BEFORE
    /// spawn — after it, the child may already have forked.
    fn prepare(cmd: &mut Command) {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }

    fn prepare_tokio(cmd: &mut tokio::process::Command) {
        cmd.process_group(0);
    }

    /// The child leads its own group, so its pid IS the group id.
    fn adopt(pid: u32, _handle: isize) -> Option<Self> {
        // pid 0 / 1 would mean "my group" / init — never a child of ours,
        // and signalling either would be catastrophic.
        (pid > 1).then_some(Self(pid as isize))
    }

    fn signal(self, sig: i32) -> bool {
        // SAFETY: killpg with a group id we created ourselves. A dead
        // group returns ESRCH, which is reported as `false`, not UB.
        unsafe { libc::killpg(self.0 as libc::pid_t, sig) == 0 }
    }

    /// Is any process still in the group? Signal 0 performs the
    /// permission/existence checks without delivering anything.
    fn alive(self) -> bool {
        self.signal(0)
    }

    /// Politely ask the whole group to exit.
    fn terminate(self) {
        self.signal(libc::SIGTERM);
    }

    /// Uncatchable. Only used after a grace period.
    fn kill(self) {
        self.signal(libc::SIGKILL);
    }

    /// Nothing to free — a pgid is not a resource.
    fn release(self) {}
}

#[cfg(windows)]
impl Group {
    /// Nothing to do pre-spawn: on Windows the container is created
    /// separately and the child assigned to it afterwards.
    fn prepare(_cmd: &mut Command) {}

    fn prepare_tokio(_cmd: &mut tokio::process::Command) {}

    /// Create a kill-on-close Job Object and put the child in it.
    ///
    /// `KILL_ON_JOB_CLOSE` is what makes this survive a Termory that never
    /// gets to run its own cleanup: when our process dies the last handle
    /// closes and the OS tears the job down.
    fn adopt(_pid: u32, handle: isize) -> Option<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        if handle == 0 {
            return None;
        }
        // SAFETY: all three calls take handles we own; the info struct is
        // zeroed and sized exactly as the API requires.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(job, handle as *mut std::ffi::c_void) != 0;
            if !ok {
                // Leaving the job assigned-to-nothing would be harmless,
                // but closing it now keeps the handle count honest.
                windows_sys::Win32::Foundation::CloseHandle(job);
                return None;
            }
            Some(Self(job as isize))
        }
    }

    /// Always false, and that is not a stub.
    ///
    /// This is only ever consulted AFTER `terminate`, and on Windows
    /// `terminate` is `TerminateJobObject` — synchronous and immediate for
    /// the whole job, with no catchable signal and nothing to wait out. So
    /// "is anything still running" is answered by construction.
    ///
    /// Returning `true` here (the obvious placeholder) is wrong in a way
    /// that shows up as a HANG: `shut_down` would poll a group it has
    /// already destroyed for the full grace period, adding 2s to every
    /// login cancel on Windows and 300ms to every quit.
    fn alive(self) -> bool {
        false
    }

    /// A Job Object has no graceful stop — `TerminateJobObject` is the only
    /// verb, so TERM and KILL are the same call here. Nothing is lost: the
    /// grace period exists to let a child handle SIGTERM, and Windows has
    /// no equivalent to hand it.
    fn terminate(self) {
        self.kill();
    }

    fn kill(self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.0` is a job handle we created and have not closed.
        unsafe {
            TerminateJobObject(self.0 as *mut std::ffi::c_void, 1);
        }
    }

    /// Closing the last handle also kills whatever is still inside (see
    /// `adopt`), so this doubles as the final sweep.
    fn release(self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: as above — closed exactly once, from `Managed::drop` or
        // the shutdown sweep.
        unsafe {
            CloseHandle(self.0 as *mut std::ffi::c_void);
        }
    }
}

impl Group {
    /// Terminate, wait out `grace`, then kill anything left.
    ///
    /// Split from the signal calls so both the per-child `terminate` and
    /// the app-exit sweep escalate identically.
    fn shut_down(self, grace: Duration) {
        self.terminate();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !self.alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill();
    }
}

/// Build a [`Group`] around a freshly spawned std child.
fn adopt_std(child: &std::process::Child) -> Option<Group> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        Group::adopt(child.id(), child.as_raw_handle() as isize)
    }
    #[cfg(not(windows))]
    Group::adopt(child.id(), 0)
}

/// Build a [`Group`] around a freshly spawned tokio child.
fn adopt_tokio(child: &tokio::process::Child) -> Option<Group> {
    let pid = child.id()?;
    #[cfg(windows)]
    {
        Group::adopt(pid, child.raw_handle().map(|h| h as isize).unwrap_or(0))
    }
    #[cfg(not(windows))]
    Group::adopt(pid, 0)
}

// ===================================================================
// Registry — what app exit has to clean up
// ===================================================================

/// Live managed groups, keyed by a monotonic ticket so a [`Managed`] can
/// deregister itself without depending on pid reuse.
///
/// Probes are absent (they are torn down before their function returns)
/// and so are detached children (killing the user's terminal on quit is
/// the exact thing this module refuses to do).
static REGISTRY: Mutex<Vec<(u64, std::sync::Arc<GroupOwner>)>> = Mutex::new(Vec::new());
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// Shared ownership of one group, so the container is freed exactly once.
///
/// The registry and the [`Managed`] handle both hold one of these, and
/// either can outlive the other: [`shutdown_all`] takes the registry's
/// copy while a login flow still holds its handle, and a `Managed` dropped
/// after that still has a group to signal. On Windows `release` is
/// `CloseHandle`, so letting both sides free it would be a double close —
/// `Arc` makes the LAST one out do it, once.
struct GroupOwner(Group);

impl Drop for GroupOwner {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn register(group: Group) -> (u64, std::sync::Arc<GroupOwner>) {
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let owner = std::sync::Arc::new(GroupOwner(group));
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.push((ticket, std::sync::Arc::clone(&owner)));
    }
    (ticket, owner)
}

fn deregister(ticket: u64) {
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.retain(|(t, _)| *t != ticket);
    }
}

/// Serialises the registry-wide sweep against every test holding a live
/// managed child — **including tests in OTHER modules**.
///
/// [`shutdown_all`] drains the GLOBAL registry, so under the suite's
/// parallelism it kills children belonging to whatever else is running.
/// `accounts::tests` spawns managed children too (the `stop_codex_login`
/// pair), and while their windows are short enough that the collision was
/// never observed in ~20 full runs, "narrow" is timing luck rather than a
/// guarantee. Tests holding a child take the read side; the sweep takes
/// the write side.
#[cfg(test)]
pub(crate) static SWEEP_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Read guard: "a managed child of mine is alive, don't sweep."
#[cfg(test)]
pub(crate) fn hold_children() -> std::sync::RwLockReadGuard<'static, ()> {
    SWEEP_LOCK.read().unwrap_or_else(|e| e.into_inner())
}

/// Is this exact group still scheduled for shutdown?
///
/// Tests assert by IDENTITY rather than by registry LENGTH: the suite runs
/// in parallel, so a sibling test registering its own child would move any
/// count-based assertion.
#[cfg(test)]
fn registry_holds(predicate: impl Fn(u64, isize) -> bool) -> bool {
    REGISTRY
        .lock()
        .map(|reg| reg.iter().any(|(t, owner)| predicate(*t, owner.0 .0)))
        .unwrap_or(false)
}

/// Tear down every managed child, blocking briefly while they exit.
///
/// Called from the app's `RunEvent::Exit`. Detached children (terminals)
/// are untouched by design.
pub fn shutdown_all() {
    let owners: Vec<std::sync::Arc<GroupOwner>> = match REGISTRY.lock() {
        Ok(mut reg) => reg.drain(..).map(|(_, owner)| owner).collect(),
        Err(_) => return,
    };
    if owners.is_empty() {
        return;
    }
    log::info!("process: shutting down {} managed child(ren)", owners.len());
    // TERM every group before waiting on any of them, so the grace
    // periods overlap instead of stacking up.
    for owner in &owners {
        owner.0.terminate();
    }
    let deadline = Instant::now() + EXIT_GRACE;
    while Instant::now() < deadline && owners.iter().any(|o| o.0.alive()) {
        std::thread::sleep(Duration::from_millis(20));
    }
    for owner in &owners {
        owner.0.kill();
    }
    // Dropping the Arcs releases each container — unless a `Managed` is
    // still holding its own reference, in which case its drop does.
}

// ===================================================================
// Reaper — the zombie fix
// ===================================================================

/// Background thread that waits on children nobody else will.
///
/// `std::process::Child::drop` does NOT reap (the standard library says so
/// outright: there is no way to prevent the child from becoming a zombie).
/// `terminal::open` used to drop its `Child` immediately, leaving one
/// zombie per terminal launch for the lifetime of the app.
///
/// One shared thread rather than a thread per child: a terminal emulator
/// is a long-lived FOREGROUND process, so a thread blocked in `wait()`
/// would stay parked for the whole session.
fn reaper() -> &'static mpsc::Sender<std::process::Child> {
    static REAPER: OnceLock<mpsc::Sender<std::process::Child>> = OnceLock::new();
    REAPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<std::process::Child>();
        let spawned = std::thread::Builder::new()
            .name("termory-reaper".into())
            .spawn(move || {
                let mut pending: Vec<std::process::Child> = Vec::new();
                loop {
                    // Block when there is nothing to poll, so an idle app
                    // pays nothing; poll on a timer once we hold children.
                    if pending.is_empty() {
                        match rx.recv() {
                            Ok(child) => pending.push(child),
                            // Sender is a leaked static, so this is
                            // unreachable in practice — bail rather than
                            // spin if it ever happens.
                            Err(_) => return,
                        }
                    } else if let Ok(child) = rx.recv_timeout(Duration::from_millis(500)) {
                        pending.push(child);
                    }
                    // `try_wait` reaps the ones that have exited; the rest
                    // stay for the next pass.
                    pending.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
                }
            });
        if let Err(err) = spawned {
            log::error!("process: reaper thread failed to start: {err}");
        }
        tx
    })
}

// ===================================================================
// 1. Probes — synchronous, timed, torn down before returning
// ===================================================================

/// Run `cmd` to completion and return its output, killing the whole group
/// if it outlives `timeout`.
///
/// `None` means spawn failure or timeout. A NON-ZERO exit still returns
/// `Some` — callers inspect `status` (the Keychain paths rely on reading
/// exit 44/36).
pub(crate) fn probe(cmd: Command, timeout: Duration) -> Option<Output> {
    probe_with_stdin(cmd, None, timeout)
}

/// [`probe`] with an optional stdin payload.
///
/// Exists because `security -i` — the form Claude Code itself uses to
/// write the Keychain (macOsKeychainStorage.ts:117) — reads its command
/// from stdin, so the credential never appears in `argv` where a process
/// monitor could see it. Passing `None` wires stdin to `/dev/null`, which
/// would make that command read EOF and write nothing.
///
/// `input` is written and the pipe closed before the wait loop. Safe
/// against deadlock at our payload sizes — a pipe buffers 64 KB and the
/// largest thing we send is a few KB of hex.
pub(crate) fn probe_with_stdin(
    mut cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Option<Output> {
    use std::io::Write as _;

    // Every probe is a silent helper — never a window the user should see.
    hide_console(&mut cmd);
    Group::prepare(&mut cmd);

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()
        .ok()?;
    let group = adopt_std(&child);

    if let Some(bytes) = input {
        // Take the handle so it drops (closing the pipe) before we wait —
        // `security -i` keeps reading until EOF.
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(bytes).is_err() {
                finish_probe(&mut child, group);
                return None;
            }
        }
    }

    // Drain both pipes on their own threads for the whole run. A pipe
    // holds ~64 KB, and a child that fills one BLOCKS until somebody
    // reads — so polling `try_wait` without reading deadlocks the child
    // against its own output and the probe only ever ends by timing out.
    // Reading after the wait loop (the old `wait_with_output`) is too
    // late for exactly that reason.
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());

    // Poll with a backoff rather than a flat interval. A version probe
    // exits in ~1ms, so a fixed 50ms sleep made EVERY probe cost 50ms —
    // measured at 55ms/call against 1.2ms for a bare `Command::output`,
    // and the tray pays that per CLI on every menu open. Starting at 1ms
    // and doubling keeps the fast case fast while a genuinely slow child
    // (the interactive-shell fallback, ~1s) still settles onto a cheap
    // interval instead of spinning.
    let mut poll = POLL_MIN;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The direct child is done; anything it left behind still
                // belongs to the group and is not ours to keep alive.
                // This also closes any pipe copy a grandchild inherited,
                // which is what lets the reader threads reach EOF.
                if let Some(group) = group {
                    group.kill();
                    group.release();
                }
                return Some(Output {
                    status,
                    stdout: stdout_reader.join().unwrap_or_default(),
                    stderr: stderr_reader.join().unwrap_or_default(),
                });
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    finish_probe(&mut child, group);
                    return None;
                }
                std::thread::sleep(poll);
                poll = (poll * 2).min(POLL_MAX);
            }
            Err(_) => {
                finish_probe(&mut child, group);
                return None;
            }
        }
    }
    // NOTE: the failure paths deliberately do NOT join the readers. Their
    // output is discarded anyway, and joining would re-introduce the hang
    // this function exists to avoid — a grandchild that escaped the group
    // still holds the pipe, so the read would never return. The threads
    // end on their own once the last writer closes.
}

/// Read a pipe to EOF on its own thread.
///
/// Returns the bytes so the caller can assemble an [`Output`]; a read
/// error yields what was collected so far rather than failing the probe.
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
        }
        buf
    })
}

/// Kill the group (when `kill_group`) and reap the direct child, so a
/// probe never leaves either a survivor or a zombie behind.
///
/// **No grace period here, deliberately.** A probe that timed out has
/// already abandoned the result, so there is nothing for the child to
/// finish writing — a polite SIGTERM followed by a wait buys nobody
/// anything, and a child that ignores it pays the full wait. Giving this
/// path a 2s grace turned a 5s probe timeout into 7s on a HOT path: the
/// tray re-probes every uninstalled CLI on each menu open.
fn finish_probe(child: &mut std::process::Child, group: Option<Group>) {
    if let Some(group) = group {
        group.kill();
        group.release();
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ===================================================================
// 2. Managed children — long-running, killable, cleaned up on exit
// ===================================================================

/// A running child plus everything it spawned, tracked for app shutdown.
///
/// Dropping it kills the group: a login flow that returns early (an error
/// path, a cancelled future) can never leave the CLI running behind it.
pub struct Managed {
    child: tokio::process::Child,
    group: Option<std::sync::Arc<GroupOwner>>,
    ticket: Option<u64>,
    /// Set once the child is known to have exited, so `drop` doesn't
    /// signal a group whose id could by then belong to something else.
    finished: bool,
}

/// Spawn a child that Termory owns for as long as it runs.
///
/// Applies the console-hiding flag and the process group, registers the
/// group for [`shutdown_all`], and sets `kill_on_drop` so the direct child
/// is reaped even on a path that forgets to wait.
///
/// The caller still configures stdio — the login flows read their auth URL
/// off a pipe, the upgrade path drains stdout and discards stderr.
pub fn spawn_managed(mut cmd: tokio::process::Command) -> io::Result<Managed> {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    Group::prepare_tokio(&mut cmd);
    cmd.kill_on_drop(true);

    let child = cmd.spawn()?;
    let registered = match adopt_tokio(&child) {
        Some(group) => Some(register(group)),
        None => {
            // Not fatal: we still hold the direct child and can kill it.
            // Worth a line, because the grandchild guarantee is gone for
            // this one.
            log::warn!("process: spawned a child with no group container");
            None
        }
    };
    let (ticket, group) = match registered {
        Some((ticket, owner)) => (Some(ticket), Some(owner)),
        None => (None, None),
    };
    Ok(Managed {
        child,
        group,
        ticket,
        finished: false,
    })
}

impl Managed {
    /// Take the child's stdout pipe (once).
    pub fn stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the child's stderr pipe (once).
    pub fn stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// Wait for the direct child to exit, then sweep whatever it left
    /// behind.
    ///
    /// **The sweep is not optional.** A wrapper exiting says nothing about
    /// the process doing the work — that asymmetry is the whole premise of
    /// this module, and it cuts both ways: the measured codex case was a
    /// wrapper KILLED with its grandchild surviving, but a wrapper that
    /// exits NORMALLY can leave one just the same. Leaving this to `Drop`
    /// missed it entirely, because `Drop` skips the group once `finished`
    /// is set.
    ///
    /// Killing here rather than in `Drop` is also what makes it SAFE.
    /// POSIX keeps a process-group id reserved for as long as the group is
    /// non-empty, so while a grandchild is alive this `killpg` can only
    /// reach our own descendants; once the group empties, the id may be
    /// recycled and a later signal could hit a stranger. Firing at the
    /// moment the child is reaped keeps that window at microseconds, where
    /// deferring it to an arbitrarily later `Drop` does not. An
    /// already-empty group just returns ESRCH, which is ignored.
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            if let Some(group) = self.group.as_ref().map(|owner| owner.0) {
                // No grace: the direct child is already gone, so anything
                // still in the group has outlived its own parent.
                group.kill();
            }
            self.finished = true;
        }
        status
    }

    /// Stop the child AND everything it spawned: terminate the group, give
    /// it `grace` to exit, then kill.
    ///
    /// This replaces the `child.kill()` that each cancel path used to call.
    /// Killing the direct child alone is what let a cancelled `codex login`
    /// leave a live OAuth server behind — one that could still complete the
    /// login and overwrite the credentials the cancel had just restored.
    ///
    /// **`grace` is the CALLER's decision** ([`CANCEL_GRACE`] /
    /// [`INSTALL_GRACE`]), because what the wait buys differs per site: an
    /// installer should get to clean up a partial download, a cancel the
    /// user is watching should not wait at all. A child that ignores
    /// SIGTERM costs the full grace every single time, so a value chosen
    /// for the installer case is paid by every cancel too.
    pub async fn terminate(&mut self, grace: Duration) {
        if self.finished {
            return;
        }
        if let Some(group) = self.group.as_ref().map(|owner| owner.0) {
            // Blocking sleeps inside a grace loop don't belong on an async
            // worker; hand the escalation to a blocking thread. The Group
            // is Copy, so the owning Arc stays here and the container is
            // still alive for the duration.
            let _ = tokio::task::spawn_blocking(move || group.shut_down(grace)).await;
        }
        // Belt and braces: with no group (or a breakaway child) this is
        // the only thing that stops it, and it also reaps.
        let _ = self.child.kill().await;
        self.finished = true;
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            deregister(ticket);
        }
        if let Some(owner) = self.group.take() {
            if !self.finished {
                // KILL only, and deliberately not a TERM first: `Drop` can
                // run on an async worker so it must not block, and a
                // SIGTERM with an uncatchable SIGKILL immediately behind
                // it is a no-op — the handler never gets to run. Sending
                // both looked like it offered a graceful exit while
                // offering nothing. A caller that WANTS the graceful path
                // has `terminate(grace)`; reaching `Drop` means nobody
                // did, and this is the last chance to reach the group.
                owner.0.kill();
            }
            // Releasing the container is the Arc's job — the registry may
            // still hold a reference (see GroupOwner).
            drop(owner);
        }
    }
}

// ===================================================================
// 3. Detached children — the user's terminal
// ===================================================================

/// Launch a process Termory does NOT own: the user's terminal emulator.
///
/// Two rules, both deliberate and both the opposite of [`spawn_managed`]:
///
/// - **Never killed.** It is not registered for shutdown. The user asked
///   for a terminal to work in; quitting Termory must leave it running.
/// - **Still reaped.** The child is handed to the shared reaper so it
///   can't sit in the process table as a zombie. This is the leak that
///   used to happen on every "Resume in terminal" / "New session" click.
///
/// It also gets its own process group, so a signal sent to Termory's group
/// (a Ctrl-C in the shell that launched a dev build) doesn't reach the
/// user's terminal.
///
/// Returns the child's pid. Callers ignore it — it exists so a test can
/// assert this pid is absent from the shutdown registry.
pub fn spawn_detached(mut cmd: Command) -> io::Result<u32> {
    Group::prepare(&mut cmd);
    let child = cmd.spawn()?;
    let pid = child.id();
    // A send failure means the reaper thread never started; dropping the
    // child then leaves a zombie, which is what we had before this module,
    // so there is nothing better to fall back to.
    if let Err(err) = reaper().send(child) {
        log::error!("process: couldn't hand a detached child to the reaper: {err}");
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direct child is reaped and the output comes back intact.
    #[cfg(unix)]
    #[test]
    fn probe_returns_output_and_reaps() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo hello"]);
        let out = probe(cmd, Duration::from_secs(5)).expect("probe output");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    /// A non-zero exit is still `Some` — the Keychain callers read the
    /// status code to tell "no entry" from "locked".
    #[cfg(unix)]
    #[test]
    fn probe_keeps_a_failing_exit_status() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 44"]);
        let out = probe(cmd, Duration::from_secs(5)).expect("probe output");
        assert_eq!(out.status.code(), Some(44));
    }

    /// Output bigger than a pipe buffer must come back whole.
    ///
    /// A pipe holds ~64 KB and then BLOCKS the writer. While the probe
    /// only read after its wait loop, a child producing more than that
    /// deadlocked against its own output: it could never exit, so
    /// `try_wait` never reported done, and the probe's only way out was
    /// the timeout — returning `None` for a command that had in fact
    /// worked. 200 KB is comfortably past the buffer on both Linux and
    /// macOS.
    #[cfg(unix)]
    #[test]
    fn probe_reads_output_larger_than_the_pipe_buffer() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "yes 0123456789 | head -c 200000"]);

        let started = Instant::now();
        let out = probe(cmd, Duration::from_secs(20)).expect("probe should not time out");

        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 200_000, "output was truncated");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "probe stalled on a full pipe ({:?})",
            started.elapsed()
        );
    }

    /// stderr is drained too — same deadlock, other pipe. Nothing reads a
    /// probe's stderr until it returns, so a chatty rc file (the shell
    /// fallback sources the user's `.zshrc`) could wedge it the same way.
    #[cfg(unix)]
    #[test]
    fn probe_reads_large_stderr_without_stalling() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "yes 0123456789 | head -c 200000 >&2"]);

        let out = probe(cmd, Duration::from_secs(20)).expect("probe should not time out");

        assert!(out.status.success());
        assert_eq!(out.stderr.len(), 200_000, "stderr was truncated");
    }

    /// A fast child must not pay a full polling interval.
    ///
    /// Probes are a HOT path — the tray re-probes every CLI on each menu
    /// open — and a `--version` exits in about a millisecond. With the
    /// flat 50ms sleep this replaced, every probe cost 50ms regardless
    /// (measured 55ms/call against 1.2ms for a bare `Command::output`);
    /// the backoff brings it to ~3ms. Averaged over several runs so one
    /// scheduling hiccup can't flake it.
    #[cfg(unix)]
    #[test]
    fn a_fast_probe_does_not_pay_the_full_poll_interval() {
        const RUNS: u32 = 5;
        let started = Instant::now();
        for _ in 0..RUNS {
            let mut cmd = Command::new("/bin/echo");
            cmd.arg("x");
            probe(cmd, Duration::from_secs(5)).expect("probe");
        }
        let per_call = started.elapsed() / RUNS;
        assert!(
            per_call < POLL_MAX,
            "a fast probe took {per_call:?}, i.e. it is waiting out the \
             poll interval ({POLL_MAX:?}) instead of backing off from \
             {POLL_MIN:?}"
        );
    }

    /// stdin is delivered, which is what `security -i` needs.
    #[cfg(unix)]
    #[test]
    fn probe_writes_stdin() {
        let mut cmd = Command::new("/bin/cat");
        cmd.arg("-");
        let out =
            probe_with_stdin(cmd, Some(b"payload"), Duration::from_secs(5)).expect("probe output");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "payload");
    }

    /// A child that outlives a grandchild, with the grandchild's fate made
    /// observable by two marker files.
    ///
    /// **The `started` marker is what makes these tests real.** The first
    /// version of them killed the child immediately after spawning it and
    /// passed even with the group kill removed — `spawn` → `terminate`
    /// outran `sh`'s own startup, so there was never a grandchild to
    /// survive, and the test was measuring a race rather than the rule.
    /// Waiting for `started` proves the grandchild is running BEFORE
    /// anything is killed; `survived` then appears a second later, and
    /// only if the kill missed it.
    /// How long the grandchild waits before writing its `survived` marker.
    ///
    /// Every kill in these tests happens well inside this window, so a
    /// pass never depends on winning a race with it — the probe test WAS
    /// flaky when this and the probe's own timeout were both one second.
    #[cfg(unix)]
    const GRANDCHILD_DELAY: Duration = Duration::from_secs(2);

    #[cfg(unix)]
    struct GrandchildProbe {
        dir: std::path::PathBuf,
        started: std::path::PathBuf,
        survived: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl GrandchildProbe {
        fn new(tag: &str) -> Self {
            // A plain counter, NOT `ThreadId`'s Debug form: that renders
            // as `ThreadId(5)`, and the parens land in a path that gets
            // interpolated into a shell command, where they are syntax.
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "termory-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("tmp dir");
            Self {
                started: dir.join("started"),
                survived: dir.join("survived"),
                dir,
            }
        }

        /// The direct child backgrounds a grandchild and then sleeps well
        /// past the end of the test, so it is always the KILL that ends
        /// the tree rather than the command finishing.
        ///
        /// The grandchild's own delay is [`GRANDCHILD_DELAY`] — long
        /// enough that a kill and the `survived` write can never race.
        fn script(&self) -> String {
            format!(
                "sh -c \"touch {}; sleep {}; touch {}\" & sleep 30",
                self.started.to_string_lossy(),
                GRANDCHILD_DELAY.as_secs(),
                self.survived.to_string_lossy()
            )
        }

        /// A child that backgrounds a grandchild and then exits NORMALLY,
        /// straight away — no trailing `sleep`, so nothing is ever killed
        /// and `wait()` returns a clean success.
        ///
        /// The grandchild is re-parented to init but STAYS in the process
        /// group (re-parenting changes the parent, not the group), which
        /// is what makes it reachable by `killpg` afterwards.
        fn exit_script(&self) -> String {
            format!(
                "sh -c \"touch {}; sleep {}; touch {}\" &",
                self.started.to_string_lossy(),
                GRANDCHILD_DELAY.as_secs(),
                self.survived.to_string_lossy()
            )
        }

        /// A child that IGNORES SIGTERM, for the escalation test.
        ///
        /// The trap is installed BEFORE the announcement, so waiting for
        /// `started` proves the trap is in place. Without that ordering
        /// the test kills a shell that still has default handlers, the
        /// polite signal works, and the escalation is never exercised —
        /// the same startup race the grandchild scripts guard against.
        fn trap_script(&self) -> String {
            format!(
                "trap '' TERM; touch {}; while true; do sleep 0.1; done",
                self.started.to_string_lossy()
            )
        }

        /// Block until the child is definitely running.
        fn wait_started(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self.started.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("grandchild never started — the test proves nothing");
        }

        /// Wait past the grandchild's own delay, then report whether it
        /// was still alive to write its marker.
        fn grandchild_survived(&self) -> bool {
            std::thread::sleep(GRANDCHILD_DELAY + Duration::from_millis(600));
            self.survived.exists()
        }
    }

    #[cfg(unix)]
    impl Drop for GrandchildProbe {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// **The grandchild rule.** A shell that backgrounds a command and then
    /// blocks leaves that command running; killing the direct child does
    /// not touch it. A timed-out probe must take the whole group down.
    #[cfg(unix)]
    #[test]
    fn probe_timeout_kills_the_whole_group() {
        let probe_dir = GrandchildProbe::new("probe");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.script()]);

        // Spawn on a helper thread so the grandchild can be confirmed
        // running while the probe is still inside its wait loop.
        // Times out well before the grandchild would write `survived`.
        let script_done = std::thread::spawn(move || {
            let started = Instant::now();
            let out = probe(cmd, Duration::from_millis(400));
            (out.is_none(), started.elapsed())
        });
        probe_dir.wait_started();

        let (timed_out, elapsed) = script_done.join().expect("probe thread");
        assert!(timed_out, "a timed-out probe returns None");
        assert!(
            elapsed < Duration::from_secs(10),
            "the probe must not sit out the child's full sleep"
        );
        assert!(
            !probe_dir.grandchild_survived(),
            "grandchild survived the group kill"
        );
    }

    /// `terminate` must reach a grandchild too — the codex-login case,
    /// where the process holding the OAuth port is not the one we spawned.
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_terminate_kills_the_whole_group() {
        let _hold = hold_children();
        let probe_dir = GrandchildProbe::new("managed");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.script()]);

        let mut managed = spawn_managed(cmd).expect("spawn");
        // Kill only once the grandchild is real (see GrandchildProbe).
        probe_dir.wait_started();
        managed.terminate(INSTALL_GRACE).await;

        assert!(
            !probe_dir.grandchild_survived(),
            "grandchild survived terminate()"
        );
    }

    /// Drop is the path an early `return` in a login flow takes, and it
    /// must reach the grandchild as well — otherwise a cancelled login
    /// leaves the OAuth server that outlived its wrapper.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_managed_child_kills_the_whole_group() {
        let _hold = hold_children();
        let probe_dir = GrandchildProbe::new("dropped");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.script()]);

        let managed = spawn_managed(cmd).expect("spawn");
        probe_dir.wait_started();
        drop(managed);

        assert!(
            !probe_dir.grandchild_survived(),
            "grandchild survived the drop"
        );
    }

    /// A child that IGNORES SIGTERM must still die.
    ///
    /// `terminate` asks politely first, which is the right default (an
    /// installer gets to clean up its partial download), but the polite
    /// signal is catchable — so the escalation to SIGKILL after
    /// [`TERM_GRACE`] is what makes the guarantee hold. Without it a
    /// cancelled login could hang on a CLI that traps the signal.
    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_escalates_to_kill_when_term_is_ignored() {
        let _hold = hold_children();
        let probe_dir = GrandchildProbe::new("trap");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.trap_script()]);
        let mut managed = spawn_managed(cmd).expect("spawn");
        let pid = managed.child.id().expect("pid") as libc::pid_t;
        // Only meaningful once the trap is actually installed.
        probe_dir.wait_started();

        let started = Instant::now();
        managed.terminate(INSTALL_GRACE).await;
        let elapsed = started.elapsed();

        // SAFETY: signal 0 only probes for existence.
        assert!(
            unsafe { libc::kill(pid, 0) } != 0,
            "a SIGTERM-ignoring child {pid} survived terminate()"
        );
        assert!(
            elapsed >= INSTALL_GRACE,
            "the grace period was skipped — the child never got its \
             chance to exit cleanly ({elapsed:?})"
        );
    }

    /// **A NORMAL exit must sweep the group too.**
    ///
    /// The other three kill paths (`terminate`, `Drop`, `shutdown_all`)
    /// all had grandchild coverage; this one did not, and that is exactly
    /// where the gap was. "The wrapper exited" says nothing about the
    /// process doing the work — the measured codex case was a wrapper
    /// KILLED with its grandchild surviving, but a wrapper that exits
    /// CLEANLY leaves one just the same, and `Drop` skips the group once
    /// `wait()` has set `finished`. So the sweep has to happen in
    /// `wait()` itself, at the moment the child is reaped.
    ///
    /// Nothing is killed here: the child exits on its own and `wait()`
    /// reports success, so a passing assertion can only come from the
    /// sweep.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_normal_exit_still_sweeps_the_group() {
        let _hold = hold_children();
        let probe_dir = GrandchildProbe::new("normalexit");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.exit_script()]);

        let mut managed = spawn_managed(cmd).expect("spawn");
        probe_dir.wait_started();

        let status = managed.wait().await.expect("wait");
        assert!(
            status.success(),
            "the child should have exited by itself, not been killed: {status:?}"
        );
        assert!(
            !probe_dir.grandchild_survived(),
            "grandchild outlived a cleanly-exiting parent"
        );
    }

    /// A user-initiated cancel must not wait out the INSTALLER's budget.
    ///
    /// The stubborn-child cost is paid in full on every cancel, so a
    /// single blanket grace is felt directly as "closing the subprocess
    /// takes ages" — reported from real use, measured at 2.05s per cancel
    /// and up to 4s for a codex login (2s for the HTTP `/cancel`, then the
    /// grace again). Same child as
    /// `terminate_escalates_to_kill_when_term_is_ignored`, so the only
    /// variable is which grace the call site passes.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancel_kills_a_stubborn_child_without_the_install_grace() {
        let _hold = hold_children();
        let probe_dir = GrandchildProbe::new("cancelgrace");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.trap_script()]);
        let mut managed = spawn_managed(cmd).expect("spawn");
        let pid = managed.child.id().expect("pid") as libc::pid_t;
        probe_dir.wait_started();

        let started = Instant::now();
        managed.terminate(CANCEL_GRACE).await;
        let elapsed = started.elapsed();

        // SAFETY: signal 0 only probes for existence.
        assert!(
            unsafe { libc::kill(pid, 0) } != 0,
            "the stubborn child {pid} survived the cancel"
        );
        assert!(
            elapsed < INSTALL_GRACE,
            "a cancel waited out the installer's grace ({elapsed:?}) — the \
             whole point of splitting the two constants"
        );
    }

    /// The app-exit sweep must take managed children down — including
    /// their grandchildren — while leaving nothing registered behind.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_all_kills_managed_children() {
        let _sweep = SWEEP_LOCK.write().unwrap_or_else(|e| e.into_inner());
        let probe_dir = GrandchildProbe::new("shutdown");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", &probe_dir.script()]);

        // The handle is deliberately KEPT alive across the assertions: if
        // it were dropped first, `Drop` would do the killing and a broken
        // sweep would still look fine.
        let managed = spawn_managed(cmd).expect("spawn");
        let ticket = managed.ticket.expect("registered");
        probe_dir.wait_started();

        shutdown_all();

        assert!(
            !registry_holds(|t, _| t == ticket),
            "shutdown_all must empty the registry"
        );
        assert!(
            !probe_dir.grandchild_survived(),
            "grandchild survived shutdown_all()"
        );
        drop(managed);
    }

    /// A managed child is registered while it runs and gone once it is
    /// dropped — otherwise the shutdown sweep would accumulate stale
    /// groups and signal pids that had been reused.
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_registers_and_deregisters() {
        let _hold = hold_children();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let mut managed = spawn_managed(cmd).expect("spawn");
        let ticket = managed.ticket.expect("registered");
        assert!(
            registry_holds(|t, _| t == ticket),
            "a live managed child must be registered for shutdown"
        );
        managed.terminate(INSTALL_GRACE).await;
        drop(managed);
        assert!(
            !registry_holds(|t, _| t == ticket),
            "a finished managed child must not stay in the registry"
        );
    }

    /// Dropping without terminating still stops the child — an early
    /// return in a login flow must not leave the CLI running.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_managed_child_stops_it() {
        let _hold = hold_children();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let managed = spawn_managed(cmd).expect("spawn");
        let pid = managed.child.id().expect("pid") as libc::pid_t;
        drop(managed);

        // kill_on_drop reaps asynchronously; give it a moment.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // SAFETY: signal 0 only probes for existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "dropped managed child {pid} is still running");
    }

    /// A detached child is NOT registered — quitting Termory must never
    /// close the terminal the user is working in.
    /// Quitting Termory must never close the terminal the user is working
    /// in, so a detached child is absent from the shutdown registry. On
    /// unix a group id IS the leader's pid, so the pid identifies it.
    #[cfg(unix)]
    #[test]
    fn detached_children_are_not_registered_for_shutdown() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 1"]);
        let pid = spawn_detached(cmd).expect("spawn");
        assert!(
            !registry_holds(|_, group| group == pid as isize),
            "detached child {pid} must stay out of the shutdown registry"
        );
    }

    /// The zombie fix: a detached child that has exited must be REAPED,
    /// not left in the process table.
    ///
    /// The assertion is that the pid disappears. A zombie is still a live
    /// entry — `kill(pid, 0)` succeeds for one — so "the process exited"
    /// and "the process was reaped" are only distinguishable this way, and
    /// the bug being fixed here produced exactly the first without the
    /// second. Checked by pid rather than by counting the suite's zombies,
    /// which parallel tests would make flaky.
    #[cfg(unix)]
    #[test]
    fn detached_children_are_reaped() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        let pid = spawn_detached(cmd).expect("spawn") as libc::pid_t;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return; // gone from the table — reaped
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("detached child {pid} was never reaped");
    }
}

//! Real failure isolation for `lullaby test`: run the suite in a child process
//! and survive a test that kills it.
//!
//! # Why a subprocess
//!
//! The interpreter returns every *runtime* error as an ordinary `RuntimeError`,
//! so the runner already contains those in-process. Two shapes are not runtime
//! errors and escape that `Result` entirely:
//!
//! * a **stack overflow** faults on the guard page and the process dies — Rust
//!   cannot unwind or catch it (this is exactly why libtest can `catch_unwind` a
//!   panic and we cannot); and
//! * a **non-terminating** test never returns at all, so there is nothing to
//!   catch.
//!
//! Neither can be contained inside the process running the test. The only
//! mechanism that contains both is an OS process boundary plus a deadline, so
//! that is what this module implements.
//!
//! # Batch-with-resume, not process-per-test
//!
//! A process per test would be the obvious shape, but it pays a process spawn
//! (~13 ms on Windows) *and* a full recompile of the program for every test —
//! ~16 ms each, so ~1.6 s on a 100-test suite against ~20 ms in-process. That is
//! a tax on the common path, where every test passes and nothing needs isolating.
//!
//! So the child runs the *whole remaining suite* sequentially in one process, and
//! the parent spawns again only when a test actually takes the child down: on a
//! crash or a timeout the parent records that test's failure and resumes the
//! batch at the next index. An all-passing suite therefore costs exactly one
//! spawn and one compile no matter how many tests it has — the same work the old
//! in-process runner did — and it keeps that runner's exact semantics, since all
//! tests still run sequentially in one process. Only a suite that actually kills
//! the runner pays per incident, which is the right place to pay.
//!
//! # The transport: a private pipe, not stdout/stderr
//!
//! The child reports results over a **dedicated pipe no test can reach**, and this
//! is a correctness property rather than a nicety.
//!
//! An earlier design put the protocol on the child's stderr and authenticated it
//! with a per-run nonce. That was **unsound**. `warn()` writes straight to process
//! stderr, and the nonce travelled in `argv` — which any process may read of
//! itself (`/proc/self/cmdline`, or `Get-CimInstance Win32_Process` through the
//! `sys_output` builtin). Secrecy the OS hands to the attacker is not secrecy, so
//! a test could forge a result line: a fake `pass` invented a phantom PASS for a
//! failing test, and a fake `done` truncated the run into a green
//! `0 passed, 0 failed`.
//!
//! Moving the protocol to a private pipe in the child's **stdin slot** was the
//! next attempt, and it was *also* broken — by a different OS route. `proc_spawn`
//! spawns with stdin **inherited**, so a grandchild received a writable handle to
//! the protocol channel and `cmd /c echo pass 1 >&0` forged a line. The audit that
//! missed it checked that no builtin *names* a descriptor (true: zero
//! `from_raw_fd`/`as_raw_handle` in the runtime) and concluded the channel was
//! unreachable. It only ruled out one route. **Process inheritance hands the
//! descriptor over with no builtin involved.**
//!
//! Note what the forger did to the `done` validation: it reported *every* index
//! first, satisfying `last_reported + 1 == names.len()`, after which `done`
//! completed the batch legitimately. Validating a verb against state the attacker
//! also controls is not validation.
//!
//! So the child now **destroys the slot's identity** before running anything (see
//! [`take_protocol_channel`]): it reclaims the descriptor, duplicates it onto one
//! that cannot be inherited (no-inherit `DuplicateHandle` / `F_DUPFD_CLOEXEC`),
//! closes the inherited original, and reopens the stdin slot onto the null device.
//! The channel then has **no name a program can reach and no slot a child can
//! inherit**, which holds against spawn routes nobody has thought of yet — rather
//! than depending on every present and future builtin remembering to null its
//! stdin.
//!
//! The child's stdout and stderr stay plain `inherit`, so a test's
//! `print`/`println`/`warn` go straight to the terminal, untouched and never
//! parsed by us.
//!
//! Reclaiming the stdin slot costs nothing: the child leaves the null device
//! there, so a test's `read_line` gets a clean EOF — exactly what it got when the
//! runner passed a null stdin.
//!
//! # Completion and ordering
//!
//! Completion is reported by an explicit `done`, not inferred from EOF: a test may
//! leave a grandchild holding the inherited pipe, and EOF would then be deferred
//! for as long as that process lives, stalling even a wholly passing suite. `done`
//! is still validated against what was actually reported — no verb may declare the
//! batch finished on its own say-so.
//!
//! Results are printed as they stream in, never collected and dumped at the end:
//! a batch only ever resumes *past* the test that killed it, so results arrive in
//! index order and printing them on arrival preserves both the live progress of
//! the old runner and its ordering. The child flushes stdout before each result
//! line and the parent flushes its own stdout before each spawn, so a test's
//! output always lands before the `PASS`/`FAIL` line reporting it.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use lullaby_runtime::run_named_function;

use crate::args::OutputMode;
use crate::compile::{SourceMode, compile};
use crate::diagnostics::format_reports;

use super::test::discover_tests;

/// The hidden internal subcommand the parent re-invokes itself with. Positional:
/// `<path> <start-index> <verbose:0|1> [filter]`.
pub(crate) const RUN_BATCH_COMMAND: &str = "__run-test-batch";

/// Protocol verbs, emitted by the child as `<verb> <index> [payload]` on the
/// private channel. There is no nonce and none is needed — see the module docs:
/// the channel is unreachable from a Lullaby program, so this is not a secret
/// being kept, it is a pipe an attacker has no handle to.
const START: &str = "start";
const PASS: &str = "pass";
const FAIL: &str = "fail";
/// The child finished the batch under its own power. This completes a batch —
/// NOT pipe EOF, which a grandchild holding the inherited handle can defer
/// indefinitely. Still validated against `last_reported`.
const DONE: &str = "done";

/// Separates the failure message from each traceback frame inside one `fail`
/// payload. A control character, so it cannot occur in a rendered diagnostic.
const FIELD_SEPARATOR: char = '\u{1f}';

/// Windows `STD_INPUT_HANDLE`, and the one call needed to repoint it.
#[cfg(windows)]
const STD_INPUT_HANDLE: u32 = -10i32 as u32;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetStdHandle(std_handle: u32, handle: *mut core::ffi::c_void) -> i32;
}

// The POSIX counterpart of `SetStdHandle`. Declared directly rather than pulling
// in a dependency: `dup2` lives in libc, which every unix Rust target already
// links.
#[cfg(unix)]
unsafe extern "C" {
    fn dup2(oldfd: core::ffi::c_int, newfd: core::ffi::c_int) -> core::ffi::c_int;
}

/// Take the protocol descriptor **out** of the stdin slot and leave the null
/// device in its place, before any test code runs.
///
/// Reclaiming alone is not enough, and this is the third time that lesson has
/// been learned the hard way. The parent hands the pipe's write end over in the
/// stdin slot; `proc_spawn` spawns with **stdin inherited**, so a grandchild
/// receives a writable handle to the protocol channel and `>&0` injects a forged
/// line — a green run for a suite whose tests fail. No builtin ever *names* a
/// descriptor (grep confirms zero `from_raw_fd`/`as_raw_handle` in the runtime),
/// but that only rules out one route: **the OS hands the descriptor over through
/// process inheritance**, no builtin required.
///
/// So the slot's identity is destroyed rather than merely read:
///
/// 1. reclaim the descriptor the parent put in the stdin slot;
/// 2. duplicate it onto a descriptor of our own that no child can receive —
///    Windows `try_clone` is a `DuplicateHandle` with `bInheritHandle = FALSE`;
///    POSIX `try_clone` is `fcntl(F_DUPFD_CLOEXEC, 3)`, close-on-exec and never
///    in a std slot; then
/// 3. put the null device in the stdin slot, which also disposes of the original:
///    `SetStdHandle(STD_INPUT_HANDLE, NUL)` on Windows, `dup2(nul, 0)` on POSIX.
///
/// After this the protocol lives on a descriptor with **no name a program can
/// reach and no standard slot a child can inherit**, so the property holds
/// against spawn routes nobody has thought of yet — including any future builtin
/// that spawns with inherited stdin. Fixing `proc_spawn` to null its stdin would
/// work today and silently re-break on the next such builtin (and is outside this
/// crate anyway).
///
/// On BOTH platforms a grandchild now inherits the null device as its stdin,
/// however it is spawned. Getting that right on POSIX needs `dup2` specifically,
/// not a close-then-reopen: `File::open` always sets `O_CLOEXEC` and `Command`
/// does no `dup2` for an inherited stdin, so a reopened fd 0 would be *closed* in
/// the grandchild rather than `/dev/null` — still safe (`>&0` is `EBADF`) but a
/// footgun, since the grandchild's next `open()` would land on descriptor 0.
///
/// It also makes the "costs nothing" claim actually true: a test's `read_line`
/// reads the null device and gets a clean EOF, exactly as it did when the runner
/// gave the child a null stdin.
#[cfg(windows)]
fn take_protocol_channel() -> Option<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    let raw = std::io::stdin().as_raw_handle();
    if raw.is_null() {
        return None;
    }
    // Own it, so dropping it below actually closes the inheritable handle the
    // parent passed us.
    let inherited = unsafe { File::from_raw_handle(raw) };
    // `try_clone` -> `DuplicateHandle` with inheritance OFF: this duplicate cannot
    // be passed to any child, however it is spawned.
    let private = inherited.try_clone().ok()?;
    drop(inherited);

    // Point the slot at the null device. `Stdio::Inherit` reads the slot at spawn
    // time, so every future grandchild inherits NUL instead of our pipe.
    let nul = File::open("NUL").ok()?;
    if unsafe { SetStdHandle(STD_INPUT_HANDLE, nul.as_raw_handle()) } == 0 {
        return None;
    }
    // The slot owns this handle for the life of the process now.
    std::mem::forget(nul);
    Some(private)
}

#[cfg(unix)]
fn take_protocol_channel() -> Option<File> {
    use std::mem::ManuallyDrop;
    use std::os::fd::{AsRawFd, FromRawFd};

    // Borrow fd 0 rather than owning it: `dup2` below closes it for us, and an
    // owning `File` would then close the null device we just installed there.
    let inherited = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    // `try_clone` -> `fcntl(F_DUPFD_CLOEXEC, 3)`: close-on-exec, so a grandchild
    // (fork+exec) never inherits the duplicate, and the minimum of 3 means it can
    // never land in a std slot. Done before the `dup2` below, so fd 0 is free to
    // overwrite.
    let private = inherited.try_clone().ok()?;

    // Replace fd 0 with the null device ATOMICALLY. `dup2` closes the pipe
    // currently at fd 0 and installs the new descriptor in one step — no window in
    // which fd 0 is unallocated — and, critically, the descriptor it creates does
    // NOT carry `O_CLOEXEC`. That matters: `File::open` always sets `O_CLOEXEC`,
    // and `Command`'s `Stdio::Inherit` performs no `dup2` for stdin, so a
    // close-then-reopen would leave a grandchild with fd 0 CLOSED rather than
    // `/dev/null` — safe (`>&0` is EBADF) but a classic footgun, since the
    // grandchild's next `open()` would land on descriptor 0.
    let nul = File::open("/dev/null").ok()?;
    if unsafe { dup2(nul.as_raw_fd(), 0) } != 0 {
        return None;
    }
    // fd 0 owns its own copy now, so this only closes the temporary.
    drop(nul);
    Some(private)
}

#[cfg(not(any(windows, unix)))]
fn take_protocol_channel() -> Option<File> {
    None
}

/// Kill the child AND every process it spawned.
///
/// Killing only the child is not enough: `sys_status`/`sys_output`/`proc_spawn`
/// let a `test_*` function spawn arbitrary processes, and a grandchild both
/// outlives a killed child and inherits the child's pipe handles — so the pipe
/// never reaches EOF and the runaway process keeps running. A runner whose
/// `--timeout N` does not actually bound the run within ~N seconds fails the very
/// guarantee it exists to provide.
///
/// Best-effort by design: this runs only on the timeout path, where we are
/// forcibly stopping a runaway test. A suite that *passes* keeps whatever it
/// spawned — `proc_spawn` is a documented builtin for background processes, and
/// reaping a passing test's children would break legitimate use.
#[cfg(windows)]
fn kill_process_tree(child: &mut Child) {
    // `/T` kills the whole tree rooted at the child; it must run BEFORE the child
    // itself dies, or the parent/child links the tree walk needs are gone.
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID"])
        .arg(child.id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The POSIX counterpart: the child leads its own process group (see
/// `process_group(0)` at spawn), so signalling the negated group id reaches every
/// descendant that has not deliberately left the group.
#[cfg(unix)]
fn kill_process_tree(child: &mut Child) {
    let _ = Command::new("kill")
        .arg("--")
        .arg(format!("-{}", child.id()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(windows, unix)))]
fn kill_process_tree(_child: &mut Child) {}

/// Windows `STATUS_STACK_OVERFLOW`, the one abnormal exit we can name exactly.
#[cfg(windows)]
const WINDOWS_STACK_OVERFLOW: i32 = 0xC000_00FDu32 as i32;

/// Describe why a child died, without guessing. A stack overflow is the common
/// cause but not the only one — an external `taskkill`/`SIGTERM` lands here too,
/// and reporting that as "most likely a stack overflow" would be a fabrication.
fn describe_abnormal_exit(status: Option<ExitStatus>) -> String {
    #[cfg(windows)]
    if let Some(status) = status
        && status.code() == Some(WINDOWS_STACK_OVERFLOW)
    {
        return "the test process terminated abnormally: stack overflow \
                (STATUS_STACK_OVERFLOW), i.e. unbounded recursion"
            .to_string();
    }
    match status {
        Some(status) => format!("the test process terminated abnormally ({status})"),
        None => "the test process terminated abnormally (cause unknown)".to_string(),
    }
}

/// How many tests passed and failed so far.
#[derive(Default)]
pub(crate) struct Tally {
    pub(crate) passed: usize,
    pub(crate) failed: usize,
}

/// Escape a message so it survives as one protocol line.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Print one test's result, in the same shape the in-process runner used.
fn report_pass(name: &str, tally: &mut Tally) {
    tally.passed += 1;
    println!("PASS {name}");
}

fn report_fail(name: &str, message: &str, frames: &[String], tally: &mut Tally) {
    tally.failed += 1;
    println!("FAIL {name}: {message}");
    for frame in frames {
        println!("{frame}");
    }
}

/// Run every discovered test under isolation, printing `PASS`/`FAIL` per test as
/// results arrive, and return the tally.
///
/// `timeout` of `None` disables the per-test deadline.
pub(crate) fn run_isolated(
    path: &Path,
    names: &[String],
    filter: Option<&str>,
    verbose: bool,
    timeout: Option<Duration>,
) -> Result<Tally, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate the lullaby executable: {error}"))?;
    let mut tally = Tally::default();
    let mut next = 0usize;

    while next < names.len() {
        let resume = run_batch(
            &exe, path, next, filter, verbose, timeout, names, &mut tally,
        )?;
        // A batch must always make progress, or this would spin forever on a child
        // that dies before reporting anything. If it made none, attribute the
        // failure to the test we tried to resume at and step past it.
        if resume > next {
            next = resume;
        } else {
            report_fail(
                &names[next],
                "the isolated test process exited before reporting this test",
                &[],
                &mut tally,
            );
            next += 1;
        }
    }

    Ok(tally)
}

/// Spawn one child covering `names[start..]` and consume its protocol stream
/// until it finishes, crashes, or trips the per-test deadline. Returns the index
/// to resume at (`names.len()` when the batch completed the suite).
#[allow(clippy::too_many_arguments)]
fn run_batch(
    exe: &Path,
    path: &Path,
    start: usize,
    filter: Option<&str>,
    verbose: bool,
    timeout: Option<Duration>,
    names: &[String],
    tally: &mut Tally,
) -> Result<usize, String> {
    // The private protocol channel. Its write end becomes the child's stdin slot;
    // a Lullaby program can reach neither (no builtin writes to stdin, and none
    // writes to a raw descriptor), so nothing a test emits can be mistaken for a
    // protocol line.
    let (protocol, protocol_write) = std::io::pipe()
        .map_err(|error| format!("could not create the test protocol pipe: {error}"))?;

    let mut command = Command::new(exe);
    command
        .arg(RUN_BATCH_COMMAND)
        .arg(path)
        .arg(start.to_string())
        .arg(if verbose { "1" } else { "0" });
    if let Some(filter) = filter {
        command.arg(filter);
    }
    // POSIX: give the child its own process group so `kill_process_tree` can
    // signal every descendant by group id.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Order our own prints against the child's inherited stdout.
    let _ = std::io::stdout().flush();
    // stdout/stderr are inherited, so a test's own output reaches the terminal
    // exactly as it did in-process and is never parsed by us.
    let mut child = command
        .stdin(Stdio::from(protocol_write))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not start the isolated test process: {error}"))?;
    // The parent must not keep the pipe's write end alive, or it would never see
    // EOF and could not detect a crash.
    drop(command);

    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(protocol).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut in_flight: Option<usize> = None;
    // The highest index this batch actually reported. This makes "the child
    // finished" distinguishable from "the child died having reported nothing",
    // and is what `done` is validated against.
    let mut last_reported: Option<usize> = None;
    let mut deadline = timeout.map(|limit| Instant::now() + limit);

    // Where to resume when the child stops without finishing: after the last test
    // we actually saw reported. NEVER `names.len()` on a guess — that would make a
    // child which died having reported nothing indistinguishable from one that ran
    // everything, silently yielding `0 passed, 0 failed` + exit 0: a green run that
    // executed no tests. `run_isolated`'s progress guard turns the report-nothing
    // case into a loud failure.
    let resume_after_reported =
        |last_reported: Option<usize>| last_reported.map_or(start, |index| index + 1);

    let resume = loop {
        let event = match deadline {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match event {
            Ok(line) => {
                let (verb, payload) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                if verb == DONE {
                    // The child may only claim the batch is complete if it actually
                    // reported every test in it: `done` must never be the one verb
                    // that can end a run unconditionally. This is an integrity check
                    // against a confused child, NOT a security control — a forger
                    // able to write to this channel would simply report every index
                    // first and satisfy it. Only the channel's unreachability makes
                    // the protocol trustworthy.
                    break match last_reported {
                        Some(index) if index + 1 == names.len() => names.len(),
                        other => resume_after_reported(other),
                    };
                }
                let (index_text, detail) = payload.split_once(' ').unwrap_or((payload, ""));
                let index: usize = match index_text.parse() {
                    Ok(index) if index < names.len() => index,
                    _ => continue,
                };
                match verb {
                    START => {
                        in_flight = Some(index);
                        deadline = timeout.map(|limit| Instant::now() + limit);
                    }
                    PASS => {
                        report_pass(&names[index], tally);
                        in_flight = None;
                        last_reported = Some(index);
                        deadline = timeout.map(|limit| Instant::now() + limit);
                    }
                    FAIL => {
                        // One line carries the whole failure: message, then a
                        // traceback frame per field. Collecting it in a single
                        // event is what lets us print on arrival.
                        let mut fields = detail.split(FIELD_SEPARATOR);
                        let message = unescape(fields.next().unwrap_or_default());
                        let frames: Vec<String> = fields.map(unescape).collect();
                        report_fail(&names[index], &message, &frames, tally);
                        in_flight = None;
                        last_reported = Some(index);
                        deadline = timeout.map(|limit| Instant::now() + limit);
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Before declaring a timeout, check whether the child has already
                // died: a crash's EOF can lag the fault (Windows tears a stack
                // overflow down in ~4s), and a deadline shorter than that would
                // otherwise report a test that DID terminate as one that did not.
                // This narrows that window but cannot close it — see the caveat in
                // `language_surface.md`.
                if let Some(status) = child.try_wait().ok().flatten() {
                    break match in_flight {
                        Some(index) => {
                            report_fail(
                                &names[index],
                                &describe_abnormal_exit(Some(status)),
                                &[],
                                tally,
                            );
                            index + 1
                        }
                        None => resume_after_reported(last_reported),
                    };
                }
                // The in-flight test outlived the deadline: it is not going to
                // finish. Kill the whole TREE -- not just the child -- bank the
                // timeout as that test's failure, and resume the batch after it.
                kill_process_tree(&mut child);
                let _ = child.kill();
                let _ = child.wait();
                let limit = timeout.unwrap_or_default().as_secs();
                break match in_flight {
                    Some(index) => {
                        report_fail(
                            &names[index],
                            &format!(
                                "timed out after {limit}s (did not terminate); the test process was \
                                 killed — raise the limit with `--timeout <seconds>` if the test is \
                                 merely slow"
                            ),
                            &[],
                            tally,
                        );
                        index + 1
                    }
                    // No test in flight: the child stalled between tests.
                    None => resume_after_reported(last_reported),
                };
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The pipe closed without a `done`: the child is gone.
                let status = child.wait().ok();
                break match in_flight {
                    // A test was running and the process died under it: a stack
                    // overflow or another abnormal termination. The exit code is
                    // platform-specific (Windows STATUS_STACK_OVERFLOW 0xC00000FD,
                    // POSIX 127 or a signal), so report the *observable* — abnormal
                    // termination — and never pin a value.
                    Some(index) => {
                        report_fail(&names[index], &describe_abnormal_exit(status), &[], tally);
                        index + 1
                    }
                    None => resume_after_reported(last_reported),
                };
            }
        }
    };

    // Deliberately NOT joined. The reader blocks until the pipe reaches EOF, and a
    // grandchild that inherited the handle defers EOF for as long as it lives —
    // joining here is what let a test's spawned process outlast the deadline and
    // unbound the whole run (`--timeout 3` taking 14s). Tree-killing above normally
    // EOFs the pipe promptly; detaching guarantees we proceed within the deadline
    // even if it does not. The thread is `'static`, owns its pipe, and exits on EOF
    // or when its send fails after `rx` drops — so it is bounded by the
    // grandchild's lifetime, never by ours.
    drop(reader);
    let _ = child.wait();
    Ok(resume)
}

// ---------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------

/// The `__run-test-batch` entry point: run `names[start..]` in THIS process,
/// reporting each test's progress and result on the private protocol channel.
/// Discovery is re-derived here rather than passed in, because it is
/// deterministic — the parent and the child agree on the list and therefore on
/// every index.
pub(crate) fn run_batch_child(
    path: PathBuf,
    start: usize,
    verbose: bool,
    filter: Option<String>,
) -> Result<(), String> {
    // FIRST, before compiling and long before any test code runs: take the
    // protocol descriptor out of the stdin slot and leave the null device there.
    let Some(mut protocol) = take_protocol_channel() else {
        return Err("the isolated test process has no protocol channel".to_string());
    };
    let compiled = match compile(&path, SourceMode::Library) {
        Ok(compiled) => compiled,
        Err(failure) => {
            return Err(format_reports(
                &failure.reports,
                OutputMode::Concise,
                failure.source.as_deref(),
            ));
        }
    };
    // `false`: the parent already printed this suite's `skip` lines.
    let (names, _filtered_out) =
        discover_tests(&compiled.checked.program, filter.as_deref(), false);

    for (index, name) in names.iter().enumerate().skip(start) {
        let _ = writeln!(protocol, "{START} {index}");
        let result = run_named_function(&compiled.checked.program, name);
        // Flush the test's own output before reporting its result, so the parent
        // prints `PASS`/`FAIL` strictly after it.
        let _ = std::io::stdout().flush();
        match result {
            Ok(_) => {
                let _ = writeln!(protocol, "{PASS} {index}");
            }
            Err(error) => {
                let mut payload = escape(&error.message);
                if verbose {
                    for frame in &error.traceback {
                        let text = match frame.span {
                            Some(span) => {
                                format!("    at {} ({}:{})", frame.function, span.line, span.column)
                            }
                            None => format!("    at {}", frame.function),
                        };
                        payload.push(FIELD_SEPARATOR);
                        payload.push_str(&escape(&text));
                    }
                }
                let _ = writeln!(protocol, "{FAIL} {index} {payload}");
            }
        }
    }
    // Report completion explicitly: the parent must not have to infer it from pipe
    // EOF, which a grandchild holding the inherited handle can defer.
    let _ = writeln!(protocol, "{DONE} 0");
    Ok(())
}

//! CLI integration tests, part 14 — the actor concurrency model. Stage 1:
//! `actor`/`state`/`init`/`on`, `spawn`, and fire-and-forget `tell`. Stage 2:
//! request-reply `ask`/`await`/`Future<R>` (see the stage-2 section below).
//!
//! Stage 1 delivers actors on the **AST interpreter only**. The scheduler is
//! single-threaded, cooperative, and deterministic: `spawn` constructs an actor
//! (running its `init`) and returns an `Actor<T>` handle; `tell` enqueues a
//! fire-and-forget message; every outstanding message is drained
//! run-to-completion, one at a time, before `main` returns. So a `tell` to a
//! handler with an observable side effect (e.g. `print`) produces the **same
//! output on every run**, which is what these tests pin.
//!
//! The IR and bytecode backends **reject** an actor program (`L0355`) and the
//! native/WASM backends **cleanly skip** it (`L0339`/`L0338`), so no backend can
//! silently diverge from the AST semantics. The negative tests pin the
//! declaration/`spawn`/`tell` diagnostics: a `tell` to a reply (`ask`) handler
//! or an unknown handler (`L0352`), a non-sendable message argument (`L0353`),
//! and any external access to an actor's private `state` (`L0354`).

use crate::*;

/// Run a valid fixture on the default (AST) backend and return the captured
/// output. The fixture path is relative to the workspace root.
fn run_ast(fixture: &str) -> std::process::Output {
    let path = workspace_root().join(fixture);
    lullaby()
        .args(["run", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli")
}

/// Assert a fixture reports `code` and exits non-zero on `lullaby check` — the
/// static-rejection path used for the negative (diagnostic) cases.
fn assert_check_rejects(fixture: &str, code: &str) {
    let path = workspace_root().join(fixture);
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "{fixture} should be rejected but exited 0. stderr: {stderr}"
    );
    assert!(
        stderr.contains(code),
        "{fixture} should report {code}. stderr: {stderr}"
    );
}

#[test]
fn actor_counter_is_deterministic_on_ast() {
    // `main` prints its own line first; the drained handler output follows,
    // deterministically. 0 + 5 + 10 + 2 = 17.
    let output = run_ast("tests/fixtures/valid/actors/counter_logger.lby");
    assert!(
        output.status.success(),
        "counter should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "main done\nhits = 17\n0\n");
}

#[test]
fn actor_multiple_actors_are_deterministic_on_ast() {
    // The two `forward`s each enqueue a `log` on the shared logger during the
    // drain; the direct `log` was enqueued first, so it runs first. FIFO order
    // is deterministic: direct, one, two.
    let output = run_ast("tests/fixtures/valid/actors/multiple_actors.lby");
    assert!(
        output.status.success(),
        "multiple actors should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "log: direct\nlog: one\nlog: two\n0\n");
}

#[test]
fn actor_ir_and_bytecode_reject_cleanly() {
    // The IR interpreter and bytecode VM do not support actors yet; both reject
    // an actor program with the dedicated `L0355` rather than silently diverging.
    let path = workspace_root().join("tests/fixtures/valid/actors/counter_logger.lby");
    for backend in ["ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                path.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] actor program should be rejected. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0355"),
            "[{backend}] actor program should report L0355. stderr: {stderr}"
        );
    }
}

#[test]
fn actor_native_skips_cleanly() {
    // A program using actors is native-ineligible: `lullaby native` skips it with
    // `L0339` (no eligible function), never miscompiling `spawn`/`tell`.
    let path = workspace_root().join("tests/fixtures/valid/actors/counter_logger.lby");
    let output = lullaby()
        .args(["native", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "native should skip an actor program. stderr: {stderr}"
    );
    assert!(
        stderr.contains("L0339"),
        "native skip should report L0339. stderr: {stderr}"
    );
}

#[test]
fn actor_wasm_skips_cleanly() {
    // A program using actors is not WASM-eligible: `lullaby wasm` skips it with
    // `L0338` (no eligible scalar function).
    let path = workspace_root().join("tests/fixtures/valid/actors/counter_logger.lby");
    let output = lullaby()
        .args(["wasm", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "wasm should skip an actor program. stderr: {stderr}"
    );
    assert!(
        stderr.contains("L0338"),
        "wasm skip should report L0338. stderr: {stderr}"
    );
}

#[test]
fn actor_tell_to_reply_handler_is_rejected() {
    // `tell` may only target a fire-and-forget handler; a reply (`-> T`) handler
    // is an `ask` handler (a later stage).
    assert_check_rejects(
        "tests/fixtures/invalid/actors/tell_reply_handler.lby",
        "L0352",
    );
}

#[test]
fn actor_tell_to_unknown_handler_is_rejected() {
    assert_check_rejects("tests/fixtures/invalid/actors/unknown_handler.lby", "L0352");
}

#[test]
fn actor_non_sendable_message_arg_is_rejected() {
    // A non-atomic `rc<T>` must not cross an actor boundary.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/non_sendable_arg.lby",
        "L0353",
    );
}

#[test]
fn actor_external_state_read_is_rejected() {
    // An actor's `state` is private: no external field read through the handle.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/external_state_read.lby",
        "L0354",
    );
}

#[test]
fn actor_external_state_write_is_rejected() {
    // An actor's `state` is private: no external field write through the handle.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/external_state_write.lby",
        "L0354",
    );
}

#[test]
fn actor_program_formats_idempotently() {
    // `lullaby fmt` renders an actor program canonically, and re-formatting the
    // output is a fixed point (the formatter round-trips the new construct).
    let path = workspace_root().join("tests/fixtures/valid/actors/counter_logger.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);

    // Write the formatted text to a temp file and format it again; the result
    // must be byte-identical (idempotent).
    let temp = std::env::temp_dir().join("lullaby_actor_fmt_idempotent.lby");
    std::fs::write(&temp, &formatted).expect("write temp");
    let second = lullaby()
        .args(["fmt", temp.to_str().expect("temp path")])
        .output()
        .expect("run fmt again");
    assert!(
        second.status.success(),
        "second fmt failed: {}",
        stderr(&second)
    );
    assert_eq!(stdout(&second), formatted, "fmt must be idempotent");
}

// ---------------------------------------------------------------------------
// Stage 2 — request-reply (`ask` / `await` / `Future<R>`).
//
// `ask handle.handler(args)` sends a request and yields a `Future<R>` for the
// handler's `-> R` reply; `await` drives the deterministic single-threaded
// mailbox until the reply slot is filled, then yields `R`. A request cycle that
// could only be satisfied by re-entering a busy actor is a clean deterministic
// deadlock (`L0356`), never a hang. Like stage 1, request-reply runs on the AST
// interpreter only: IR/bytecode reject (`L0355`), native/WASM skip
// (`L0339`/`L0338`), and a `no-runtime` module rejects it (`L0441`).
// ---------------------------------------------------------------------------

/// Assert a fixture runs on the AST backend but fails at run time reporting
/// `code` (the runtime-rejection path, e.g. a deterministic actor deadlock).
fn assert_run_fails(fixture: &str, code: &str) {
    let output = run_ast(fixture);
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "{fixture} should fail at run time but exited 0. stderr: {stderr}"
    );
    assert!(
        stderr.contains(code),
        "{fixture} should report {code}. stderr: {stderr}"
    );
}

#[test]
fn actor_ask_await_reply_is_deterministic() {
    // Two `tell`s enqueue increments ahead of the `ask`; `await` drives the
    // mailbox FIFO, so both are processed before `value` replies. 0 + 5 + 3 = 8.
    // Then `main` prints 8 and returns 0 (the CLI echoes the non-void return).
    let output = run_ast("tests/fixtures/valid/actors/ask_counter.lby");
    assert!(
        output.status.success(),
        "ask/await counter should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "8\n0\n");
}

#[test]
fn actor_ask_chained_and_inter_actor_is_deterministic() {
    // The client asks a Coordinator, which asks a Worker twice inside its turn
    // and awaits each reply (a nested `await` that runs the Worker while the
    // Coordinator is busy, without re-entering it). solve(3) = 9 + 16 = 25.
    let output = run_ast("tests/fixtures/valid/actors/ask_chained.lby");
    assert!(
        output.status.success(),
        "chained ask should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "25\n0\n");
}

#[test]
fn actor_ask_self_cycle_deadlocks_cleanly() {
    // An actor that asks itself inside a turn can never complete (non-reentrant
    // run-to-completion): the scheduler detects no deliverable message and
    // reports L0356 rather than hanging.
    assert_run_fails("tests/fixtures/valid/actors/ask_deadlock.lby", "L0356");
}

#[test]
fn actor_ask_fire_and_forget_handler_is_rejected() {
    // `ask` requires a reply (`-> T`) handler; asking a fire-and-forget handler
    // has no reply to await. -> L0352
    assert_check_rejects(
        "tests/fixtures/invalid/actors/ask_fire_and_forget_handler.lby",
        "L0352",
    );
}

#[test]
fn actor_ask_non_sendable_arg_is_rejected() {
    // An `ask` argument crosses the actor boundary, so a non-atomic `rc<T>` is
    // rejected exactly like a `tell` argument. -> L0353
    assert_check_rejects(
        "tests/fixtures/invalid/actors/ask_non_sendable_arg.lby",
        "L0353",
    );
}

#[test]
fn actor_ask_non_sendable_reply_is_rejected() {
    // A reply value crosses the actor boundary back to the asker, so a
    // non-atomic `rc<T>` reply is rejected at the handler declaration. -> L0353
    assert_check_rejects(
        "tests/fixtures/invalid/actors/ask_non_sendable_reply.lby",
        "L0353",
    );
}

#[test]
fn actor_ask_ir_and_bytecode_reject_cleanly() {
    // Request-reply runs only on the AST interpreter; the IR interpreter and
    // bytecode VM reject an actor program (reusing the stage-1 `L0355` gate,
    // which covers `ask` because it shares the `tell` message-send node).
    let path = workspace_root().join("tests/fixtures/valid/actors/ask_counter.lby");
    for backend in ["ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                path.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] ask program should be rejected. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0355"),
            "[{backend}] ask program should report L0355. stderr: {stderr}"
        );
    }
}

#[test]
fn actor_ask_native_and_wasm_skip_cleanly() {
    // A program using `ask` declares actors, so both native and WASM skip it
    // cleanly (`L0339`/`L0338`) — never miscompiling request-reply.
    let path = workspace_root().join("tests/fixtures/valid/actors/ask_counter.lby");
    for (command, code) in [("native", "L0339"), ("wasm", "L0338")] {
        let output = lullaby()
            .args([command, path.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{command}] ask program should skip. stderr: {stderr}"
        );
        assert!(
            stderr.contains(code),
            "[{command}] ask program should report {code}. stderr: {stderr}"
        );
    }
}

#[test]
fn actor_ask_in_no_runtime_module_is_rejected() {
    // A `no-runtime` (freestanding) module has no scheduler: `ask`/`await` and
    // the actor they drive are rejected with `L0441`.
    assert_check_rejects("tests/fixtures/invalid/no_runtime/actor_ask.lby", "L0441");
}

#[test]
fn ask_await_program_formats_idempotently() {
    // `lullaby fmt` renders `ask`/`await`/`Future<R>` canonically and
    // re-formatting is a fixed point (the formatter round-trips request-reply).
    let path = workspace_root().join("tests/fixtures/valid/actors/ask_chained.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);
    assert!(
        formatted.contains("await ask "),
        "formatted output should render `await ask`: {formatted}"
    );

    let temp = std::env::temp_dir().join("lullaby_ask_fmt_idempotent.lby");
    std::fs::write(&temp, &formatted).expect("write temp");
    let second = lullaby()
        .args(["fmt", temp.to_str().expect("temp path")])
        .output()
        .expect("run fmt again");
    assert!(
        second.status.success(),
        "second fmt failed: {}",
        stderr(&second)
    );
    assert_eq!(stdout(&second), formatted, "fmt must be idempotent");
}

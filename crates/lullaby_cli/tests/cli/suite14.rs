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
    let scratch = ScratchDir::new("actor_program_formats_idempotently");
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
    let temp = scratch.join("lullaby_actor_fmt_idempotent.lby");
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
    let scratch = ScratchDir::new("ask_await_program_formats_idempotently");
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

    let temp = scratch.join("lullaby_ask_fmt_idempotent.lby");
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
// Stage 3 — message ownership: move-by-default + `copy` + `shared` + the
// use-after-send analysis (`L0357`), plus the transitive-sendability closure
// (`L0353` now also catches a non-atomic `rc`/`ref`/`ptr` hidden in a struct
// field or enum payload). All run on the AST interpreter, like stages 1-2.
// ---------------------------------------------------------------------------

#[test]
fn move_copy_scalar_is_reusable_after_send() {
    // A scalar (`i64`) is a *copy* type: sending it copies it, so the sender
    // keeps its value and may send/read it again with no use-after-send error.
    let output = run_ast("tests/fixtures/valid/actors/move_reused_scalar_ok.lby");
    assert!(
        output.status.success(),
        "copy-type reuse should run: {}",
        stderr(&output)
    );
    // `main` prints `7`, then the drained `add`/`report` turns total to 14, and
    // `lullaby run` prints `main`'s `0` return.
    assert_eq!(stdout(&output), "7\n14\n0\n");
}

#[test]
fn shared_handle_is_reusable_across_actors() {
    // A `shared<T>` handle (the atomic-rc immutable share) is sendable and not
    // consumed by a send, so it can be handed to several actors. Both readers
    // read the same immutable `42`.
    let output = run_ast("tests/fixtures/valid/actors/shared_two_actors.lby");
    assert!(
        output.status.success(),
        "shared reuse should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "read 42\nread 42\n0\n");
}

#[test]
fn use_after_tell_is_rejected() {
    // A `string` (owned aggregate) is moved into a `tell`; reading it afterward
    // is a use-after-send.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/move_use_after_tell.lby",
        "L0357",
    );
}

#[test]
fn use_after_ask_is_rejected() {
    // Moving a value into an `ask` request consumes it too: a later use is
    // rejected even after the reply is awaited.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/move_use_after_ask.lby",
        "L0357",
    );
}

#[test]
fn rc_in_struct_field_is_not_sendable() {
    // Transitive sendability: a non-atomic `rc<T>` hidden in a struct field is
    // caught by `L0353` when the struct is sent in a `tell`.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/rc_in_struct_field.lby",
        "L0353",
    );
}

#[test]
fn ref_in_enum_payload_reply_is_not_sendable() {
    // Transitive sendability: a borrowed `ref<T>` hidden in an enum-variant
    // payload is caught by `L0353` at the `ask` reply handler's declaration.
    assert_check_rejects(
        "tests/fixtures/invalid/actors/ref_in_enum_payload_reply.lby",
        "L0353",
    );
}

// ---------------------------------------------------------------------------
// Stage 4: supervision / failure handling.
//
// Actor failure is **result-based, not panic-based**. Decision A5 aborts on a
// contract violation and does not unwind, so a supervisor has nothing to catch;
// the supervised failure channel is instead a handler declared `-> result<R, E>`
// replying `err(e)`. A genuine panic still aborts the program and is never
// supervised — `supervised_panic_aborts_the_program` pins that boundary, and it
// must not be "fixed" into a supervised failure.
//
// Supervision is opt-in via `spawn NAME(args) supervise restart|stop|escalate`,
// because `err` is also the ordinary recoverable-error channel.
// ---------------------------------------------------------------------------

#[test]
fn supervise_restart_gives_the_child_fresh_state() {
    // Two good deposits accumulate (4, then 7) — a supervised child that succeeds
    // is completely unaffected by having a policy. The third overflows: the
    // handler replies `err`, the asker still receives that `err`, and the
    // supervisor then restarts the tank. The restart is observable in the fourth
    // deposit: it lands on fresh state (5), not on the pre-failure level (12).
    let output = run_ast("tests/fixtures/valid/actors/supervise_restart.lby");
    assert!(
        output.status.success(),
        "supervise restart should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "ok 4\nok 7\nerr overflow\nok 5\n0\n");
}

#[test]
fn supervise_stop_terminates_the_child_and_drops_later_tells() {
    // The failing deposit replies `err` to its asker, then the policy stops the
    // tank for good. A later `tell` to a stopped actor is dropped (a
    // fire-and-forget send has no channel to report on), so the program still
    // drains to a clean exit.
    let output = run_ast("tests/fixtures/valid/actors/supervise_stop.lby");
    assert!(
        output.status.success(),
        "supervise stop should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "ok 4\nerr overflow\nafter stop\n0\n");
}

#[test]
fn ask_to_a_stopped_actor_fails_deterministically() {
    // An `ask` to an actor supervision has stopped can never be answered. The
    // reply type is the user's own `result<i64, string>` and the runtime cannot
    // fabricate an inhabitant of an arbitrary `E`, so this is a clean L0359 —
    // never a fabricated `err`, and above all never a hang.
    assert_run_fails(
        "tests/fixtures/valid/actors/supervise_ask_stopped.lby",
        "L0359",
    );
}

#[test]
fn an_ask_in_flight_when_the_child_stops_is_failed_not_hung() {
    // Both requests are queued before either runs. Awaiting the first fails the
    // tank and stops it, which purges the mailbox — including the second,
    // still-queued request. Its slot is marked unavailable, so awaiting it
    // reports L0359 rather than waiting forever for a turn that will never run.
    let output = run_ast("tests/fixtures/valid/actors/supervise_inflight_ask.lby");
    assert!(
        !output.status.success(),
        "an in-flight ask across a stop should fail"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("L0359"),
        "the purged in-flight asker should report L0359. stderr: {stderr}"
    );
    // The failure itself still reached its own asker as an ordinary `err` value.
    assert_eq!(stdout(&output), "err overflow\n");
}

#[test]
fn an_ask_in_flight_across_a_restart_is_served_by_the_fresh_actor() {
    // A restart preserves the mailbox, so a request queued behind the failing one
    // is not lost: it runs on the restarted actor. `init` reset the level to 0, so
    // the in-flight deposit of 2 replies `ok 2` (not `ok 8`). The failing message
    // is *not* replayed — it was consumed by the turn that returned `err` — which
    // is why a restart cannot loop on a poison message.
    let output = run_ast("tests/fixtures/valid/actors/supervise_inflight_restart.lby");
    assert!(
        output.status.success(),
        "an in-flight ask across a restart should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "ok 6\nerr overflow\nok 2\n0\n");
}

#[test]
fn supervise_escalate_walks_up_a_nested_supervision_tree() {
    // main --restart--> Manager --escalate--> Worker. The Worker's failure is not
    // handled locally: the Worker stops and the failure passes to the Manager,
    // which applies its own `restart`. The Manager's handler still completes
    // normally first (returning -1) — a supervisory action lands at the turn
    // boundary, never underneath a running turn — and the restart is then
    // observable: the fresh Worker has a full budget, so spending 4 leaves 6.
    let output = run_ast("tests/fixtures/valid/actors/supervise_escalate_nested.lby");
    assert!(
        output.status.success(),
        "nested escalate should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "6\n-1\n6\n0\n");
}

#[test]
fn escalation_that_reaches_the_root_stops_the_program() {
    // An actor spawned from `main` is a root actor with no supervisor, so an
    // escalation from it has nowhere to go. The failure is not silently
    // discarded: the program stops with a deterministic L0362.
    let output = run_ast("tests/fixtures/valid/actors/supervise_escalate_root.lby");
    assert!(
        !output.status.success(),
        "escalation to the root should stop the program"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("L0362"),
        "escalation to the root should report L0362. stderr: {stderr}"
    );
    // The successful open ran; "unreachable" after the escalation never printed.
    assert_eq!(stdout(&output), "ok 1\n");
}

#[test]
fn supervised_panic_aborts_the_program() {
    // THE A5 BOUNDARY PIN. Do not "improve" this into a supervised failure.
    //
    // The actor is supervised with `restart` and has a fallible handler, so
    // supervision is fully armed — and its `dig(5)` failure is duly handled as a
    // result-based failure. But its `bug` handler indexes out of bounds: a
    // contract violation, i.e. a *bug*. Per decision A5 that aborts the program
    // deterministically and does not unwind, so a supervisor has nothing to catch.
    // It is NOT restarted, NOT stopped, and "unreachable" never prints.
    let output = run_ast("tests/fixtures/valid/actors/supervise_panic_aborts.lby");
    assert!(
        !output.status.success(),
        "a panicking handler must abort the program, not be supervised"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("L0413"),
        "a bounds violation in a handler must abort with the bounds diagnostic, \
         not be turned into a supervised failure. stderr: {stderr}"
    );
    // The `err` path was supervised; the panic path aborted before "unreachable".
    assert_eq!(stdout(&output), "err no such hole\n");
}

#[test]
fn a_failing_init_is_caught_instead_of_restarting_forever() {
    // A restart cannot loop on a poison message, but an `init` that itself drives
    // a child to fail re-fails on every restart. Rather than spin forever, the
    // scheduler reports a deterministic L0363 on the second attempt.
    let output = run_ast("tests/fixtures/valid/actors/supervise_restart_loop.lby");
    assert!(!output.status.success(), "a failing init should be caught");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("L0363"),
        "a failing init should report L0363 rather than looping. stderr: {stderr}"
    );
    // Exactly two attempts ran — the original and one restart — then it stopped.
    assert_eq!(
        stdout(&output),
        "init saw always fails\ninit saw always fails\n"
    );
}

#[test]
fn supervision_composes_with_message_ownership() {
    // Stage-3 semantics are unchanged by supervision: a moved payload stays moved
    // and is discarded with the restarted actor's state, `shared<T>` is not
    // consumed by a send (so `tag` is still readable at the end, and `init`
    // re-supplies it from the retained spawn argument on restart), and copy-set
    // values stay usable across sends.
    let output = run_ast("tests/fixtures/valid/actors/supervise_ownership.lby");
    assert!(
        output.status.success(),
        "ownership composition should run: {}",
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "ok 1\nvault holds 1\nerr vault full\nvault holds 0\ntag still readable: vault\n0\n"
    );
}

#[test]
fn move_into_a_supervised_message_is_still_use_after_send() {
    // A failed handler does not hand its payload back and a restart discards it,
    // so a value moved into a message stays moved whatever supervision does. -> L0357
    assert_check_rejects(
        "tests/fixtures/invalid/actors/supervise_move_use_after_send.lby",
        "L0357",
    );
}

#[test]
fn supervise_on_an_actor_that_cannot_fail_is_rejected() {
    // `Counter` declares no fallible handler and the program uses no `escalate`,
    // so the policy could never apply. This is the diagnostic that corrects the
    // most likely misconception: supervision does not catch panics. -> L0358
    assert_check_rejects(
        "tests/fixtures/invalid/actors/supervise_no_fallible_handler.lby",
        "L0358",
    );
}

#[test]
fn no_runtime_rejects_supervision() {
    // The freestanding tier has no scheduler, so it has no supervision either.
    // The `supervise` clause rides on the existing `spawn` node, so it inherits
    // the same L0441 gate with no new tier plumbing. -> L0441
    assert_check_rejects(
        "tests/fixtures/invalid/no_runtime/actor_supervise.lby",
        "L0441",
    );
}

#[test]
fn supervise_ir_and_bytecode_reject_cleanly() {
    // Supervision runs on the AST interpreter only. Because the `supervise` clause
    // is a field on the existing `spawn` node rather than a new construct, the
    // IR/bytecode backends reject it through the stage-1 `L0355` gate unchanged.
    let path = workspace_root().join("tests/fixtures/valid/actors/supervise_restart.lby");
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
            "{backend} should reject a supervised actor program. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0355"),
            "{backend} should reject with L0355. stderr: {stderr}"
        );
    }
}

#[test]
fn supervise_clause_formats_idempotently() {
    let scratch = ScratchDir::new("supervise_clause_formats_idempotently");
    // The formatter must render the `supervise POLICY` clause back out, so a
    // formatted program still supervises. Dropping it would silently unsupervise
    // an actor — a formatter changing program behavior — so pin both that the
    // clause survives and that re-formatting is a fixed point.
    let path = workspace_root().join("tests/fixtures/valid/actors/supervise_escalate_nested.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);
    assert!(
        formatted.contains("supervise restart"),
        "fmt must preserve `supervise restart`; dropping it would unsupervise the \
         actor. output: {formatted}"
    );
    assert!(
        formatted.contains("supervise escalate"),
        "fmt must preserve `supervise escalate`. output: {formatted}"
    );

    let temp = scratch.join("lullaby_actor_supervise_fmt_idempotent.lby");
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

#[test]
fn supervise_output_is_byte_identical_across_repeated_runs() {
    // Determinism is the property stages 2-4 are held to: supervision adds
    // restart/stop/escalate decisions to the schedule, and every one of them must
    // be reproducible. The scheduler is single-threaded and run-to-completion, and
    // supervisory actions land at deterministic turn boundaries, so the same
    // program produces byte-identical output on every run.
    for fixture in [
        "tests/fixtures/valid/actors/supervise_restart.lby",
        "tests/fixtures/valid/actors/supervise_stop.lby",
        "tests/fixtures/valid/actors/supervise_inflight_ask.lby",
        "tests/fixtures/valid/actors/supervise_inflight_restart.lby",
        "tests/fixtures/valid/actors/supervise_escalate_nested.lby",
        "tests/fixtures/valid/actors/supervise_escalate_root.lby",
        "tests/fixtures/valid/actors/supervise_restart_loop.lby",
        "tests/fixtures/valid/actors/supervise_ownership.lby",
    ] {
        let first = run_ast(fixture);
        for _ in 0..4 {
            let again = run_ast(fixture);
            assert_eq!(
                stdout(&first),
                stdout(&again),
                "{fixture} stdout must be identical on every run"
            );
            assert_eq!(
                first.status.code(),
                again.status.code(),
                "{fixture} exit code must be identical on every run"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 5 — the `Future<T>` combinators `join_all` and `select`.
//
// `join_all EXPR` waits for **every** future in a collection of `ask` replies to
// resolve and yields the results in input order (kind-preserving:
// `array<Future<T>>` -> `array<T>`, `list<Future<T>>` -> `list<T>`). `select
// EXPR` waits for the **first** future to resolve and yields a `Selected<T>`
// { `index i64`, `value T` }; when more than one future is resolved at the
// moment `select` inspects them, the **lowest input index** wins the tie. Both
// combinators run on the same deterministic run-to-completion scheduler as
// `await`, so their output is byte-identical across runs, and both inherit the
// actor tier story unchanged: IR/bytecode reject the program (`L0355`),
// native/WASM skip it (`L0339`/`L0338`), and a `no-runtime` module rejects it
// (`L0441`). Misuse (an operand that is not a collection of `Future<T>`) is a
// static `L0364`.
// ---------------------------------------------------------------------------

#[test]
fn join_all_collects_every_reply_in_input_order() {
    // Three asks fanned to one worker; `join_all` waits for all and returns
    // `[2*2, 3*3, 4*4] = [4, 9, 16]` in input order. Determinism: the scheduler
    // is FIFO run-to-completion, so this is byte-identical every run.
    let output = run_ast("tests/fixtures/valid/actors/join_all_asks.lby");
    assert!(
        output.status.success(),
        "join_all should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "4\n9\n16\n0\n");
}

#[test]
fn select_returns_the_first_future_to_resolve_by_readiness() {
    // index 0 asks the router itself (busy for the whole `route` turn, so its
    // reply can never be produced there); index 1 asks an idle backend, which
    // resolves. `select` returns index 1 — proving it waits on readiness, not
    // input order — with the backend's reply (5 + 100 = 105).
    let output = run_ast("tests/fixtures/valid/actors/select_first_ready.lby");
    assert!(
        output.status.success(),
        "select should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "index 1\nanswer 105\n0\n");
}

#[test]
fn select_tie_break_is_lowest_input_index() {
    // Both futures are resolved by the single turn that runs `coord.compute` (its
    // internal `await` drives the queued `leaf.step2`, index 1, to resolve
    // *before* `compute` itself, index 0). `select` still returns index 0: the
    // tie-break scans in input order, not chronological readiness. Value 41 is
    // `compute`'s reply (1 + 40), not `step2`'s 2.
    let output = run_ast("tests/fixtures/valid/actors/select_tiebreak.lby");
    assert!(
        output.status.success(),
        "select tie-break should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "index 0\nvalue 41\n0\n");
}

#[test]
fn combinator_output_is_byte_identical_across_repeated_runs() {
    // The determinism guarantee stages 2-4 are held to extends to the
    // combinators: every `join_all`/`select` schedule is a function of the fixed
    // single-threaded run-to-completion order, so the same program produces
    // byte-identical output (and the same exit code) on every run. Mirrors
    // `supervise_output_is_byte_identical_across_repeated_runs`.
    for fixture in [
        "tests/fixtures/valid/actors/join_all_asks.lby",
        "tests/fixtures/valid/actors/select_first_ready.lby",
        "tests/fixtures/valid/actors/select_tiebreak.lby",
    ] {
        let first = run_ast(fixture);
        for _ in 0..4 {
            let again = run_ast(fixture);
            assert_eq!(
                stdout(&first),
                stdout(&again),
                "{fixture} stdout must be identical on every run"
            );
            assert_eq!(
                first.status.code(),
                again.status.code(),
                "{fixture} exit code must be identical on every run"
            );
        }
    }
}

#[test]
fn join_all_over_non_futures_is_rejected() {
    // `join_all` requires a collection of `Future<T>`; an `array<i64>` has no
    // futures to wait on. -> L0364
    assert_check_rejects(
        "tests/fixtures/invalid/actors/join_all_non_future.lby",
        "L0364",
    );
}

#[test]
fn select_over_non_futures_is_rejected() {
    // `select` requires a collection of `Future<T>`; an `array<i64>` has no
    // future to select. -> L0364
    assert_check_rejects(
        "tests/fixtures/invalid/actors/select_non_future.lby",
        "L0364",
    );
}

#[test]
fn combinators_in_no_runtime_module_are_rejected() {
    // A `no-runtime` (freestanding) module has no scheduler: the future
    // combinators, like the actor they drive, are rejected with `L0441`.
    assert_check_rejects(
        "tests/fixtures/invalid/no_runtime/actor_join_all.lby",
        "L0441",
    );
}

#[test]
fn combinator_ir_and_bytecode_reject_cleanly() {
    // The combinators run only on the AST interpreter; the IR interpreter and
    // bytecode VM reject an actor program through the stage-1 `L0355` gate.
    let path = workspace_root().join("tests/fixtures/valid/actors/join_all_asks.lby");
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
            "[{backend}] combinator program should be rejected. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0355"),
            "[{backend}] combinator program should report L0355. stderr: {stderr}"
        );
    }
}

#[test]
fn combinator_native_and_wasm_skip_cleanly() {
    // A program using `join_all`/`select` declares actors, so native and WASM
    // skip it cleanly (`L0339`/`L0338`) — never miscompiling a combinator.
    let path = workspace_root().join("tests/fixtures/valid/actors/select_tiebreak.lby");
    for (command, code) in [("native", "L0339"), ("wasm", "L0338")] {
        let output = lullaby()
            .args([command, path.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{command}] combinator program should skip. stderr: {stderr}"
        );
        assert!(
            stderr.contains(code),
            "[{command}] combinator program should report {code}. stderr: {stderr}"
        );
    }
}

#[test]
fn combinator_program_formats_idempotently() {
    let scratch = ScratchDir::new("combinator_program_formats_idempotently");
    // `lullaby fmt` renders `join_all`/`select` canonically, and re-formatting the
    // output is a fixed point (the formatter round-trips the new combinators).
    let path = workspace_root().join("tests/fixtures/valid/actors/select_tiebreak.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);
    assert!(
        formatted.contains("select [ask "),
        "formatted output should render `select [ask`: {formatted}"
    );

    let temp = scratch.join("lullaby_combinator_fmt_idempotent.lby");
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

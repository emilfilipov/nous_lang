//! CLI integration tests — an **inline conditional as a closure body**, across
//! every execution tier.
//!
//! # The divergence these pin
//!
//! A closure body is a single expression in the surface grammar (`expr_parser`
//! parses it with `parse_conditional`), but a *ternary* body does not lower to a
//! single IR expression: `desugar_conditional` hoists `let #cond_N` plus an `if`
//! and rewrites the position to `#cond_N`. That scaffolding reads the closure's
//! **parameters**.
//!
//! The closure lowerer used to lower its body with a plain `lower_expr`, leaving
//! those statements in the shared `try_prelude`, which the enclosing block lowerer
//! drained into the **enclosing function**. At runtime the enclosing frame has no
//! such parameter, so:
//!
//! | tier            | `fn x i64 -> 1 if x > 0 else 0`     |
//! |-----------------|-------------------------------------|
//! | AST interpreter | correct answer (never hoists)       |
//! | IR interpreter  | `L0403 unknown variable \`x\`` in `main` |
//! | bytecode VM     | `L0403 unknown variable \`x\`` in `main` |
//! | native          | clean `L0339` skip                  |
//!
//! Two tiers died at **runtime**, naming the user's own closure parameter, on a
//! program semantics had accepted and one tier answered correctly. The fix gives
//! `IrClosureDef` a `prelude` that travels with the closure and runs in the
//! closure's own frame, per call.
//!
//! # Why native is pinned as a refusal, not an answer
//!
//! Native does not compile a ternary-bodied closure: the body is `#cond_N`, which
//! its capture analysis cannot bind to a native local, so the function skips
//! cleanly to the interpreters (`L0339`). That is the correct-or-refuse contract —
//! a tier answers like the others or declines with a diagnostic. What it must never
//! do is compile the body while ignoring the prelude, which would answer
//! *differently*. `native_closure_conditional_body_skips_cleanly` pins the refusal
//! so that stays a deliberate boundary rather than something that quietly erodes.
//!
//! # The two ways this was got wrong, both pinned here
//!
//! **The temps had to become unspellable.** They were `__cond_N`/`__match_v_N` —
//! names a user can spell. A program that declared one had its binding clobbered by
//! the desugar's own `let` (`desugar_temps_do_not_shadow_user_bindings`), and,
//! worse, a user-declared `__cond_0` *satisfied native's capture lookup*, so the
//! skip above evaporated and native compiled the body while dropping the prelude —
//! exiting 1110 where the interpreters said 556
//! (`native_closure_conditional_skip_survives_a_user_declared_temp_name`). The
//! prefix is now `#`, which the lexer cannot produce, so both are impossible by
//! construction rather than by convention.
//!
//! **`?` had to be refused, not reinterpreted.** The prelude mechanism also carries
//! the `?` desugar, whose `return` has no meaning in a closure frame. Yielding it as
//! the closure's value turned a loud `L0403` into a *silent* type-confused answer.
//! `?` in a closure body is now rejected by semantics on every tier (`L0462`,
//! `try_inside_a_closure_body_is_refused_on_every_tier`).

use super::{ScratchDir, lullaby, stderr, stdout, workspace_root};

/// Run a fixture on the three interpreter tiers and assert each prints `expected`.
fn assert_interpreters_agree(fixture: &str, expected: &str) {
    let path = workspace_root().join(format!("tests/fixtures/valid/{fixture}.lby"));
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(backend) = backend {
            args.push("--backend".to_string());
            args.push(backend.to_string());
        }
        args.push(path.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(
            output.status.success(),
            "{fixture} on backend {backend:?}: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            expected,
            "{fixture}: backend {backend:?} must agree with every other tier"
        );
    }
}

/// An inline conditional as a closure body — captured, nested, passed to a
/// higher-order function, returned from one, and with both branches taken —
/// computes 45 identically on all three interpreter tiers.
///
/// Before the fix this printed 45 on the AST interpreter and failed with
/// `L0403 unknown variable \`x\`` on the other two.
#[test]
pub(crate) fn runs_closure_conditional_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_closure_conditional.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(backend) = backend {
            args.push("--backend".to_string());
            args.push(backend.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(
            output.status.success(),
            "backend {backend:?}: a ternary closure body must run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "45",
            "backend {backend:?}: every tier must agree on the ternary-closure result"
        );
    }
}

/// The ternary body is evaluated **lazily, at call time**: exactly the taken arm
/// runs, exactly once per call.
///
/// The transcript is the whole point. `TAKEN` is printed only by the then-arm, and
/// the closure is called once taking each branch, so the exact stdout distinguishes
/// correct laziness from every plausible wrong schedule:
///
/// - untaken arm evaluated too  -> `TAKEN` appears twice,
/// - prelude run eagerly at closure creation -> `TAKEN` precedes `start`, or the
///   parameter-reading prelude fails outright,
/// - prelude run once and cached -> the second call cannot re-decide.
#[test]
pub(crate) fn closure_conditional_body_is_lazy_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_closure_conditional_lazy.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(backend) = backend {
            args.push("--backend".to_string());
            args.push(backend.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(
            output.status.success(),
            "backend {backend:?}: {}",
            stderr(&output)
        );
        let rendered = stdout(&output);
        let lines: Vec<&str> = rendered.lines().map(str::trim).collect();
        // `start` then one `TAKEN` (from `f(2)` only) then `end`, then `a + b` = 3.
        assert_eq!(
            lines,
            vec!["start", "TAKEN", "end", "3"],
            "backend {backend:?}: only the taken arm may run, once, at call time"
        );
    }
}

/// A closure whose *only* disqualifying trait is its ternary body skips cleanly to
/// the interpreters (`L0339`) rather than being miscompiled.
///
/// The fixture is otherwise squarely inside the native scalar subset — a direct
/// literal, no captures, scalar parameter and return, called directly — so this
/// isolates the conditional as the sole cause. Native must refuse, and the
/// interpreters must still compute 10.
#[test]
pub(crate) fn native_closure_conditional_body_skips_cleanly() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_closure_conditional_skip.lby");

    let native = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !native.status.success(),
        "a ternary-bodied closure must not compile natively"
    );
    let rendered = format!("{}{}", stdout(&native), stderr(&native));
    assert!(rendered.contains("L0339"), "expected L0339: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected `main` in the skip listing: {rendered}"
    );

    // The refusal is a boundary, not a dead end: the program still runs, and every
    // interpreter tier agrees on the answer native declined to produce.
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(backend) = backend {
            args.push("--backend".to_string());
            args.push(backend.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let run = lullaby().args(&args).output().expect("run cli");
        assert!(
            run.status.success(),
            "backend {backend:?}: {}",
            stderr(&run)
        );
        assert_eq!(
            stdout(&run).trim(),
            "10",
            "backend {backend:?}: the skipped program still runs on the interpreters"
        );
    }
}

/// A user binding named like a desugar temp is an ordinary binding on every tier.
///
/// The lowerer's temps used to be spelled `__cond_N`/`__match_v_N` — names a user
/// can spell — and the desugar's own `let` then clobbered the user's binding. Only
/// the AST interpreter, which never desugars, stayed correct:
///
/// ```text
/// let __cond_0 i64 = 555
/// let y i64 = 1 if __cond_0 > 0 else 2   # AST: 556   IR/bytecode/native: 4
/// ```
///
/// The temps are now prefixed `#`, which the lexer cannot produce, so the collision
/// cannot be expressed. All four tiers run this, native included via a real `.exe`
/// exit code. With the bug the answer is 18, not 148.
#[test]
pub(crate) fn desugar_temps_do_not_shadow_user_bindings() {
    assert_interpreters_agree("run_desugar_temp_shadow", "148");

    let fixture = workspace_root().join("tests/fixtures/valid/run_desugar_temp_shadow.lby");
    let dir = ScratchDir::new("desugar_temp_shadow");
    let out = dir.join("shadow.exe");
    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out"),
            fixture.to_str().expect("fixture"),
        ])
        .output()
        .expect("run cli");
    assert!(
        emit.status.success(),
        "the shadow fixture must compile natively: {}",
        stderr(&emit)
    );
    let exe = std::process::Command::new(&out).output().expect("run exe");
    assert_eq!(
        exe.status.code().expect("native exit code"),
        148,
        "native must agree with the interpreters on a user-declared `__cond_0`"
    );
}

/// Native's ternary-closure skip cannot be defeated by a user-chosen name.
///
/// The skip is narrow: the body's free `#cond_N` is treated as a capture, and
/// native refuses because `ctx.local` cannot resolve it. While the temp was spelled
/// `__cond_0`, *declaring that name* satisfied the lookup — so native compiled a
/// closure it had always refused, **ignoring the prelude that defines the value**,
/// and exited 1110 where every interpreter said 556. That is precisely the
/// compile-while-dropping-the-prelude outcome correct-or-refuse forbids.
///
/// This pins the skip as unsubvertible. It is a *different* failure from
/// `native_closure_conditional_body_skips_cleanly`: that one dies if native stops
/// refusing at all, this one dies only if a user identifier can reach the temp.
#[test]
pub(crate) fn native_closure_conditional_skip_survives_a_user_declared_temp_name() {
    let fixture =
        workspace_root().join("tests/fixtures/valid/native_closure_conditional_shadow_skip.lby");

    let native = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !native.status.success(),
        "a user-declared `__cond_0` must not let a ternary-bodied closure compile"
    );
    let rendered = format!("{}{}", stdout(&native), stderr(&native));
    assert!(rendered.contains("L0339"), "expected L0339: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected `main` in the skip listing: {rendered}"
    );

    assert_interpreters_agree("native_closure_conditional_shadow_skip", "556");
}

/// `?` inside a closure body is refused identically by every tier (`L0462`).
///
/// It used to type-check, binding to the *enclosing* function's return type, and
/// then diverge: the AST interpreter propagated the error out of that function
/// while the IR/bytecode tiers produced a type-confused value — a closure declared
/// `fn(i64) -> i64` handing back `err(-1)`, which flowed through `ok(f(7))` and out
/// of a `main -> i64` with **exit 0**. A silent wrong answer where another tier had
/// been loud is the one move correct-or-refuse forbids outright, so all four tiers
/// now decline together, at compile time.
#[test]
pub(crate) fn try_inside_a_closure_body_is_refused_on_every_tier() {
    let fixture = workspace_root().join("tests/fixtures/invalid/try_in_closure_body.lby");
    let path = fixture.to_str().expect("fixture path");

    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(backend) = backend {
            args.push("--backend".to_string());
            args.push(backend.to_string());
        }
        args.push(path.to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(
            !output.status.success(),
            "backend {backend:?}: `?` in a closure body must not run"
        );
        let rendered = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            rendered.contains("L0462"),
            "backend {backend:?}: expected L0462: {rendered}"
        );
    }

    // Native refuses at the same frontend gate, before any codegen decision.
    let native = lullaby().args(["native", path]).output().expect("run cli");
    assert!(
        !native.status.success(),
        "native must refuse `?` in a closure"
    );
    let rendered = format!("{}{}", stdout(&native), stderr(&native));
    assert!(
        rendered.contains("L0462"),
        "native: expected L0462: {rendered}"
    );
}

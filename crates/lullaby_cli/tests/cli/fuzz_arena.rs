//! Fuzzing for the freestanding-tier **static-buffer arena**
//! (`native_object_arena.rs`, `documents/freestanding_tier_design.md` §5). A
//! submodule of `fuzz.rs`, reusing its shared `Rng` and `fuzz_native_exit` harness
//! via `use super::*`.
//!
//! # Two oracles, both real
//!
//! The arena has **full four-tier parity**, so this fuzzer gets both oracles the
//! harness supports:
//!
//! * a **differential** oracle — native's exit code must equal the interpreters',
//!   and the three interpreters must agree with each other; and
//! * a **value** oracle — the generator computes each program's exact result
//!   independently in Rust, so a bug that corrupts *every* tier identically still
//!   fails. A pure differential would miss that.
//!
//! (An earlier version had only the value oracle, because the interpreters refused
//! `arena_alloc` outright. That refusal was wrong — an arena cell is an ordinary
//! `array<i64>` element, which every tier addresses — and removing it made the
//! stronger differential oracle available.)
//!
//! Between them these catch a bump that fails to advance (allocations aliasing), a
//! cursor that starts at garbage, and a mis-scaled address — each as a wrong exit
//! code rather than a subtle difference.
//!
//! # The boundary is the point
//!
//! [`gen_arena_program`] deliberately straddles the buffer's capacity: it picks a
//! request sequence that sometimes fits **exactly** and sometimes exceeds by
//! exactly one cell. The exactly-fitting case must SUCCEED and the by-one case must
//! TRAP, which pins the range check's off-by-one — a `jae` where the code means
//! `ja` would either trap on a legal full-buffer allocation or hand out a pointer
//! one cell past the end. Both are caught here.

use super::*;

/// One generated arena program plus the oracle for it.
struct ArenaCase {
    source: String,
    /// The exit code a correct implementation must produce, or `None` when the
    /// program is expected to hit the overflow edge (and therefore has no clean
    /// exit code at all — only "did not exit normally with the success value").
    expected: Option<i64>,
    /// The value the program *would* return if an overflowing allocation wrongly
    /// succeeded. Only meaningful when `expected` is `None`; asserting the run does
    /// not produce it is what separates a real trap from a silently handed-out
    /// out-of-bounds pointer.
    value_if_overflow_succeeded: i64,
}

/// Generate one static-buffer arena program over a caller-owned `array<i64>`.
///
/// The shape: declare a buffer of `len` cells, open an arena over it, then perform
/// `count` single-cell allocations, writing a distinct value into each and summing
/// them back through the returned pointers. Summing through the *returned pointers*
/// (rather than re-indexing the buffer) is what makes a non-advancing bump visible:
/// if two allocations aliased, later writes would clobber earlier ones and the sum
/// would be wrong.
///
/// # Why every program first calls `dirty`
///
/// This is load-bearing, and was added because the generator **provably lacked
/// teeth without it**. The arena's bump cursor is a frame word, and the prologue
/// zeroes it (`emit_arena_cursor_init`). But a process's initial stack pages are
/// already zero, so an arena function called directly from a fresh `main` starts
/// with a zero cursor *whether or not the compiler zeroes it* — and deleting the
/// zeroing entirely still passed this fuzzer.
///
/// `dirty` fixes that: it fills its own frame with large non-zero values and
/// returns, so the arena function's frame reuses that dirtied stack and its cursor
/// word holds garbage. The zeroing then becomes observable — and with it removed,
/// the generated programs really do fail (see the test's teeth record).
fn gen_arena_program(seed: u64) -> ArenaCase {
    let mut rng = Rng(seed ^ 0x5C4A_7E19_B03D_A6F1u64);
    let len = rng.range(1, 6);
    // Straddle the capacity: `count` is usually within the buffer, but one time in
    // three it is exactly `len + 1` — one cell too many. `count == len` (an exact
    // fit) is a frequent, deliberate case: it is the boundary the range check must
    // get right.
    let overflows = rng.chance(3);
    let count = if overflows {
        len + 1
    } else {
        rng.range(1, len)
    };

    let buffer = (0..len)
        .map(|_| "0".to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut body = String::new();
    let mut values = Vec::new();
    for k in 0..count {
        // Distinct per-cell values, so aliasing changes the sum.
        let v = rng.range(1, 40);
        values.push(v);
        body.push_str(&format!(
            "        let p{k} ptr<i64> = arena_alloc(pool, 1)\n        ptr_write(p{k}, {v})\n"
        ));
    }
    let sum_expr = (0..count)
        .map(|k| format!("ptr_read(p{k})"))
        .collect::<Vec<_>>()
        .join(" + ");
    let total: i64 = values.iter().sum();

    // Fill a frame with large non-zero words so the arena function below reuses a
    // DIRTY stack rather than the process's zeroed initial pages. Without this the
    // bump cursor would read zero by luck and the prologue's zeroing would be
    // untestable — see the function docs.
    let dirt = (0..8)
        .map(|k| (1_000_000 + k * 7919).to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let source = format!(
        "no-runtime\n\n\
         fn dirty -> i64\n    \
             let d array<i64> = [{dirt}]\n    \
             let t i64 = 0\n    \
             let i i64 = 0\n    \
             while i < 8\n        \
                 t += d[i]\n        \
                 i += 1\n    \
             t\n\n\
         fn use_arena -> i64\n    \
             let buf array<i64> = [{buffer}]\n    \
             region pool in buf\n    \
             unsafe\n{body}        {sum_expr}\n\n\
         fn main -> i64\n    \
             let d i64 = dirty()\n    \
             let r i64 = use_arena()\n    \
             r\n"
    );

    ArenaCase {
        source,
        expected: if overflows { None } else { Some(total) },
        value_if_overflow_succeeded: total,
    }
}

/// **THE oracle for the static-buffer arena.** Compile each generated program to a
/// real `.exe` and check its exit code against *both* the generator's independently
/// computed result and the interpreters' — the arena has four-tier parity, so
/// native must agree with the other three as well as with the arithmetic.
///
/// What this catches, each as a wrong exit code rather than a subtle difference:
///
/// * a bump that does not advance (two allocations aliasing -> wrong sum),
/// * a cursor that starts from garbage rather than zero (`emit_arena_cursor_init`),
/// * a mis-scaled element address (`lea rax, [rax + rcx*8]`),
/// * an off-by-one range check — an exactly-full buffer must succeed, and one cell
///   past it must trap.
///
/// # Teeth: measured, not assumed
///
/// Two independent bug injections were built and run against this exact generator,
/// then reverted. Both make it FAIL, and it passes against the real implementation.
///
/// 1. **Cursor zeroing removed** (the body of `emit_arena_cursor_init` deleted, so
///    the bump cursor starts at whatever the frame slot held): the generated
///    programs exit `0xC000001D` — the stale cursor inherited from `dirty`'s frame
///    exceeds the buffer length, so the range check trips on an allocation that
///    should have succeeded.
///
///    **This injection is why `dirty` exists.** The first version of this generator
///    called the arena function straight from `main` and **passed with the zeroing
///    deleted** — a process's initial stack pages are already zero, so the cursor
///    read zero by luck. The generator had no teeth for the very thing it was
///    written to protect. Dirtying the stack first is what makes the zeroing
///    observable; the reproducer was verified by hand end-to-end (correct code
///    returns 7, injected code traps).
///
/// 2. **The overflow check weakened from `ja` to `jae`** — the classic off-by-one,
///    which rejects a legal exactly-full buffer: the exact-fit programs trap where
///    the oracle expects a real sum.
///
/// Gated on the link toolchain; skips cleanly when absent.
#[test]
fn fuzz_arena_native_matches_the_value_oracle() {
    if !native_exe_runnable() {
        eprintln!("native exe not runnable here; skipping arena native fuzz");
        return;
    }
    const PROGRAMS: u64 = 300;
    let base_seed = 0x00A2_E5A0_5EED_1C37u64;
    let dir = ScratchDir::new("arena");

    // Counts what ACTUALLY executed, so the batch cannot silently do nothing and
    // still pass green (asserted after the loop).
    let mut ran = 0u64;
    let mut overflow_cases = 0u64;
    let mut success_cases = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let case = gen_arena_program(seed);
        let Some(exit) = fuzz_native_exit(&case.source, &dir, &format!("arena_{i}")) else {
            continue;
        };
        ran += 1;
        match case.expected {
            Some(expected) => {
                success_cases += 1;
                assert_eq!(
                    i64::from(exit),
                    expected,
                    "arena value mismatch on program #{i} (seed {seed:#x}): the arena must \
                     hand out distinct, correctly-scaled cells from a zeroed cursor\n{}",
                    case.source
                );
                // The differential half: native must also agree with the tier the
                // author is most likely to have tested against.
                let (ast, _, _) = run_interpreters(&case.source);
                assert_eq!(
                    ast,
                    Outcome::Value(i64::from(exit)),
                    "native and the interpreters disagree on arena program #{i} (seed \
                     {seed:#x})\n{}",
                    case.source
                );
            }
            None => {
                overflow_cases += 1;
                assert_ne!(
                    i64::from(exit),
                    case.value_if_overflow_succeeded,
                    "program #{i} (seed {seed:#x}) requests one cell MORE than its buffer \
                     holds, so it must hit the overflow edge. Producing the success value \
                     would mean the arena handed out a pointer past the buffer's end — the \
                     silent wrong answer the edge exists to prevent\n{}",
                    case.source
                );
                assert_ne!(
                    exit, 0,
                    "an arena overflow must not exit cleanly on program #{i} (seed \
                     {seed:#x})\n{}",
                    case.source
                );
            }
        }
    }

    // A generator that produced nothing, or that never straddled the capacity in
    // either direction, would pass vacuously. Assert both halves really ran.
    assert!(ran > 0, "arena fuzz executed no programs");
    assert!(
        success_cases > 0,
        "arena fuzz never exercised a fitting allocation ({ran} ran)"
    );
    assert!(
        overflow_cases > 0,
        "arena fuzz never exercised the overflow edge ({ran} ran)"
    );
}

/// The **differential** oracle, now that the arena has four-tier parity: the three
/// interpreters must agree with each other on every generated program.
///
/// Always runs — no link toolchain needed — so this is the arena's cross-engine net
/// even on a machine where the native fuzzer skips. The overflow programs are
/// expected to abort (`L0460`), and the three engines must agree on *that* too:
/// `Outcome::Error` on one tier and a value on another would itself be a finding.
#[test]
fn fuzz_arena_interpreters_agree() {
    const PROGRAMS: u64 = 400;
    let base_seed = 0x00A2_E5A0_5EED_1C37u64;
    let mut overflow_cases = 0u64;
    let mut success_cases = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let case = gen_arena_program(seed);
        let (ast, ir, bc) = run_interpreters(&case.source);
        assert!(
            ast == ir && ir == bc,
            "arena backend divergence on program #{i} (seed {seed:#x}):\n{}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}",
            case.source
        );
        match case.expected {
            Some(expected) => {
                success_cases += 1;
                assert_eq!(
                    ast,
                    Outcome::Value(expected),
                    "arena value mismatch on the interpreters, program #{i} (seed \
                     {seed:#x})\n{}",
                    case.source
                );
            }
            None => {
                overflow_cases += 1;
                assert_eq!(
                    ast,
                    Outcome::Error,
                    "program #{i} (seed {seed:#x}) requests one cell MORE than its buffer \
                     holds, so the interpreters must abort (`L0460`) rather than return a \
                     value\n{}",
                    case.source
                );
            }
        }
    }
    assert!(
        success_cases > 0 && overflow_cases > 0,
        "arena interpreter fuzz must exercise both a fitting allocation and the overflow \
         edge (fit={success_cases}, overflow={overflow_cases})"
    );
}

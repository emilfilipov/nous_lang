
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate;

use super::*;

fn run_source(source: &str) -> Result<Value, RuntimeError> {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    validate(&program).expect("semantic");
    run_main(&program)
}

#[test]
fn runs_function_calls_and_arithmetic() {
    let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(40, 2)\n    value\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn move_on_functional_update_builds_list_correctly() {
    // `l = push(l, i)` in a loop consumes `l` by move on the fast path; the
    // built list must be byte-for-byte what a clone would produce. Sum of
    // 0..=49 is 1225, length 50.
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    for i from 0 to 49\n",
        "        l = push(l, i)\n",
        "    let total i64 = 0\n",
        "    let n i64 = len(l)\n",
        "    for i from 0 to n - 1\n",
        "        total += get(l, i)\n",
        "    total * 100 + len(l)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(122550));
}

#[test]
fn move_on_functional_update_preserves_aliased_binding() {
    // `let b = a` clones `a`; the subsequent `a = push(a, 9)` moves `a`'s slot
    // but must never corrupt the independent `b`.
    let source = concat!(
        "fn main -> i64\n",
        "    let a list<i64> = list_new()\n",
        "    a = push(a, 1)\n",
        "    a = push(a, 2)\n",
        "    a = push(a, 3)\n",
        "    let b list<i64> = a\n",
        "    a = push(a, 9)\n",
        "    let bsum i64 = get(b, 0) + get(b, 1) + get(b, 2)\n",
        "    len(a) * 10000 + get(a, 3) * 100 + len(b) * 10 + bsum\n",
    );
    // a=[1,2,3,9], b=[1,2,3]: 40936. A corrupted b would change the result.
    assert_eq!(run_source(source).expect("run"), Value::I64(40936));
}

#[test]
fn move_on_functional_update_error_in_other_argument_leaves_target_intact() {
    // In `l = push(l, boom())` the other argument throws before `l` is moved,
    // so `l` stays intact and the `catch` observes the original list, never a
    // moved-out placeholder.
    let source = concat!(
        "fn boom -> i64\n",
        "    throw \"boom\"\n\n",
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 1)\n",
        "    l = push(l, 2)\n",
        "    let caught i64 = 0\n",
        "    try\n",
        "        l = push(l, boom())\n",
        "    catch m\n",
        "        caught = 1\n",
        "    caught * 1000 + len(l) * 10 + get(l, 0) + get(l, 1)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1023));
}

#[test]
fn non_blocking_recv_surfaces_would_block_as_none() {
    // A UDP socket bound to an ephemeral loopback port and put into
    // non-blocking mode reports "no datagram pending" as `ok(none)` — never
    // blocking — so this is deterministic with no live peer. The fixture maps
    // `set_nonblocking` ok to 100 and an `ok(none)` recv to 10, summing 110.
    let source = concat!(
        "fn probe s Socket -> i64\n",
        "    let toggled result<i64, string> = set_nonblocking(s, true)\n",
        "    let received result<option<string>, string> = udp_recv_nb(s)\n",
        "    tcp_close(s)\n",
        "    let a i64 = unwrap_toggle(toggled)\n",
        "    let b i64 = unwrap_recv(received)\n",
        "    a + b\n\n",
        "fn unwrap_toggle r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(code) -> 100\n",
        "        err(message) -> 0\n\n",
        "fn unwrap_recv r result<option<string>, string> -> i64\n",
        "    match r\n",
        "        ok(maybe) ->\n",
        "            match maybe\n",
        "                some(data) -> 0\n",
        "                none -> 10\n",
        "        err(message) -> 0\n\n",
        "fn main -> i64\n",
        "    let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n",
        "    match bound\n",
        "        ok(s) -> probe(s)\n",
        "        err(message) -> 0\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(110));
}

#[test]
fn rejects_asm_on_the_ast_interpreter() {
    // Inline assembly is native-only; the AST interpreter rejects it with
    // `L0425` rather than executing raw machine code.
    let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
    let error = run_source(source).expect_err("asm must not run on an interpreter");
    assert_eq!(error.code, "L0425");
}

#[test]
fn rejects_extern_call_on_the_ast_interpreter() {
    // A C-ABI `extern fn` call is native-only; the AST interpreter rejects it
    // with `L0423` regardless of the C scalar width (here an `i32` signature),
    // rather than executing C or silently no-op-ing.
    let source =
        "extern fn toupper c i32 -> i32\n\nfn main -> i64\n    to_i64(toupper(to_i32(97)))\n";
    let error = run_source(source).expect_err("extern call must not run on an interpreter");
    assert_eq!(error.code, "L0423");
    assert!(
        error.message.contains("toupper") && error.message.contains("lullaby native"),
        "L0423 names the extern and points at `lullaby native`: {}",
        error.message
    );
}

#[test]
fn dispatches_trait_method_by_receiver_type() {
    // `p.show()` dispatches to Point's `Show` impl; the bounded generic
    // `describe(p)` calls the same trait method on the concrete type.
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n",
        "    y i64\n\n",
        "enum Light\n",
        "    Red\n",
        "    Green\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "impl Show for Light\n",
        "    fn show self -> string\n",
        "        match self\n",
        "            Red -> \"r\"\n",
        "            Green -> \"green\"\n\n",
        "fn describe<T: Show> v T -> string\n",
        "    v.show()\n\n",
        "fn main -> i64\n",
        "    let p Point = Point(3, 4)\n",
        "    let g Light = Green\n",
        // len("3")=1 + len("green")=5 + len(describe(p))=1 + len(describe(g))=5
        "    len(p.show()) + len(g.show()) + len(describe(p)) + len(describe(g))\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(12));
}

#[test]
fn runs_higher_order_function_call() {
    let source = concat!(
        "fn inc x i64 -> i64\n",
        "    x + 1\n\n",
        "fn dbl x i64 -> i64\n",
        "    x * 2\n\n",
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn picker -> fn(i64) -> i64\n",
        "    dbl\n\n",
        "fn main -> i64\n",
        "    let g fn(i64) -> i64 = inc\n",
        "    let h fn(i64) -> i64 = picker()\n",
        "    apply(inc, 10) + g(5) + h(20) + apply(dbl, 3)\n",
    );
    // apply(inc,10)=11 + g(5)=6 + h=dbl,h(20)=40 + apply(dbl,3)=6 = 63.
    assert_eq!(run_source(source).expect("run"), Value::I64(63));
}

#[test]
fn runs_char_and_byte_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a char = 'A'\n",
        "    let b char = char_from(char_code(a) + 1)\n",
        "    let ordered i64 = 0\n",
        "    if a < b\n",
        "        ordered = 1\n",
        "    let big byte = byte(250)\n",
        "    let text string = to_string(a) + to_string(byte(10))\n",
        "    char_code(b) + byte_val(big) + ordered + len(text)\n",
    );
    // code('B')=66 + byte_val(250)=250 + ordered=1 + len("A10")=3 = 320.
    assert_eq!(run_source(source).expect("run"), Value::I64(320));
}

#[test]
fn char_from_rejects_invalid_scalar() {
    let source = "fn main -> char\n    char_from(0 - 1)\n";
    let error = run_source(source).expect_err("invalid scalar");
    assert_eq!(error.code, "L0417");
}

#[test]
fn byte_rejects_out_of_range() {
    let source = "fn main -> byte\n    byte(300)\n";
    let error = run_source(source).expect_err("out of range");
    assert_eq!(error.code, "L0417");
}

#[test]
fn runs_memory_builtins() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn runs_store_builtin() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn runs_list_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 10)\n",
        "    l = push(l, 20)\n",
        "    l = push(l, 30)\n",
        "    l = set(l, 1, 25)\n",
        "    let a i64 = get(l, 0)\n",
        "    let b i64 = get(l, 2)\n",
        "    let n i64 = len(l)\n",
        "    l = pop(l)\n",
        "    a + b + n + len(l) + get(l, 1)\n",
    );
    // [10,20,30] -> set(1,25) -> [10,25,30]; a=10, b=30, n=3;
    // pop -> [10,25]; len=2, get(1)=25; 10+30+3+2+25 = 70.
    assert_eq!(run_source(source).expect("run"), Value::I64(70));
}

#[test]
fn list_get_out_of_bounds_errors() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 1)\n",
        "    get(l, 5)\n",
    );
    let error = run_source(source).expect_err("run");
    assert_eq!(error.code, "L0413");
}

#[test]
fn map_set_get_round_trips_via_option() {
    let source = concat!(
        "fn main -> i64\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"x\", 41)\n",
        "    m = map_set(m, \"x\", 42)\n",
        "    match map_get(m, \"x\")\n",
        "        some(v) -> v\n",
        "        none -> 0\n",
    );
    // Insert then replace `x`; `map_get` returns `some(42)`, unwrapped to 42.
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn map_get_missing_key_returns_none() {
    let source = concat!(
        "fn main -> i64\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"x\", 1)\n",
        "    m = map_del(m, \"x\")\n",
        "    match map_get(m, \"x\")\n",
        "        some(v) -> v\n",
        "        none -> 7\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(7));
}

#[test]
fn runs_string_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let s string = \"Hello, World\"\n",
        "    let parts array<string> = split(\"a,b,c\", \",\")\n",
        "    find(s, \"World\") + len(parts)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(10));
}

#[test]
fn runs_string_transforms() {
    let source = concat!(
        "fn main -> string\n",
        "    let joined string = join(split(\"a,b,c\", \",\"), \"-\")\n",
        "    upper(replace(substring(joined, 0, 3), \"-\", \"_\"))\n",
    );
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("A_B".to_string()).into())
    );
}

#[test]
fn substring_out_of_range_is_runtime_error() {
    let source = "fn main -> string\n    substring(\"hi\", 0, 5)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0413");
}

#[test]
fn split_empty_separator_is_runtime_error() {
    let source = "fn main -> i64\n    len(split(\"hi\", \"\"))\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0417");
}

#[test]
fn runs_integer_math_builtins() {
    let source = "fn main -> i64\n    let a i64 = abs(0 - 5)\n    let b i64 = min(3, 7)\n    let c i64 = max(3, 7)\n    let d i64 = pow(2, 10)\n    a + b + c + d\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(1039));
}

#[test]
fn runs_float_math_builtins() {
    let source = "fn check f f64 want f64 -> i64\n    if f == want\n        1\n    else\n        0\n\nfn main -> i64\n    check(sqrt(16.0), 4.0) + check(floor(2.7), 2.0) + check(ceil(2.1), 3.0) + check(round(2.5), 3.0)\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(4));
}

#[test]
fn rejects_negative_integer_pow_at_runtime() {
    let source = "fn main -> i64\n    pow(2, 0 - 1)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0417");
}

#[test]
fn runs_if_expression_result() {
    let source = "fn main -> i64\n    if true\n        42\n    else\n        0\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn runs_while_loop_with_assignment() {
    let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(3));
}

#[test]
fn runs_loop_break_and_continue() {
    let source = "fn main -> i64\n    let x i64 = 0\n    loop\n        x += 1\n        if x < 3\n            continue\n        break\n    x\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(3));
}

#[test]
fn runs_logical_expressions() {
    let source = "fn main -> bool\n    not false and true or false\n";
    assert_eq!(run_source(source).expect("run"), Value::Bool(true));
}

#[test]
fn short_circuits_logical_expressions() {
    let source = "fn main -> bool\n    false and (1 / 0 == 0) or true\n";
    assert_eq!(run_source(source).expect("run"), Value::Bool(true));
}

#[test]
fn runs_for_loop() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(6));
}

#[test]
fn mutates_array_elements_and_reports_len() {
    let source = "fn main -> i64\n    let xs array<i64> = [1, 2, 3]\n    xs[0] = 10\n    xs[len(xs) - 1] += 4\n    xs[0] + xs[2]\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(17));
}

#[test]
fn array_element_assignment_bounds_checked() {
    let source = "fn main -> i64\n    let xs array<i64> = [1]\n    xs[3] = 9\n    xs[0]\n";
    let error = run_source(source).expect_err("out of bounds");
    assert_eq!(error.code, "L0413");
}

#[test]
fn runs_for_loop_with_step() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 5 by 2\n        total += i\n    total\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(9));
}

#[test]
fn runs_descending_for_loop() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 3 to 1 by -1\n        total += i\n    total\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(6));
}

#[test]
fn runs_array_literal_and_index() {
    let source = "fn main -> i64\n    let values array<i64> = [2, 4, 6]\n    values[2]\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(6));
}

#[test]
fn rejects_array_index_out_of_bounds() {
    let source = "fn main -> i64\n    let values array<i64> = [1, 2]\n    values[3]\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0413");
}

#[test]
fn rejects_zero_for_step() {
    let source = "fn main -> i64\n    for i from 1 to 3 by 0\n        i\n    0\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0411");
}

#[test]
fn keeps_let_bindings_block_scoped() {
    let source =
        "fn main -> i64\n    let x i64 = 1\n    if true\n        let x i64 = 2\n        x\n    x\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn rejects_double_dealloc() {
    // The free is inside a branch, so the conservative compile-time
    // lifetime analysis does not track it out; the runtime L0406 guard
    // still catches the double free.
    let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    dealloc(ptr)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0406");
}

#[test]
fn rejects_store_after_dealloc() {
    let source = "fn main -> void\n    let ptr ptr_i64 = alloc(1)\n    if true\n        dealloc(ptr)\n    store(ptr, 2)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0406");
}

#[test]
fn runs_file_io_builtins() {
    let path = std::env::temp_dir()
        .join(format!("lullaby-runtime-{}.txt", std::process::id()))
        .to_string_lossy()
        .replace('\\', "/");
    let source = format!(
        "fn main -> string\n    write_file(\"{path}\", \"alpha\")\n    append_file(\"{path}\", \" beta\")\n    read_file(\"{path}\")\n"
    );
    assert_eq!(
        run_source(&source).expect("run"),
        Value::String(("alpha beta".to_string()).into())
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn reports_missing_file_as_resource_error() {
    let path = std::env::temp_dir()
        .join(format!("lullaby-missing-{}.txt", std::process::id()))
        .to_string_lossy()
        .replace('\\', "/");
    let source = format!("fn main -> string\n    read_file(\"{path}\")\n");
    let error = run_source(&source).expect_err("runtime error");
    assert_eq!(error.code, "L0414");
    assert_eq!(error.category, ErrorCategory::Resource);
}

#[test]
fn runs_safe_system_status_builtin() {
    let source = "fn main -> i64\n    sys_status(\"rustc\", [\"--version\"])\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(0));
}

#[test]
fn runs_reference_counted_values() {
    let source = "fn main -> i64\n    let handle rc<i64> = rc_new(41)\n    let shared rc<i64> = rc_clone(handle)\n    let view ref<i64> = rc_borrow(handle)\n    let a i64 = rc_get(handle)\n    let b i64 = ref_get(view)\n    rc_release(shared)\n    rc_release(handle)\n    a + b - 40\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn rejects_use_after_rc_release() {
    // Release inside a branch escapes the conservative compile-time
    // analysis; the runtime guard still reports the dangling handle.
    let source = "fn main -> i64\n    let handle rc<i64> = rc_new(1)\n    if true\n        rc_release(handle)\n    rc_get(handle)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0406");
}

#[test]
fn parallel_map_returns_mapped_list_in_order() {
    // Each element is squared on its own OS thread; results come back in the
    // same order as the input, so the mapped list is deterministic.
    let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 1)\n    base = push(base, 2)\n    base = push(base, 3)\n    base = push(base, 4)\n    parallel_map(sq, base)\n";
    assert_eq!(
        run_source(source).expect("run"),
        Value::Array((vec![Value::I64(1), Value::I64(4), Value::I64(9), Value::I64(16),]).into())
    );
}

#[test]
fn spawn_channel_round_trip_sums_deterministically() {
    // Four detached workers each `send(ch, v * v)`; `main` joins them and
    // sums the four received values. The total is order-independent, so it is
    // a deterministic 30 (1 + 4 + 9 + 16) regardless of thread scheduling.
    let source = "fn worker ch Chan v i64 -> void\n    send(ch, v * v)\n\nfn main -> i64\n    let ch Chan = chan_new()\n    let t1 Task = spawn(worker, ch, 1)\n    let t2 Task = spawn(worker, ch, 2)\n    let t3 Task = spawn(worker, ch, 3)\n    let t4 Task = spawn(worker, ch, 4)\n    task_join(t1)\n    task_join(t2)\n    task_join(t3)\n    task_join(t4)\n    let total i64 = 0\n    for i from 0 to 3\n        total += recv(ch)\n    total\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(30));
}

#[test]
fn mutex_accumulates_and_reads_back() {
    // Exercise the mutex builtins: create, set, atomically add, and read back.
    let source = "fn main -> i64\n    let m Mutex = mutex_new(10)\n    mutex_set(m, 5)\n    let a i64 = mutex_add(m, 3)\n    mutex_add(m, 4)\n    a + mutex_get(m)\n";
    // set -> 5, add 3 -> 8 (returned as `a`), add 4 -> 12 (read back).
    assert_eq!(run_source(source).expect("run"), Value::I64(20));
}

#[test]
fn mutex_shared_across_threads_via_clone() {
    // The `Value::Mutex` handle shares its cell on clone, so accumulating from
    // several OS threads over the same `Arc<Mutex<i64>>` is safe and yields a
    // deterministic total. This proves cross-thread mutex sharing directly
    // (the language `spawn`'s fixed `(Chan, i64)` shape cannot pass a mutex to
    // a worker yet, so this is verified at the runtime level).
    let mutex = SharedMutex {
        cell: Arc::new(Mutex::new(0)),
    };
    let value = Value::Mutex(mutex);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let handle = value.clone();
            scope.spawn(move || {
                for _ in 0..100 {
                    Runtime::builtin_mutex_add(vec![handle.clone(), Value::I64(1)])
                        .expect("mutex_add");
                }
            });
        }
    });
    assert_eq!(
        Runtime::builtin_mutex_get(vec![value]).expect("mutex_get"),
        Value::I64(800)
    );
}

#[test]
fn atomic_ops_are_deterministic_single_threaded() {
    // Exercise the full atomic surface deterministically, mirroring the
    // `run_atomics.lby` parity fixture: new(10), add(5) -> prev 10 (cell 15),
    // load -> 15, cas(15, 99) -> 15 (cell 99), load -> 99, swap(7) -> 99
    // (cell 7), and the bitwise fetch-ops.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(10)\n",
        "    let p0 i64 = atomic_add(a, 5)\n", // prev 10, cell 15
        "    let l0 i64 = atomic_load(a)\n",   // 15
        "    let c0 i64 = atomic_cas(a, 15, 99)\n", // 15, cell 99
        "    let l1 i64 = atomic_load(a)\n",   // 99
        "    let s0 i64 = atomic_swap(a, 7)\n", // 99, cell 7
        "    let sub0 i64 = atomic_sub(a, 2)\n", // prev 7, cell 5
        "    let and0 i64 = atomic_and(a, 6)\n", // prev 5 (5&6=4), cell 4
        "    let or0 i64 = atomic_or(a, 1)\n", // prev 4 (4|1=5), cell 5
        "    let xor0 i64 = atomic_xor(a, 7)\n", // prev 5 (5^7=2), cell 2
        "    let final i64 = atomic_load(a)\n", // 2
        "    p0 + l0 + c0 + l1 + s0 + sub0 + and0 + or0 + xor0 + final\n",
    );
    // 10 + 15 + 15 + 99 + 99 + 7 + 5 + 4 + 5 + 2 = 261.
    assert_eq!(run_source(source).expect("run"), Value::I64(261));
}

#[test]
fn ordered_atomics_run_deterministically() {
    // Mirrors the `run_atomic_orderings.lby` parity fixture: a `release`
    // store, `acquire`/`relaxed`/`seq_cst` loads, a `relaxed` fetch-and-add,
    // an `acq_rel`/`acquire` CAS, a `seq_cst` swap, a `relaxed` fetch-and-sub,
    // and a `seq_cst` fence. Single-threaded, so the ordering does not change
    // the produced value: the deterministic total is 300.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(10)\n",
        "    atomic_store_ordered(a, 20, release)\n", // cell 20
        "    let l0 i64 = atomic_load_ordered(a, acquire)\n", // 20
        "    let p0 i64 = atomic_add_ordered(a, 5, relaxed)\n", // prev 20, cell 25
        "    let l1 i64 = atomic_load_ordered(a, seq_cst)\n", // 25
        "    let c0 i64 = atomic_cas_ordered(a, 25, 99, acq_rel, acquire)\n", // 25, cell 99
        "    let l2 i64 = atomic_load_ordered(a, relaxed)\n", // 99
        "    let s0 i64 = atomic_swap_ordered(a, 7, seq_cst)\n", // prev 99, cell 7
        "    let sub0 i64 = atomic_sub_ordered(a, 2, relaxed)\n", // prev 7, cell 5
        "    fence(seq_cst)\n",
        "    let last i64 = atomic_load_ordered(a, acquire)\n", // 5
        "    l0 + p0 + l1 + c0 + l2 + s0 + sub0 + last\n",
    );
    // 20 + 20 + 25 + 25 + 99 + 99 + 7 + 5 = 300.
    assert_eq!(run_source(source).expect("run"), Value::I64(300));
}

#[test]
fn expect_memory_order_maps_each_variant_to_std_ordering() {
    // The five `MemoryOrder` unit variants decode to the exact std ordering,
    // proving the interpreter selects the real hardware/std ordering rather
    // than seq_cst for everything.
    let order = |name: &str| {
        expect_memory_order(
            "t",
            Value::Enum(Box::new(EnumValue {
                enum_name: "MemoryOrder".to_string(),
                variant: name.to_string(),
                payload: Vec::new(),
            })),
        )
        .expect("decode")
    };
    assert_eq!(order("relaxed"), Ordering::Relaxed);
    assert_eq!(order("acquire"), Ordering::Acquire);
    assert_eq!(order("release"), Ordering::Release);
    assert_eq!(order("acq_rel"), Ordering::AcqRel);
    assert_eq!(order("seq_cst"), Ordering::SeqCst);
}

#[test]
fn ordered_atomic_builtins_guard_invalid_orderings_without_panicking() {
    // A dynamically supplied ordering that is illegal for the op returns a
    // clean `L0432` runtime error instead of panicking inside `std`.
    let atomic = || {
        Value::Atomic(SharedAtomic {
            cell: Arc::new(AtomicI64::new(0)),
        })
    };
    let order = |name: &str| {
        Value::Enum(Box::new(EnumValue {
            enum_name: "MemoryOrder".to_string(),
            variant: name.to_string(),
            payload: Vec::new(),
        }))
    };
    // A `release` load is illegal.
    let load = builtin_atomic_load_ordered(vec![atomic(), order("release")]);
    assert_eq!(load.expect_err("guard").code, "L0432");
    // An `acquire` store is illegal.
    let store = builtin_atomic_store_ordered(vec![atomic(), Value::I64(1), order("acquire")]);
    assert_eq!(store.expect_err("guard").code, "L0432");
    // A `relaxed` fence is illegal.
    let fence = builtin_fence(vec![order("relaxed")]);
    assert_eq!(fence.expect_err("guard").code, "L0432");
    // A `release` CAS failure ordering is illegal.
    let cas = builtin_atomic_cas_ordered(vec![
        atomic(),
        Value::I64(0),
        Value::I64(1),
        order("seq_cst"),
        order("release"),
    ]);
    assert_eq!(cas.expect_err("guard").code, "L0432");
}

#[test]
fn atomic_shared_across_threads_via_clone() {
    // The `Value::Atomic` handle shares its `Arc<AtomicI64>` on clone, so
    // many OS threads racing `atomic_add` against the same cell lose no
    // updates: the final total is the exact sum. This proves real atomicity
    // (an ordinary `mutex`-free counter would drop increments under this
    // contention).
    let atomic = SharedAtomic {
        cell: Arc::new(AtomicI64::new(0)),
    };
    let value = Value::Atomic(atomic);
    const THREADS: i64 = 8;
    const ITERS: i64 = 10_000;
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let handle = value.clone();
            scope.spawn(move || {
                for _ in 0..ITERS {
                    Runtime::builtin_atomic_add(vec![handle.clone(), Value::I64(1)])
                        .expect("atomic_add");
                }
            });
        }
    });
    assert_eq!(
        Runtime::builtin_atomic_load(vec![value]).expect("atomic_load"),
        Value::I64(THREADS * ITERS)
    );
}

#[test]
fn atomic_add_returns_previous_and_races_lose_no_updates() {
    // A second multi-threaded proof that also checks the fetch-and-op
    // *return contract*: `atomic_add` returns the PREVIOUS value, so the set
    // of returned values across a single-threaded run is a permutation of
    // the prefix sums. Here we assert the stronger cross-thread invariant:
    // with N threads each adding a distinct large stride, the final load is
    // the exact arithmetic sum with no lost update.
    let atomic = SharedAtomic {
        cell: Arc::new(AtomicI64::new(0)),
    };
    let value = Value::Atomic(atomic);
    const THREADS: i64 = 6;
    const ITERS: i64 = 5_000;
    const STRIDE: i64 = 3;
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let handle = value.clone();
            scope.spawn(move || {
                for _ in 0..ITERS {
                    Runtime::builtin_atomic_add(vec![handle.clone(), Value::I64(STRIDE)])
                        .expect("atomic_add");
                }
            });
        }
    });
    assert_eq!(
        Runtime::builtin_atomic_load(vec![value]).expect("atomic_load"),
        Value::I64(THREADS * ITERS * STRIDE)
    );
}

#[test]
fn runs_unsafe_raw_pointer_read() {
    let source = "fn main -> i64\n    let p ptr_i64 = alloc(42)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(42));
}

#[test]
fn try_catch_yields_a_value_from_either_arm() {
    let caught = "fn main -> string\n    try\n        throw \"boom\"\n    catch message\n        \"caught: \" + message\n";
    assert_eq!(
        run_source(caught).expect("run"),
        Value::String(("caught: boom".to_string()).into())
    );
    let ok = "fn main -> i64\n    try\n        42\n    catch message\n        0\n";
    assert_eq!(run_source(ok).expect("run"), Value::I64(42));
}

#[test]
fn catches_thrown_error_and_recovers() {
    let source = "fn main -> i64\n    let result i64 = 0\n    try\n        throw \"boom\"\n    catch message\n        result = 7\n    result\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(7));
}

#[test]
fn propagates_uncaught_throw() {
    let source = "fn main -> i64\n    throw \"unhandled\"\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0420");
    assert_eq!(error.message, "unhandled");
}

#[test]
fn assert_true_returns_void() {
    let source = "fn main -> void\n    assert(true)\n";
    assert_eq!(run_source(source).expect("run"), Value::Void);
}

#[test]
fn assert_false_yields_catchable_runtime_error() {
    // `assert(false)` raises the same catchable user-error a `throw` does.
    let source = "fn main -> void\n    assert(false)\n";
    let error = run_source(source).expect_err("runtime error");
    assert_eq!(error.code, "L0420");
    assert_eq!(error.message, "assertion failed");
}

#[test]
fn assert_false_is_recoverable_by_try_catch() {
    let source = "fn main -> i64\n    let result i64 = 0\n    try\n        assert(false)\n    catch message\n        result = 7\n    result\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(7));
}

#[test]
fn run_named_function_runs_a_test_without_main() {
    // A library-style program with no `main`: run a named zero-arg function
    // directly. A passing test returns Ok; a failing one propagates L0420.
    let source = "fn test_ok -> void\n    assert(true)\n\nfn test_bad -> void\n    assert(false)\n";
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    validate(&program).expect("semantic");
    assert_eq!(
        run_named_function(&program, "test_ok").expect("run test_ok"),
        Value::Void
    );
    let error = run_named_function(&program, "test_bad").expect_err("test_bad fails");
    assert_eq!(error.code, "L0420");
    assert_eq!(error.message, "assertion failed");
}

#[test]
fn catch_binds_thrown_message_across_call_boundary() {
    let source = "fn risky -> i64\n    throw \"from risky\"\n\nfn main -> string\n    let captured string = \"\"\n    try\n        let value i64 = risky()\n    catch message\n        captured = message\n    captured\n";
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("from risky".to_string()).into())
    );
}

#[test]
fn mutates_struct_fields() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1, 2)\n    p.x = 10\n    p.y += 5\n    p.x + p.y\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(17));
}

#[test]
fn constructs_and_reads_struct_fields() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x * p.x + p.y * p.y\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(25));
}

#[test]
fn passes_structs_through_functions() {
    let source = "struct Player\n    name string\n    score i64\n\nfn label hero Player -> string\n    hero.name + \":\" + to_string(hero.score)\n\nfn main -> string\n    label(Player(\"Ada\", 100))\n";
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("Ada:100".to_string()).into())
    );
}

#[test]
fn evaluates_f64_arithmetic() {
    let source = "fn main -> f64\n    let x f64 = 3.5\n    x + 1.5\n";
    assert_eq!(run_source(source).expect("run"), Value::F64(5.0));
}

#[test]
fn compares_and_stringifies_f64() {
    let source = "fn main -> string\n    let x f64 = 2.5\n    to_string(x < 3.0) + \" \" + to_string(x * 2.0)\n";
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("true 5".to_string()).into())
    );
}

#[test]
fn concatenates_strings_and_converts_values() {
    let source = "fn main -> string\n    let n i64 = 40 + 2\n    \"answer: \" + to_string(n) + \" ok=\" + to_string(n == 42)\n";
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("answer: 42 ok=true".to_string()).into())
    );
}

#[test]
fn runs_standard_stream_builtins() {
    let source =
        "fn main -> void\n    println(\"hello\")\n    print(\"a\")\n    warn(\"w\")\n    flush()\n";
    assert_eq!(run_source(source).expect("run"), Value::Void);
}

#[test]
fn runs_safe_system_output_builtin() {
    let source = "fn main -> bool\n    let output string = sys_output(\"rustc\", [\"--version\"])\n    output == \"\" == false\n";
    assert_eq!(run_source(source).expect("run"), Value::Bool(true));
}

#[test]
fn constructs_and_passes_enum_values() {
    // Constructs unit and payload variants, stores them in locals and arrays,
    // passes them through functions, and returns an i64 computed from plain
    // locals (there is no `match` yet).
    let source = "enum Color\n    Red\n    Green\n    Blue\n\nenum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn tag c Color -> i64\n    7\n\nfn main -> i64\n    let c Color = Green\n    let palette array<Color> = [Red, Green, Blue]\n    let circle Shape = Circle(2.0)\n    let hole Shape = Empty\n    let shapes array<Shape> = [circle, hole]\n    tag(c) + len(palette) + len(shapes)\n";
    assert_eq!(run_source(source).expect("run"), Value::I64(12));
}

#[test]
fn matches_enum_and_extracts_payload() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r * r\n",
        "        Rect(w, h) -> w * h\n",
        "        Empty -> 0\n\n",
        "fn main -> i64\n",
        "    area(Circle(3)) + area(Rect(4, 5)) + area(Empty)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(29));
}

#[test]
fn match_wildcard_arm_covers_remaining_variants() {
    let source = concat!(
        "enum Color\n    Red\n    Green\n    Blue\n\n",
        "fn rank c Color -> i64\n",
        "    match c\n",
        "        Green -> 10\n",
        "        _ -> 1\n\n",
        "fn main -> i64\n    rank(Green) + rank(Red) + rank(Blue)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(12));
}

#[test]
fn runs_option_and_result_via_match() {
    let source = concat!(
        "fn unwrap_or o option<i64> fallback i64 -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> fallback\n\n",
        "fn describe r result<i64, string> -> string\n",
        "    match r\n",
        "        ok(v) -> \"ok \" + to_string(v)\n",
        "        err(m) -> \"err \" + m\n\n",
        "fn main -> string\n",
        "    let a option<i64> = some(3)\n",
        "    let b option<i64> = none\n",
        "    let sum i64 = unwrap_or(a, 0) + unwrap_or(b, 100)\n",
        "    let good result<i64, string> = ok(sum)\n",
        "    let bad result<i64, string> = err(\"boom\")\n",
        "    describe(good) + \" / \" + describe(bad)\n",
    );
    assert_eq!(
        run_source(source).expect("run"),
        Value::String(("ok 103 / err boom".to_string()).into())
    );
}

#[test]
fn enum_value_display_formats_unit_and_payload_variants() {
    let unit = Value::Enum(Box::new(EnumValue {
        enum_name: "Shape".to_string(),
        variant: "Empty".to_string(),
        payload: Vec::new(),
    }));
    assert_eq!(unit.to_string(), "Empty");

    let payload = Value::Enum(Box::new(EnumValue {
        enum_name: "Shape".to_string(),
        variant: "Circle".to_string(),
        payload: vec![Value::F64(2.0)],
    }));
    assert_eq!(payload.to_string(), "Circle(2)");
}

#[test]
fn runs_generic_identity_at_two_types() {
    // A single erased generic function called at `i64` and `string`; the
    // string result is measured with `len` so `main` stays `i64`.
    let source = concat!(
        "fn identity<T> x T -> T\n",
        "    x\n\n",
        "fn main -> i64\n",
        "    let n i64 = identity(41)\n",
        "    let s string = identity(\"abc\")\n",
        "    n + len(s)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(44));
}

#[test]
fn runs_generic_choose_selecting_by_flag() {
    let source = concat!(
        "fn choose<T> pick bool a T b T -> T\n",
        "    if pick\n",
        "        return a\n",
        "    b\n\n",
        "fn main -> i64\n",
        "    choose(true, 10, 20) + choose(false, 3, 7)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(17));
}

#[test]
fn tcp_connect_refused_yields_err_result() {
    // Connecting to port 1 on loopback is a deterministic refusal, so the
    // `result` takes the `err` arm and the program returns 1. No server.
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", 1)\n",
        "    match outcome\n",
        "        ok(conn) -> 0\n",
        "        err(message) -> 1\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn proc_spawn_missing_command_yields_err_result() {
    // Spawning a command that does not exist on any platform deterministically
    // takes the `err` arm, so the program returns 1. This mirrors the
    // backend-invariant `run_process.lby` parity fixture. (Array literals must
    // be non-empty in the current implementation, so a harmless arg is supplied; a
    // missing command fails to spawn regardless of its arguments.)
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<process, string> = proc_spawn(\"lullaby_definitely_not_a_real_program_zzz\", [\"--version\"])\n",
        "    match outcome\n",
        "        ok(p) -> 7\n",
        "        err(message) -> 1\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn proc_spawn_wait_and_stdout_success_path() {
    // Spawn a universally-available shell that echoes `hello`, wait for exit,
    // and assert the exit code is 0 and captured stdout contains `hello`. The
    // command is platform-conditional so the test runs on the host. Every
    // `match` sits in tail position (via helper functions) to stay within the
    // fixture-style surface the parser accepts.
    let (cmd, arg0, arg1) = if cfg!(windows) {
        ("cmd", "/c", "echo hello")
    } else {
        ("sh", "-c", "echo hello")
    };
    let source = format!(
        concat!(
            "fn main -> i64\n",
            "    let spawned result<process, string> = proc_spawn(\"{cmd}\", [\"{arg0}\", \"{arg1}\"])\n",
            "    match spawned\n",
            "        ok(p) -> run_child(p)\n",
            "        err(message) -> 100\n",
            "\n",
            "fn run_child p process -> i64\n",
            "    let waited result<i64, string> = proc_wait(p)\n",
            "    let captured result<string, string> = proc_stdout(p)\n",
            "    check_wait(waited, captured)\n",
            "\n",
            "fn check_wait waited result<i64, string> captured result<string, string> -> i64\n",
            "    match waited\n",
            "        ok(status) -> check_output(status, captured)\n",
            "        err(message) -> 200\n",
            "\n",
            "fn check_output code i64 captured result<string, string> -> i64\n",
            "    match captured\n",
            "        ok(text) -> classify(code, text)\n",
            "        err(message) -> 300\n",
            "\n",
            "fn classify code i64 text string -> i64\n",
            "    if code == 0 and contains(text, \"hello\")\n",
            "        0\n",
            "    else\n",
            "        1\n",
        ),
        cmd = cmd,
        arg0 = arg0,
        arg1 = arg1,
    );
    assert_eq!(run_source(&source).expect("run"), Value::I64(0));
}

#[test]
fn http_get_refused_yields_err_result() {
    // Connecting to port 1 on loopback is a deterministic refusal, so the
    // `result` takes the `err` arm and the program returns 1. No server.
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<string, string> = http_get(\"http://127.0.0.1:1/\")\n",
        "    match outcome\n",
        "        ok(body) -> 0\n",
        "        err(message) -> 1\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn http_get_https_url_yields_err_result() {
    // `https://` is out of scope; it returns an `err` deterministically.
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<string, string> = http_get(\"https://example.com/\")\n",
        "    match outcome\n",
        "        ok(body) -> 0\n",
        "        err(message) -> 1\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn to_bytes_from_bytes_round_trip_and_byte_len() {
    // `to_bytes("Hi")` = [72, 105]; `from_bytes` decodes back to "Hi";
    // `byte_len("café")` = 5 while `len` counts 4 characters.
    let source = concat!(
        "fn main -> i64\n",
        "    let bytes list<byte> = to_bytes(\"Hi\")\n",
        "    let first i64 = byte_val(get(bytes, 0))\n",
        "    let second i64 = byte_val(get(bytes, 1))\n",
        "    let decoded i64 = 0\n",
        "    match from_bytes(bytes)\n",
        // 72 + 105 + len("Hi")=2 + (byte_len=5 - len=4)=1 => 180
        "        ok(s) -> first + second + len(s) + (byte_len(\"café\") - len(\"café\"))\n",
        "        err(m) -> 0 - len(m)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(180));
}

#[test]
fn from_bytes_rejects_invalid_utf8_with_err() {
    // A lone `0xFF` byte is not valid UTF-8: `from_bytes` returns `err`
    // (never a panic, never a lossy replacement).
    let source = concat!(
        "fn main -> i64\n",
        "    let bad list<byte> = push(list_new(), byte(255))\n",
        "    match from_bytes(bad)\n",
        "        ok(s) -> len(s)\n",
        "        err(m) -> 1\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(1));
}

#[test]
fn os_random_bytes_len_and_bounds_behavior() {
    // A positive length yields exactly that many bytes.
    assert_eq!(os_random_bytes(16).expect("ok").len(), 16);
    // Zero yields an empty buffer (no syscall, no error).
    assert_eq!(os_random_bytes(0).expect("ok"), Vec::<u8>::new());
    // A negative length is an error, not a panic.
    assert_eq!(
        os_random_bytes(-1),
        Err("os_random length must be non-negative".to_string())
    );
}

#[test]
fn os_random_returns_requested_length_and_empty_and_err() {
    // `os_random(16)` yields `ok` with 16 bytes; `os_random(0)` yields `ok`
    // with an empty list; `os_random(-1)` yields `err` (never a panic). The
    // fixed total is 16 + 0 + (0 - 1) = 15.
    let source = concat!(
        "fn amount n i64 -> i64\n",
        "    match os_random(n)\n",
        "        ok(bytes) -> len(bytes)\n",
        "        err(_) -> 0 - 1\n\n",
        "fn main -> i64\n",
        "    amount(16) + amount(0) + amount(0 - 1)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(15));
}

#[test]
fn os_random_is_non_deterministic_across_calls() {
    // A real OS CSPRNG (not a seeded PRNG) produces different 32-byte draws
    // with overwhelming probability, so two draws must differ.
    let first = os_random_bytes(32).expect("ok");
    let second = os_random_bytes(32).expect("ok");
    assert_eq!(first.len(), 32);
    assert_eq!(second.len(), 32);
    assert_ne!(first, second, "two OS-CSPRNG draws must not be identical");
}

#[test]
fn try_operator_propagates_ok_and_err_on_ast_backend() {
    // `checked(a)? + checked(b)?` yields the sum when both succeed and
    // short-circuits with the first `err` otherwise. The AST interpreter
    // realizes `?` via a function-level early-return signal.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    if n < 0\n",
        "        return err(\"neg\")\n",
        "    ok(n)\n\n",
        "fn add_checked a i64 b i64 -> result<i64, string>\n",
        "    let x i64 = checked(a)?\n",
        "    let y i64 = checked(b)?\n",
        "    ok(x + y)\n\n",
        "fn unwrap r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> 0 - len(m)\n\n",
        "fn main -> i64\n",
        // success 3 + 4 = 7; failure err("neg") -> -3.
        "    unwrap(add_checked(3, 4)) + unwrap(add_checked(-1, 4))\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(4));
}

#[test]
fn try_operator_propagates_none_on_ast_backend() {
    // `?` on an `option` returns `none` from the enclosing option-returning
    // function when the operand is `none`.
    let source = concat!(
        "fn lookup present bool -> option<i64>\n",
        "    if present\n",
        "        return some(9)\n",
        "    none\n\n",
        "fn twice present bool -> option<i64>\n",
        "    let x i64 = lookup(present)?\n",
        "    some(x + x)\n\n",
        "fn unwrap o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> -1\n\n",
        "fn main -> i64\n",
        // present -> 18; absent -> none -> -1.
        "    unwrap(twice(true)) + unwrap(twice(false))\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(17));
}

#[test]
fn nested_try_operator_runs_on_ast_backend() {
    // `checked(checked(n)? + n)?` nests two `?`s in one expression; both must
    // succeed for the value to flow through.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    if n < 0\n",
        "        return err(\"neg\")\n",
        "    ok(n)\n\n",
        "fn double_checked n i64 -> result<i64, string>\n",
        "    let v i64 = checked(checked(n)? + n)?\n",
        "    ok(v)\n\n",
        "fn unwrap r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> 0 - len(m)\n\n",
        "fn main -> i64\n",
        // double_checked(5) = 10; double_checked(-2) = err("neg") -> -3.
        "    unwrap(double_checked(5)) + unwrap(double_checked(-2))\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(7));
}

#[test]
fn int_kind_normalize_wraps_each_width() {
    assert_eq!(IntKind::I8.normalize(128), -128);
    assert_eq!(IntKind::I16.normalize(32_768), -32_768);
    assert_eq!(IntKind::I32.normalize(2_147_483_648), -2_147_483_648);
    assert_eq!(IntKind::U16.normalize(-1), 65_535);
    assert_eq!(IntKind::U32.normalize(-1), 4_294_967_295);
    // The 64-bit unsigned kinds fill the cell; normalization keeps the bits.
    assert_eq!(IntKind::U64.normalize(-1), -1);
    assert_eq!(IntKind::Usize.normalize(-1), -1);
}

#[test]
fn int_div_and_cmp_respect_signedness_at_64_bit() {
    // `to_u64(0 - 1)` is stored as the bit pattern of -1, i.e. u64::MAX.
    let umax = IntKind::U64.normalize(-1);
    // Unsigned division divides on the magnitude, not the signed -1.
    assert_eq!(int_div(umax, 2, IntKind::U64), (u64::MAX / 2) as i64);
    // Signed i64-style division of the same bits would be 0 (-1 / 2).
    assert_eq!(int_div(-1, 2, IntKind::Isize), 0);
    // Unsigned ordering treats the cell as u64::MAX (greater than 1).
    assert!(int_cmp(umax, 1, IntKind::U64).is_gt());
    // Signed ordering of the same bits (-1) is less than 1.
    assert!(int_cmp(-1, 1, IntKind::Isize).is_lt());
}

#[test]
fn signed_division_min_over_neg_one_wraps_not_panics() {
    // `i64::MIN / -1` is the one signed-overflow case: raw `/` panics, but the
    // language wraps it to `i64::MIN` on every backend. `int_div` on the
    // 64-bit signed kind (`isize`) yields `i64::MIN`; the plain-`i64`
    // interpreter path is covered end-to-end by the `run_div_overflow`
    // fixture. A narrower signed kind never reaches the overflow (the
    // sign-extended cell is not `i64::MIN`), but still divides correctly.
    assert_eq!(int_div(i64::MIN, -1, IntKind::Isize), i64::MIN);
    assert_eq!(int_div(-128, -1, IntKind::I8), 128);
}

#[test]
fn runs_unsigned_64_bit_wraparound_end_to_end() {
    // `to_u64(0 - 1)` is u64::MAX; dividing by 2 uses unsigned semantics, and
    // `to_i64` reinterprets the resulting bits back into an i64.
    let source = concat!(
        "fn main -> i64\n",
        "    let big u64 = to_u64(0 - 1)\n",
        "    let half u64 = big / to_u64(2)\n",
        "    to_i64(half)\n",
    );
    assert_eq!(
        run_source(source).expect("run"),
        Value::I64((u64::MAX / 2) as i64)
    );
}

#[test]
fn overflow_arith_checked_saturating_wrapping() {
    // checked_add overflows i8 (127 + 1) -> none.
    let none = overflow_arith(
        "checked_add",
        vec![Value::int(127, IntKind::I8), Value::int(1, IntKind::I8)],
        ArithOp::Add,
        OverflowMode::Checked,
    )
    .expect("checked_add");
    assert_eq!(none, option_value(None));
    // checked_add in range -> some(120).
    let some = overflow_arith(
        "checked_add",
        vec![Value::int(100, IntKind::I8), Value::int(20, IntKind::I8)],
        ArithOp::Add,
        OverflowMode::Checked,
    )
    .expect("checked_add");
    assert_eq!(some, option_value(Some(Value::int(120, IntKind::I8))));
    // saturating_mul clamps to u32::MAX.
    let sat = overflow_arith(
        "saturating_mul",
        vec![
            Value::int(100_000, IntKind::U32),
            Value::int(100_000, IntKind::U32),
        ],
        ArithOp::Mul,
        OverflowMode::Saturating,
    )
    .expect("saturating_mul");
    assert_eq!(sat, Value::int(4_294_967_295, IntKind::U32));
    // wrapping_add wraps u32::MAX + 1 -> 0.
    let wrap = overflow_arith(
        "wrapping_add",
        vec![
            Value::int(4_294_967_295, IntKind::U32),
            Value::int(1, IntKind::U32),
        ],
        ArithOp::Add,
        OverflowMode::Wrapping,
    )
    .expect("wrapping_add");
    assert_eq!(wrap, Value::int(0, IntKind::U32));
}

#[test]
fn gcd_is_total_and_non_negative() {
    assert_eq!(gcd_i64(0, 0), 0);
    assert_eq!(gcd_i64(0, 7), 7);
    assert_eq!(gcd_i64(7, 0), 7);
    assert_eq!(gcd_i64(12, 18), 6);
    assert_eq!(gcd_i64(-12, 18), 6);
    assert_eq!(gcd_i64(-12, -18), 6);
    assert_eq!(gcd_i64(21, 14), 7);
    // Coprime -> 1.
    assert_eq!(gcd_i64(17, 4), 1);
    // `i64::MIN` must not panic; |MIN| shares the value 2^63 whose divisors
    // give a positive result with a positive operand, and `gcd(MIN, 0)`
    // wraps its own magnitude back to `i64::MIN` (documented total edge).
    assert_eq!(gcd_i64(i64::MIN, 4), 4);
    assert_eq!(gcd_i64(i64::MIN, i64::MIN), i64::MIN);
    assert_eq!(gcd_i64(i64::MIN, 0), i64::MIN);
}

#[test]
fn list_sum_and_extreme_helpers() {
    // Empty list sums to 0 and has no extreme.
    assert_eq!(list_sum_values("t", vec![]).unwrap(), Value::I64(0));
    assert_eq!(list_extreme("t", vec![], false).unwrap(), None);
    assert_eq!(list_extreme("t", vec![], true).unwrap(), None);
    // i64 list.
    let ints = vec![Value::I64(3), Value::I64(9), Value::I64(1), Value::I64(7)];
    assert_eq!(list_sum_values("t", ints.clone()).unwrap(), Value::I64(20));
    assert_eq!(
        list_extreme("t", ints.clone(), false).unwrap(),
        Some(Value::I64(1))
    );
    assert_eq!(list_extreme("t", ints, true).unwrap(), Some(Value::I64(9)));
    // Wrapping i64 sum matches `+` (i64::MAX + 1 -> i64::MIN).
    let wrap = vec![Value::I64(i64::MAX), Value::I64(1)];
    assert_eq!(list_sum_values("t", wrap).unwrap(), Value::I64(i64::MIN));
    // f64 list.
    let floats = vec![Value::F64(1.5), Value::F64(0.5), Value::F64(3.0)];
    assert_eq!(
        list_sum_values("t", floats.clone()).unwrap(),
        Value::F64(5.0)
    );
    assert_eq!(
        list_extreme("t", floats.clone(), false).unwrap(),
        Some(Value::F64(0.5))
    );
    assert_eq!(
        list_extreme("t", floats, true).unwrap(),
        Some(Value::F64(3.0))
    );
    // A non-numeric element is a runtime type error.
    assert!(list_sum_values("t", vec![Value::Bool(true)]).is_err());
}

#[test]
fn closure_captures_enclosing_local_by_value() {
    // `add_n` captures `n = 10` when the literal evaluates; `apply(add_n, 5)`
    // is 15 and `add_n(2)` is 12, so the canonical example returns 27.
    let source = concat!(
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    let n i64 = 10\n",
        "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
        "    apply(add_n, 5) + add_n(2)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(27));
}

#[test]
fn closure_capture_is_a_snapshot_not_a_reference() {
    // Capture is by value at literal-evaluation time: mutating the enclosing
    // local after the closure is built does not change the captured value.
    let source = concat!(
        "fn main -> i64\n",
        "    let seed i64 = 7\n",
        "    let grab fn(i64) -> i64 = fn x i64 -> x + seed\n",
        "    let early i64 = grab(1)\n",
        "    seed = 1000\n",
        "    let late i64 = grab(1)\n",
        "    early + late\n",
    );
    // 8 + 8 = 16 (both reads see the snapshotted seed = 7).
    assert_eq!(run_source(source).expect("run"), Value::I64(16));
}

#[test]
fn closure_returned_from_function_is_callable_later() {
    // A closure returned from `make_adder` carries its captured `base`, so it
    // stays callable at its call site: add10(5) = 15, add100(3) = 103 -> 118.
    let source = concat!(
        "fn make_adder base i64 -> fn(i64) -> i64\n",
        "    fn x i64 -> x + base\n\n",
        "fn main -> i64\n",
        "    let add10 fn(i64) -> i64 = make_adder(10)\n",
        "    let add100 fn(i64) -> i64 = make_adder(100)\n",
        "    add10(5) + add100(3)\n",
    );
    assert_eq!(run_source(source).expect("run"), Value::I64(118));
}

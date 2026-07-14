use lullaby_lexer::lex;
use lullaby_parser::parse;

use super::*;

fn validate_source(source: &str) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    validate(&program)
}

#[test]
fn non_void_function_may_return_last_expression() {
    assert!(validate_source("fn add x i64 y i64 -> i64\n    x + y\n").is_ok());
}

#[test]
fn accepts_i64_bitwise_operators() {
    // `& | ^ << >>` and unary `~` are all `i64 -> i64`.
    let source = concat!(
        "fn main -> i64\n",
        "    let a i64 = 6 & 3\n",
        "    let b i64 = 6 | 1\n",
        "    let c i64 = 6 ^ 3\n",
        "    let d i64 = 1 << 4\n",
        "    let e i64 = 64 >> 2\n",
        "    let f i64 = ~0\n",
        "    a + b + c + d + e + f\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_non_i64_bitwise_operand() {
    // A `bool` operand to a bitwise op reuses the arithmetic operand family
    // (`L0307`); bitwise ops require two operands of the same integer type.
    let source = "fn main -> i64\n    let x bool = true\n    x & 1\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "expected L0307 for a non-integer bitwise operand: {diagnostics:?}"
    );
}

#[test]
fn accepts_fixed_width_bitwise_operators() {
    // Bitwise ops (& | ^ << >> ~) apply to any integer width and preserve it;
    // both operands must share the type (no width mixing).
    let source = concat!(
        "fn main -> i64\n",
        "    let a u8 = to_u8(200)\n",
        "    let b u8 = (a & to_u8(15)) | (a << to_u8(1))\n",
        "    let c i32 = ~to_i32(5)\n",
        "    let d i32 = c >> to_i32(1)\n",
        "    to_i64(b) + to_i64(d)\n",
    );
    validate_source(source).expect("fixed-width bitwise operators type-check");
}

#[test]
fn rejects_mixed_width_bitwise_operands() {
    // A u8 and an i32 cannot share a bitwise op.
    let source = "fn main -> i64\n    let x u8 = to_u8(1)\n    let y i32 = to_i32(1)\n    to_i64(x & to_u8(0)) + to_i64(y)\n";
    // The valid form above must pass; the mixed form below must fail.
    validate_source(source).expect("same-width bitwise is fine");
    let bad =
        "fn main -> i64\n    let x u8 = to_u8(1)\n    let y i32 = to_i32(1)\n    to_i64(x & y)\n";
    let diagnostics = validate_source(bad).expect_err("u8 & i32 must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "expected L0307 for mixed-width bitwise operands: {diagnostics:?}"
    );
}

#[test]
fn rejects_non_i64_bitwise_not_operand() {
    let source = "fn main -> i64\n    let x f64 = 1.0\n    ~x\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "expected L0307 for a non-i64 `~` operand: {diagnostics:?}"
    );
}

#[test]
fn validates_extern_declaration_and_call() {
    // A body-less `extern fn` registers a signature so calls type-check like
    // any other call (arity + i64 argument/return types), even though it has
    // no body to validate.
    let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n";
    let checked = validate_source(source).expect("extern decl + call type-checks");
    let extern_fn = checked
        .program
        .functions
        .iter()
        .find(|f| f.name == "llabs")
        .expect("extern function present");
    assert!(extern_fn.is_extern, "llabs is marked extern");
    assert!(extern_fn.body.is_empty(), "extern function has no body");
}

#[test]
fn validates_extern_with_pointer_and_cstr_signature() {
    // An extern parameter/return may be a C scalar, a raw pointer `ptr<T>`, or
    // (parameter only) the `cstr` marker; a Lullaby `string` argument passed to
    // a `cstr` parameter type-checks (the FFI boundary materializes it), and a
    // `ptr<byte>` return binds to a `ptr<byte>` local.
    let source = concat!(
        "extern fn strlen s cstr -> usize\n\n",
        "extern fn malloc n usize -> ptr<byte>\n\n",
        "extern fn free p ptr<byte> -> void\n\n",
        "fn main -> i64\n",
        "    let p ptr<byte> = malloc(to_usize(8))\n",
        "    free(p)\n",
        "    to_i64(strlen(\"hello\"))\n",
    );
    let checked = validate_source(source).expect("pointer/cstr extern type-checks");
    assert!(
        checked
            .program
            .functions
            .iter()
            .any(|f| f.name == "strlen" && f.is_extern),
        "expected the cstr extern registered"
    );
}

#[test]
fn rejects_extern_with_nonmarshallable_param() {
    // A `list<i64>` extern parameter is not C-marshallable; the declaration is
    // rejected with `L0424` (the shared FFI-signature family) rather than
    // silently demoted at native codegen.
    let source = "extern fn bad xs list<i64> -> i64\n\nfn main -> i64\n    0\n";
    let diagnostics = validate_source(source).expect_err("non-marshallable extern rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0424"),
        "expected L0424 for a non-marshallable extern parameter: {diagnostics:?}"
    );
}

#[test]
fn rejects_extern_callback_parameter() {
    // A function-pointer (callback) extern parameter is deferred and rejected
    // with `L0424` — never miscompiled.
    let source = "extern fn run cb fn(i32) -> i32 -> i32\n\nfn main -> i64\n    0\n";
    let diagnostics = validate_source(source).expect_err("callback extern rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0424"),
        "expected L0424 for a callback extern parameter: {diagnostics:?}"
    );
}

#[test]
fn validates_export_i64_scalar_function() {
    // An `export fn` with an all-i64 signature and a body type-checks like any
    // ordinary function; the `is_export` marker is preserved.
    let source = "export fn add_seven x i64 -> i64\n    x + 7\n";
    let checked = validate_source(source).expect("i64 export type-checks");
    let export_fn = checked
        .program
        .functions
        .iter()
        .find(|f| f.name == "add_seven")
        .expect("export function present");
    assert!(export_fn.is_export, "add_seven is marked export");
}

#[test]
fn rejects_export_with_non_scalar_signature() {
    // Exports support the `i64`/`f64`/`f32` scalar set; a `string` return is
    // outside it and is `L0424`.
    let source = "export fn label x i64 -> string\n    to_string(x)\n";
    let diagnostics = validate_source(source).expect_err("non-scalar export rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0424"),
        "expected L0424: {diagnostics:?}"
    );
}

#[test]
fn validates_export_float_scalar_function() {
    // An `export fn` with `f64`/`f32` params and return is now in the exported
    // scalar set (SSE-register marshalling), so it type-checks and keeps its
    // `is_export` marker. A mixed float/int signature is also accepted.
    let source = "export fn scale x f64 y f64 -> f64\n    x * y\n";
    let checked = validate_source(source).expect("f64 export type-checks");
    assert!(
        checked
            .program
            .functions
            .iter()
            .find(|f| f.name == "scale")
            .expect("export function present")
            .is_export,
        "scale is marked export"
    );

    let mixed = "export fn bias x f32 n i64 -> f32\n    x\n";
    assert!(
        validate_source(mixed).is_ok(),
        "a mixed float/int export signature type-checks"
    );
}

#[test]
fn extern_call_arity_mismatch_is_reported() {
    // Extern call sites are checked like ordinary calls: wrong arity is L0312.
    let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(1, 2)\n";
    let diagnostics = validate_source(source).expect_err("arity mismatch");
    assert!(diagnostics.iter().any(|d| d.code == "L0312"));
}

#[test]
fn validates_calls_and_bindings() {
    let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(1, 2)\n    value\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_inferred_bindings() {
    let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value = add(1, 2)\n    let values = [value, 4]\n    values[0]\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_memory_builtins() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_store_builtin() {
    let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_assignment_and_loops() {
    let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_for_loop() {
    let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_logical_expressions() {
    let source = "fn main -> bool\n    not false and true or false\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_array_literal_and_index() {
    let source = "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn checked_program_exposes_function_signatures() {
    let checked = validate_source("fn add x i64 y i64 -> i64\n    x + y\n").expect("semantic");
    let signature = checked.info.signatures.get("add").expect("signature");
    assert_eq!(
        signature.params,
        vec![TypeRef::new("i64"), TypeRef::new("i64")]
    );
    assert_eq!(signature.return_type, TypeRef::new("i64"));
}

#[test]
fn checked_program_exposes_expression_types() {
    let checked =
        validate_source("fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n")
            .expect("semantic");
    assert!(checked.info.expression_types.iter().any(|expr_type| {
        expr_type.function == "main" && expr_type.ty == TypeRef::new("array<i64>")
    }));
    assert!(
        checked.info.expression_types.iter().any(|expr_type| {
            expr_type.function == "main" && expr_type.ty == TypeRef::new("i64")
        })
    );
}

#[test]
fn non_void_function_rejects_empty_return() {
    let diagnostics = validate_source("fn bad -> i64\n    return\n").expect_err("semantic");
    assert_eq!(diagnostics[0].code, "L0304");
}

#[test]
fn catches_type_mismatch() {
    let diagnostics = validate_source("fn bad -> i64\n    let value bool = 1\n    value\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0303")
    );
}

#[test]
fn accepts_first_class_function_values() {
    let source = concat!(
        "fn inc x i64 -> i64\n",
        "    x + 1\n\n",
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    let g fn(i64) -> i64 = inc\n",
        "    apply(inc, 10) + g(5)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn function_name_is_a_function_value() {
    let source = concat!(
        "fn inc x i64 -> i64\n",
        "    x + 1\n\n",
        "fn main -> fn(i64) -> i64\n",
        "    inc\n",
    );
    let checked = validate_source(source).expect("semantic");
    assert_eq!(
        checked
            .info
            .signatures
            .get("main")
            .expect("main")
            .return_type,
        TypeRef::new("fn(i64) -> i64")
    );
}

#[test]
fn rejects_calling_a_non_function_local() {
    let source = concat!("fn main -> i64\n", "    let x i64 = 3\n", "    x(1)\n",);
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0390")
    );
}

#[test]
fn rejects_passing_a_wrong_signature_function() {
    let source = concat!(
        "fn two x i64 y i64 -> i64\n",
        "    x + y\n\n",
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    apply(two, 10)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn validates_string_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let parts array<string> = split(\"a,b\", \",\")\n",
        "    let joined string = join(parts, \"-\")\n",
        "    let head string = substring(joined, 0, 1)\n",
        "    let ok bool = contains(head, \"a\")\n",
        "    let cleaned string = trim(upper(lower(replace(joined, \"-\", \"+\"))))\n",
        "    find(cleaned, head)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_string_builtin_wrong_type() {
    let diagnostics =
        validate_source("fn main -> i64\n    substring(42, 0, 1)\n    0\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn os_random_type_checks_and_yields_result_of_list_byte() {
    // `os_random(len i64) -> result<list<byte>, string>`: an `i64` argument
    // type-checks, and the `ok` payload is a `list<byte>` (so `len` on it is
    // valid and the whole program is well-typed).
    let source = concat!(
        "fn count n i64 -> i64\n",
        "    match os_random(n)\n",
        "        ok(bytes) -> len(bytes)\n",
        "        err(_) -> 0\n\n",
        "fn main -> i64\n",
        "    count(16)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "os_random should type-check with an i64 argument and a list<byte> ok payload"
    );
}

#[test]
fn rejects_os_random_wrong_argument_type() {
    // A `string` where `os_random` expects an `i64` is an argument-type
    // error (`L0313`), never accepted.
    let diagnostics =
            validate_source("fn main -> i64\n    match os_random(\"16\")\n        ok(b) -> len(b)\n        err(_) -> 0\n")
                .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313"),
        "expected L0313 for a non-i64 os_random argument: {diagnostics:?}"
    );
}

#[test]
fn rejects_repeat_wrong_count_type() {
    let diagnostics = validate_source("fn main -> i64\n    repeat(\"ab\", \"x\")\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn rejects_join_non_array_argument() {
    let diagnostics =
        validate_source("fn main -> i64\n    join(\"a\", \"-\")\n    0\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn accepts_bit_intrinsics() {
    let source = "fn main -> i64\n    let a i64 = rotate_left(1, 4)\n    let b i64 = rotate_right(a, 4)\n    let c i64 = count_ones(255)\n    let d i64 = leading_zeros(1)\n    let e i64 = trailing_zeros(16)\n    let f i64 = reverse_bytes(b)\n    a + b + c + d + e + f\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_rotate_left_with_non_i64_argument() {
    let diagnostics =
        validate_source("fn main -> i64\n    let x i64 = rotate_left(1, 2.0)\n    x\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374")
    );
}

#[test]
fn rejects_count_ones_with_non_i64_argument() {
    let diagnostics = validate_source("fn main -> i64\n    let x i64 = count_ones(1.0)\n    x\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374")
    );
}

#[test]
fn catches_assignment_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> bool\n    let value bool = false\n    value = 1\n    value\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0314")
    );
}

#[test]
fn catches_assignment_type_mismatch_after_inference() {
    let diagnostics =
        validate_source("fn bad -> i64\n    let value = 1\n    value = false\n    value\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0314")
    );
}

#[test]
fn catches_undeclared_assignment() {
    let diagnostics =
        validate_source("fn bad -> i64\n    value = 1\n    value\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0316")
    );
}

#[test]
fn catches_break_outside_loop() {
    let diagnostics = validate_source("fn bad -> void\n    break\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0317")
    );
}

#[test]
fn catches_invalid_logical_operand() {
    let diagnostics = validate_source("fn bad -> bool\n    1 and true\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0320")
    );
}

#[test]
fn catches_invalid_for_range_type() {
    let diagnostics =
        validate_source("fn bad -> i64\n    for i from false to 3\n        i\n    0\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0321")
    );
}

#[test]
fn catches_invalid_for_step_type() {
    let diagnostics =
        validate_source("fn bad -> i64\n    for i from 1 to 3 by false\n        i\n    0\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0322")
    );
}

#[test]
fn catches_array_literal_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> array<i64>\n    [1, false]\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0324")
    );
}

#[test]
fn catches_array_index_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> i64\n    let values array<i64> = [1, 2]\n    values[true]\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0326")
    );
}

#[test]
fn catches_ordering_type_mismatch() {
    let diagnostics = validate_source("fn bad -> bool\n    false < true\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0327")
    );
}

#[test]
fn catches_store_value_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> void\n    let ptr ptr_i64 = alloc(1)\n    store(ptr, false)\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0328")
    );
}

#[test]
fn validates_io_and_system_builtins() {
    let source = "fn main -> bool\n    write_file(\"target/lullaby_semantics_io.txt\", \"alpha\")\n    append_file(\"target/lullaby_semantics_io.txt\", \" beta\")\n    let content string = read_file(\"target/lullaby_semantics_io.txt\")\n    let exists bool = file_exists(\"target/lullaby_semantics_io.txt\")\n    let status i64 = sys_status(\"rustc\", [\"--version\"])\n    content == \"alpha beta\" and exists and status == 0\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_stdin_builtins() {
    // `read_line() -> option<string>` type-checks through `match`, and
    // `read_all() -> string` composes with the string library.
    let source = "fn main -> i64\n    let line option<string> = read_line()\n    let all string = read_all()\n    match line\n        some(text) -> len(text) + len(all)\n        none -> len(all)\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_stdin_builtins_with_arguments() {
    // Both stdin builtins are nullary; passing an argument is an arity error.
    assert!(validate_source("fn bad -> string\n    read_all(\"x\")\n").is_err());
    let diagnostics =
        validate_source("fn bad -> i64\n    let line option<string> = read_line(\"x\")\n    0\n")
            .expect_err("semantic");
    assert!(
        !diagnostics.is_empty(),
        "read_line with an argument must be rejected"
    );
}

#[test]
fn resolves_type_aliases_structurally() {
    // `Count` is an alias for `i64`, so alias and target are interchangeable.
    let source = "alias Count = i64\n\nfn main -> Count\n    let a Count = 41\n    let b i64 = a\n    b + 1\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn resolves_alias_inside_generic_argument() {
    let source = "alias Count = i64\n\nfn main -> i64\n    let values array<Count> = [1, 2]\n    values[0]\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_duplicate_type_alias() {
    let diagnostics = validate_source("alias A = i64\nalias A = bool\n\nfn main -> i64\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0360")
    );
}

#[test]
fn rejects_cyclic_type_alias() {
    let diagnostics = validate_source("alias A = B\nalias B = A\n\nfn main -> i64\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0361")
    );
}

#[test]
fn detects_use_after_free_at_compile_time() {
    let diagnostics = validate_source(
            "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    unsafe\n        ptr_read(p)\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0350")
    );
}

#[test]
fn detects_double_free_at_compile_time() {
    let diagnostics = validate_source(
        "fn main -> void\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    dealloc(p)\n",
    )
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0350")
    );
}

#[test]
fn allows_use_before_free() {
    let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_returning_borrowed_reference() {
    let diagnostics =
        validate_source("fn leak h rc<i64> -> ref<i64>\n    rc_borrow(h)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0351")
    );
}

#[test]
fn validates_try_catch_and_throw() {
    let source = "fn main -> void\n    try\n        throw \"oops\"\n    catch message\n        warn(message)\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn try_catch_is_a_value_expression() {
    // Both arms yield a string, so the try/catch can be the function's final value.
    let source = "fn main -> string\n    try\n        throw \"x\"\n    catch message\n        \"caught: \" + message\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_throwing_non_string() {
    let diagnostics = validate_source("fn main -> void\n    throw 42\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn validates_region_declarations() {
    let source = "fn main -> i64\n    region pool: size=4096, align=16, kind=static\n    region scratch: size=1024, kind=dynamic, mutable=true\n    0\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_region_with_bad_size() {
    let diagnostics =
        validate_source("fn main -> i64\n    region pool: size=0\n    0\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0340")
    );
}

#[test]
fn rejects_region_with_non_power_of_two_alignment() {
    let diagnostics =
        validate_source("fn main -> i64\n    region pool: size=1024, align=15\n    0\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0340")
    );
}

#[test]
fn rejects_duplicate_region() {
    let diagnostics = validate_source(
        "fn main -> i64\n    region pool: size=16\n    region pool: size=32\n    0\n",
    )
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0341")
    );
}

#[test]
fn validates_reference_builtins() {
    let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let s rc<i64> = rc_clone(h)\n    let v ref<i64> = rc_borrow(h)\n    let a i64 = ref_get(v)\n    rc_release(s)\n    rc_release(h)\n    a\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn requires_unsafe_for_raw_pointer_read() {
    let diagnostics =
        validate_source("fn main -> i64\n    let p ptr_i64 = alloc(1)\n    ptr_read(p)\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0330")
    );
}

#[test]
fn validates_raw_memory_layout_builtins() {
    // `size_of`/`align_of` on scalars, a fixed array, and a struct, plus
    // `offset_of` with a string-literal field, all type-check (no `unsafe`
    // needed since they are compile-time layout queries).
    let source = "struct Pair\n    a i32\n    b i64\n\nfn main -> i64\n    let arr array<i64> = [1, 2]\n    let p Pair = Pair(to_i32(1), 2)\n    size_of(0) + align_of(byte(0)) + size_of(arr) + size_of(p) + offset_of(p, \"b\")\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_pointer_int_cast_and_volatile_inside_unsafe() {
    let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let n i64 = 0\n    let v i64 = 0\n    unsafe\n        n = ptr_to_int(p)\n        let back ptr_i64 = int_to_ptr(n)\n        volatile_store(back, 7)\n        v = volatile_load(back)\n    dealloc(p)\n    v\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_offset_of_on_non_struct() {
    let diagnostics =
        validate_source("fn main -> i64\n    let x i64 = 1\n    offset_of(x, \"a\")\n")
            .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0431"),
        "offset_of on a non-struct must be L0431"
    );
}

#[test]
fn rejects_offset_of_unknown_field() {
    let diagnostics = validate_source(
            "struct Pair\n    a i64\n    b i64\n\nfn main -> i64\n    let p Pair = Pair(1, 2)\n    offset_of(p, \"missing\")\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0431"),
        "offset_of of an unknown field must be L0431"
    );
}

#[test]
fn rejects_size_of_on_non_sized_type() {
    let diagnostics =
        validate_source("fn main -> i64\n    size_of(\"text\")\n    0\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0431"),
        "size_of on a string must be L0431"
    );
}

#[test]
fn rejects_ptr_to_int_outside_unsafe() {
    let diagnostics = validate_source(
            "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let n i64 = ptr_to_int(p)\n    dealloc(p)\n    n\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0330"),
        "ptr_to_int outside unsafe must be L0330"
    );
}

#[test]
fn validates_asm_inside_unsafe() {
    // A well-formed `asm` inside `unsafe` with in-range bytes type-checks; a
    // trailing `asm` satisfies the `i64` final-value requirement.
    let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_asm_outside_unsafe() {
    let diagnostics = validate_source("fn main -> i64\n    asm 72, 199, 192, 42, 0, 0, 0\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0330"),
        "asm outside unsafe must be L0330"
    );
}

#[test]
fn rejects_asm_byte_out_of_range() {
    let diagnostics = validate_source("fn main -> i64\n    unsafe\n        asm 256\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0425"),
        "out-of-range asm byte must be L0425"
    );
}

#[test]
fn allows_raw_pointer_read_inside_unsafe() {
    let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_reference_builtin_type_mismatch() {
    let diagnostics = validate_source("fn main -> i64\n    let x i64 = 1\n    rc_get(x)\n    x\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0331")
    );
}

#[test]
fn validates_struct_field_mutation() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1, 2)\n    p.x = 9\n    p.y += 1\n    p.x + p.y\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_field_mutation_type_mismatch() {
    let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x = true\n    p.x\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0314")
    );
}

#[test]
fn rejects_mutation_of_unknown_field() {
    let diagnostics = validate_source(
            "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.z = 5\n    p.x\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0371")
    );
}

#[test]
fn validates_array_element_mutation_and_len() {
    let source = "fn main -> i64\n    let xs array<i64> = [1, 2, 3]\n    xs[0] = 10\n    xs[1] += 5\n    xs[len(xs) - 1]\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_array_element_type_mismatch() {
    let diagnostics = validate_source(
        "fn main -> i64\n    let xs array<i64> = [1]\n    xs[0] = true\n    xs[0]\n",
    )
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0314")
    );
}

#[test]
fn rejects_index_assignment_on_non_array() {
    let diagnostics = validate_source("fn main -> i64\n    let n i64 = 1\n    n[0] = 2\n    n\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0325")
    );
}

#[test]
fn rejects_len_on_non_collection() {
    let diagnostics = validate_source("fn main -> i64\n    len(5)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0373")
    );
}

#[test]
fn validates_math_builtins() {
    let source = "fn main -> i64\n    let a i64 = abs(0 - 5)\n    let b i64 = min(a, max(2, 9))\n    let c i64 = pow(2, 3)\n    let d f64 = sqrt(floor(ceil(round(2.5))))\n    if d > 0.0\n        b + c\n    else\n        0\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_math_builtin_on_wrong_type() {
    let diagnostics = validate_source("fn main -> i64\n    sqrt(4)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374")
    );
}

#[test]
fn accepts_list_search_and_rejects_element_mismatch() {
    let ok = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 3)\n",
        "    list_index_of(l, 3)\n",
    );
    assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

    let bad = concat!(
        "fn main -> bool\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 3)\n",
        "    list_contains(l, true)\n",
    );
    let diagnostics = validate_source(bad).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_chars_round_trip_and_rejects_wrong_types() {
    let ok = concat!(
        "fn main -> i64\n",
        "    let cs list<char> = chars(\"hi\")\n",
        "    len(string_from_chars(cs))\n",
    );
    assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

    // `chars` needs a string; `string_from_chars` needs a list<char>.
    let bad1 = validate_source("fn main -> i64\n    len(chars(5))\n").expect_err("semantic");
    assert!(bad1.iter().any(|d| d.code == "L0375"), "{bad1:?}");
    let bad2 = validate_source("fn main -> i64\n    len(string_from_chars(\"x\"))\n")
        .expect_err("semantic");
    assert!(bad2.iter().any(|d| d.code == "L0375"), "{bad2:?}");
}

#[test]
fn validates_trig_exp_and_log_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a f64 = sin(cos(tan(atan(0.0))))\n",
        "    let b f64 = atan2(1.0, 2.0)\n",
        "    let c f64 = exp(ln(log10(1000.0)))\n",
        "    if a + b + c > 0.0\n",
        "        1\n",
        "    else\n",
        "        0\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_trig_builtin_on_wrong_type() {
    let diagnostics =
        validate_source("fn main -> i64\n    let x f64 = sin(1)\n    0\n").expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0374"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_atan2_with_non_f64_argument() {
    let diagnostics = validate_source("fn main -> i64\n    let x f64 = atan2(1.0, 2)\n    0\n")
        .expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0374"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_min_with_mismatched_operands() {
    let diagnostics = validate_source("fn main -> i64\n    min(1, 2.0)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374")
    );
}

#[test]
fn accepts_clamp_sign_gcd_numeric_helpers() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a i64 = clamp(20, 5, 10)\n",
        "    let b f64 = clamp(1.0, 3.0, 8.0)\n",
        "    let s i64 = sign(a)\n",
        "    let t i64 = sign(b)\n",
        "    let g i64 = gcd(12, 18)\n",
        "    if b > 0.0\n",
        "        a + s + t + g\n",
        "    else\n",
        "        0\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_clamp_with_mismatched_operands() {
    let diagnostics =
        validate_source("fn main -> i64\n    clamp(1, 2.0, 3)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_sign_on_non_numeric() {
    let diagnostics = validate_source("fn main -> i64\n    sign(\"x\")\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_gcd_on_f64() {
    let diagnostics = validate_source("fn main -> i64\n    gcd(1.0, 2.0)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0374"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_list_aggregate_helpers() {
    let source = concat!(
        "fn pick o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let l list<i64> = push(list_new(), 3)\n",
        "    let total i64 = list_sum(l)\n",
        "    let lo i64 = pick(list_min(l))\n",
        "    let hi i64 = pick(list_max(l))\n",
        "    total + lo + hi\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn accepts_list_sum_on_f64_list() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<f64> = push(list_new(), 1.5)\n",
        "    let total f64 = list_sum(l)\n",
        "    if total > 0.0\n",
        "        1\n",
        "    else\n",
        "        0\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_list_sum_on_string_list() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<string> = push(list_new(), \"a\")\n",
        "    let total i64 = list_sum(l)\n",
        "    total\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_list_max_on_string_list() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<string> = push(list_new(), \"a\")\n",
        "    let hi option<string> = list_max(l)\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn validates_struct_construction_and_field_access() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x + p.y\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_unknown_struct_field() {
    let diagnostics = validate_source(
        "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.z\n",
    )
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0371")
    );
}

#[test]
fn rejects_wrong_struct_construction() {
    let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0372")
    );
}

#[test]
fn rejects_struct_field_type_mismatch() {
    let diagnostics = validate_source(
        "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(true)\n    p.x\n",
    )
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0372")
    );
}

#[test]
fn validates_f64_arithmetic() {
    let source = "fn main -> f64\n    let x f64 = 1.5\n    x * 2.0 - 0.5\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_mixing_i64_and_f64() {
    let diagnostics =
        validate_source("fn main -> f64\n    let x f64 = 1.5\n    x + 2\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0307")
    );
}

#[test]
fn validates_named_field_construction_in_any_order() {
    let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(y: 4, x: 3)\n    p.x\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_named_construction_missing_field() {
    let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(x: 3)\n    p.x\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0372")
    );
}

#[test]
fn rejects_named_construction_unknown_field() {
    let diagnostics = validate_source(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(x: 3, y: 4, z: 5)\n    p.x\n",
        )
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0372")
    );
}

#[test]
fn validates_string_concatenation_and_to_string() {
    let source = "fn main -> string\n    \"n=\" + to_string(1 + 2) + \" b=\" + to_string(true)\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_char_and_byte_builtins_and_comparisons() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a char = 'a'\n",
        "    let b char = char_from(char_code(a) + 1)\n",
        "    let ordered i64 = 0\n",
        "    if a < b\n",
        "        ordered = 1\n",
        "    let small byte = byte(10)\n",
        "    let big byte = byte(250)\n",
        "    let s string = to_string(a) + to_string(small)\n",
        "    char_code(b) + byte_val(big) + ordered + len(s)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_char_builtin_with_wrong_argument_type() {
    let diagnostics = validate_source("fn main -> i64\n    char_code(65)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0389")
    );
}

#[test]
fn validates_char_classification_predicates() {
    // The six `char -> bool` classification predicates each take one `char`
    // and yield a `bool` usable in a condition.
    let source = concat!(
        "fn main -> i64\n",
        "    let c char = '7'\n",
        "    let flags i64 = 0\n",
        "    if is_digit(c)\n",
        "        flags = flags + 1\n",
        "    if is_alpha(c)\n",
        "        flags = flags + 1\n",
        "    if is_alnum(c)\n",
        "        flags = flags + 1\n",
        "    if is_whitespace(c)\n",
        "        flags = flags + 1\n",
        "    if is_upper(c)\n",
        "        flags = flags + 1\n",
        "    if is_lower(c)\n",
        "        flags = flags + 1\n",
        "    flags\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_char_classification_predicate_with_wrong_argument_type() {
    // `is_digit` requires a `char`; passing an `i64` is an `L0389` argument
    // type error.
    let diagnostics = validate_source("fn main -> bool\n    is_digit(7)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0389")
    );
}

#[test]
fn validates_env_and_args_process_builtins() {
    // `env(name)` yields `option<string>`; `args()` yields `list<string>`.
    let source = concat!(
        "fn env_flag name string -> i64\n",
        "    match env(name)\n",
        "        some(_) -> 1\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    env_flag(\"HOME\") + len(args())\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_env_with_wrong_argument_type() {
    let diagnostics = validate_source(concat!(
        "fn main -> i64\n",
        "    match env(5)\n",
        "        some(_) -> 1\n",
        "        none -> 0\n",
    ))
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0332")
    );
}

#[test]
fn rejects_args_with_wrong_arity() {
    let diagnostics = validate_source("fn main -> i64\n    len(args(1))\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0332")
    );
}

#[test]
fn rejects_mixed_string_and_int_addition() {
    let diagnostics = validate_source("fn main -> string\n    \"n=\" + 5\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0307")
    );
}

#[test]
fn validates_standard_stream_builtins() {
    let source = "fn main -> void\n    println(\"hello\")\n    print(\"partial\")\n    warn(\"careful\")\n    flush()\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn catches_stream_builtin_argument_type_mismatch() {
    let diagnostics = validate_source("fn bad -> void\n    println(1)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn catches_stream_builtin_arity_mismatch() {
    let diagnostics = validate_source("fn bad -> void\n    flush(\"x\")\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0312")
    );
}

#[test]
fn validates_time_builtins() {
    let source = concat!(
        "fn main -> void\n",
        "    let a i64 = mono_now()\n",
        "    let b i64 = wall_now()\n",
        "    sleep_millis(0)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn catches_sleep_millis_argument_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> void\n    sleep_millis(\"x\")\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn catches_file_builtin_argument_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> string\n    read_file(1)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn catches_fs_builtin_argument_type_mismatch() {
    // A non-`string` path to a file-system builtin reports the dedicated
    // `L0333` code.
    let diagnostics = validate_source("fn bad -> i64\n    file_size(1)\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0333")
    );
}

#[test]
fn catches_fs_builtin_arity_mismatch() {
    // Wrong arity for a file-system builtin also reports `L0333`.
    let diagnostics = validate_source("fn bad -> void\n    make_dir()\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0333")
    );
}

#[test]
fn accepts_parallel_map_with_matching_types() {
    let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    parallel_map(sq, base)\n";
    validate_source(source).expect("semantic");
}

#[test]
fn rejects_parallel_map_non_function_first_argument() {
    // The first argument must be a `fn(i64) -> i64`, not a plain list.
    let source =
        "fn main -> list<i64>\n    let base list<i64> = list_new()\n    parallel_map(base, base)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0334")
    );
}

#[test]
fn rejects_parallel_map_non_list_second_argument() {
    // The second argument must be a `list<i64>`, not an `i64`.
    let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    parallel_map(sq, 5)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0334")
    );
}

#[test]
fn accepts_list_map_with_named_and_closure_functions() {
    // `list_map(list<i64>, fn(i64) -> U)` yields `list<U>`; a named function
    // and a closure literal are both accepted, and `U` may differ from `T`.
    let named = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_map(base, sq)\n";
    validate_source(named).expect("list_map with named function");
    let closure = "fn tag x i64 -> bool\n    x > 0\n\nfn main -> list<bool>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_map(base, fn x i64 -> x > 1)\n";
    validate_source(closure).expect("list_map with closure returning a different type");
}

#[test]
fn accepts_list_filter_and_list_reduce() {
    let filter = "fn keep x i64 -> bool\n    x > 1\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_filter(base, keep)\n";
    validate_source(filter).expect("list_filter with predicate");
    let reduce = "fn main -> i64\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_reduce(base, 0, fn acc i64 x i64 -> acc + x)\n";
    validate_source(reduce).expect("list_reduce with folding closure");
}

#[test]
fn rejects_list_map_non_function_second_argument() {
    // The second argument must be a function value, not a plain list. A
    // wrong list-builtin argument reports the general `L0387` code.
    let source = "fn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_map(base, base)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn rejects_list_filter_non_bool_predicate() {
    // `list_filter`'s predicate must return `bool`; `fn(i64) -> i64` is
    // rejected with the list-builtin diagnostic `L0387`.
    let source = "fn sq x i64 -> i64\n    x * x\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_filter(base, sq)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn rejects_list_reduce_wrong_function_arity() {
    // `list_reduce`'s folder must be `fn(U, T) -> U`; a single-argument
    // function is the wrong arity and reports `L0387`.
    let source = "fn inc x i64 -> i64\n    x + 1\n\nfn main -> i64\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    list_reduce(base, 0, inc)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn accepts_sort_by_with_named_and_closure_comparators() {
    // `sort_by(list<T>, fn(T, T) -> i64)` yields `list<T>`; a named
    // comparator and a closure literal are both accepted.
    let named = "fn cmp a i64 b i64 -> i64\n    a - b\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    sort_by(base, cmp)\n";
    validate_source(named).expect("sort_by with named comparator");
    let closure = "fn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    sort_by(base, fn a i64 b i64 -> b - a)\n";
    validate_source(closure).expect("sort_by with closure comparator");
}

#[test]
fn accepts_sort_over_i64_f64_and_string_lists() {
    // `sort` accepts `list<i64>`, `list<f64>`, and `list<string>`.
    let ints = "fn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    sort(base)\n";
    validate_source(ints).expect("sort over list<i64>");
    let floats = "fn main -> list<f64>\n    let base list<f64> = list_new()\n    base = push(base, 2.0)\n    sort(base)\n";
    validate_source(floats).expect("sort over list<f64>");
    let strings = "fn main -> list<string>\n    let base list<string> = list_new()\n    base = push(base, \"a\")\n    sort(base)\n";
    validate_source(strings).expect("sort over list<string>");
}

#[test]
fn rejects_sort_by_non_function_comparator() {
    // The second argument must be a function value; a plain list reports the
    // general `L0387` list-builtin diagnostic.
    let source = "fn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    sort_by(base, base)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn rejects_sort_by_wrong_arity_comparator() {
    // `sort_by`'s comparator must be `fn(T, T) -> i64`; a single-argument
    // function is the wrong arity and reports `L0387`.
    let source = "fn inc x i64 -> i64\n    x + 1\n\nfn main -> list<i64>\n    let base list<i64> = list_new()\n    base = push(base, 2)\n    sort_by(base, inc)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn rejects_sort_on_unsupported_element_type() {
    // `sort` only orders `i64`/`f64`/`string` scalar lists; a `list<bool>`
    // is rejected with `L0387`.
    let source = "fn main -> list<bool>\n    let base list<bool> = list_new()\n    base = push(base, true)\n    sort(base)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0387")
    );
}

#[test]
fn accepts_concurrency_builtins_with_matching_types() {
    let source = "fn worker ch Chan v i64 -> void\n    send(ch, v * v)\n\nfn main -> i64\n    let ch Chan = chan_new()\n    let t Task = spawn(worker, ch, 3)\n    task_join(t)\n    let m Mutex = mutex_new(0)\n    mutex_set(m, 5)\n    mutex_add(m, 2)\n    recv(ch) + mutex_get(m)\n";
    validate_source(source).expect("semantic");
}

#[test]
fn rejects_spawn_non_function_first_argument() {
    // `spawn`'s first argument must be a `fn(Chan, i64) -> void`.
    let source = "fn main -> i64\n    let ch Chan = chan_new()\n    let t Task = spawn(ch, ch, 3)\n    task_join(t)\n    0\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337")
    );
}

#[test]
fn rejects_send_non_chan_first_argument() {
    // `send` requires a `Chan` handle as its first argument.
    let source = "fn main -> void\n    send(5, 5)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337")
    );
}

#[test]
fn rejects_mutex_add_non_mutex_first_argument() {
    // `mutex_add` requires a `Mutex` handle as its first argument.
    let source = "fn main -> i64\n    mutex_add(5, 1)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337")
    );
}

#[test]
fn accepts_atomic_builtins_with_matching_types() {
    // The full `atomic_i64` surface type-checks: construct, load/store,
    // swap, strong CAS, and every fetch-and-op. Each op takes the
    // `atomic_i64` handle first and returns the documented type.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(10)\n",
        "    let p i64 = atomic_add(a, 5)\n",
        "    atomic_sub(a, 1)\n",
        "    atomic_and(a, 15)\n",
        "    atomic_or(a, 1)\n",
        "    atomic_xor(a, 2)\n",
        "    atomic_store(a, 42)\n",
        "    let s i64 = atomic_swap(a, 7)\n",
        "    let c i64 = atomic_cas(a, 7, 99)\n",
        "    p + s + c + atomic_load(a)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_atomic_load_non_atomic_first_argument() {
    // `atomic_load` requires an `atomic_i64` handle as its first argument;
    // a bare `i64` is rejected with the concurrency-builtin code L0337.
    let source = "fn main -> i64\n    atomic_load(5)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_atomic_add_wrong_operand_type() {
    // The `v` operand of a fetch-and-op must be an `i64`, not a string.
    let source = "fn main -> i64\n    let a atomic_i64 = atomic_new(0)\n    atomic_add(a, \"x\")\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_ordered_atomics_with_valid_orderings() {
    // The full ordering-taking atomic surface plus `fence` type-checks when
    // each op is given an ordering it permits: a `release` store, an
    // `acquire`/`relaxed`/`seq_cst` load, every RMW under an arbitrary
    // ordering, an `acq_rel`/`acquire` CAS, and a `seq_cst` fence.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(10)\n",
        "    atomic_store_ordered(a, 20, release)\n",
        "    let l i64 = atomic_load_ordered(a, acquire)\n",
        "    let p i64 = atomic_add_ordered(a, 5, relaxed)\n",
        "    atomic_sub_ordered(a, 1, seq_cst)\n",
        "    atomic_and_ordered(a, 15, acquire)\n",
        "    atomic_or_ordered(a, 1, release)\n",
        "    atomic_xor_ordered(a, 2, acq_rel)\n",
        "    let s i64 = atomic_swap_ordered(a, 7, seq_cst)\n",
        "    let c i64 = atomic_cas_ordered(a, 7, 99, acq_rel, acquire)\n",
        "    fence(seq_cst)\n",
        "    l + p + s + c + atomic_load_ordered(a, relaxed)\n",
    );
    validate_source(source).expect("semantic");
}

#[test]
fn accepts_memory_order_passed_through_a_local() {
    // A `MemoryOrder` value bound to a local type-checks as the ordering
    // argument; the specific ordering is validated at runtime, not statically.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(0)\n",
        "    let o MemoryOrder = acquire\n",
        "    atomic_load_ordered(a, o)\n",
    );
    validate_source(source).expect("semantic");
}

#[test]
fn rejects_release_ordering_on_a_load() {
    // A load can never use `release` (nor `acq_rel`): rejected with L0432.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(0)\n",
        "    atomic_load_ordered(a, release)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0432"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_acquire_ordering_on_a_store() {
    // A store can never use `acquire` (nor `acq_rel`): rejected with L0432.
    let source = concat!(
        "fn main -> void\n",
        "    let a atomic_i64 = atomic_new(0)\n",
        "    atomic_store_ordered(a, 1, acquire)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0432"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_relaxed_ordering_on_a_fence() {
    // A `relaxed` fence is meaningless: rejected with L0432.
    let source = "fn main -> void\n    fence(relaxed)\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0432"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_release_cas_failure_ordering() {
    // A CAS failure ordering is a load and cannot be `release`/`acq_rel`.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(0)\n",
        "    atomic_cas_ordered(a, 0, 1, seq_cst, release)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0432"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_non_memory_order_ordering_argument() {
    // The ordering argument must be a `MemoryOrder`; an `i64` is rejected
    // with the concurrency-builtin type code L0337.
    let source = concat!(
        "fn main -> i64\n",
        "    let a atomic_i64 = atomic_new(0)\n",
        "    atomic_load_ordered(a, 5)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0337"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_user_enum_reusing_memory_order_variant() {
    // `acquire` and friends are reserved by the built-in `MemoryOrder` enum;
    // a user enum reusing one collides with L0382.
    let source = concat!(
        "enum Signal\n",
        "    acquire\n",
        "    done\n",
        "\n",
        "fn main -> i64\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0382"),
        "{diagnostics:?}"
    );
}

#[test]
fn catches_write_bytes_data_type_mismatch() {
    // `write_bytes` requires a `list<byte>` data argument.
    let diagnostics = validate_source("fn bad -> void\n    write_bytes(\"p\", \"not bytes\")\n")
        .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0333")
    );
}

#[test]
fn string_bytes_builtins_type_check() {
    // `to_bytes` -> `list<byte>`, `from_bytes` -> `result<string, string>`
    // (unwrapped with `match`), and `byte_len` -> `i64`.
    validate_source(concat!(
        "fn f -> i64\n",
        "    let b list<byte> = to_bytes(\"hi\")\n",
        "    let n i64 = byte_len(\"café\")\n",
        "    match from_bytes(b)\n",
        "        ok(s) -> len(s) + n + byte_val(get(b, 0))\n",
        "        err(m) -> 0 - len(m)\n",
    ))
    .expect("string↔bytes builtins type-check");
}

#[test]
fn catches_to_bytes_argument_type_mismatch() {
    // `to_bytes` requires a `string` argument; a wrong type reports the
    // string-builtin family code `L0375`.
    let diagnostics =
        validate_source("fn bad -> i64\n    len(to_bytes(7))\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn catches_from_bytes_argument_type_mismatch() {
    // `from_bytes` requires a `list<byte>` argument; a `string` is rejected
    // with the string-builtin family code `L0375`.
    let diagnostics = validate_source(concat!(
        "fn bad -> i64\n",
        "    match from_bytes(\"not bytes\")\n",
        "        ok(s) -> len(s)\n",
        "        err(m) -> 0 - len(m)\n",
    ))
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn parse_number_builtins_type_check() {
    // `parse_i64` -> `result<i64, string>` and `parse_f64` ->
    // `result<f64, string>`, unwrapped with `match` in a helper's tail.
    validate_source(concat!(
        "fn to_int s string -> i64\n",
        "    match parse_i64(s)\n",
        "        ok(n) -> n\n",
        "        err(m) -> 0 - len(m)\n",
        "fn to_float s string -> f64\n",
        "    match parse_f64(s)\n",
        "        ok(x) -> x\n",
        "        err(m) -> 0.0\n",
        "fn f -> f64\n",
        "    to_float(\"3.5\") + sqrt(to_float(\"4.0\"))\n",
        "fn g -> i64\n",
        "    to_int(\"42\")\n",
    ))
    .expect("parse_i64/parse_f64 builtins type-check");
}

#[test]
fn catches_parse_i64_argument_type_mismatch() {
    // `parse_i64` requires a `string` argument; an `i64` is rejected with
    // the string-builtin family code `L0375`.
    let diagnostics = validate_source(concat!(
        "fn bad -> i64\n",
        "    match parse_i64(7)\n",
        "        ok(n) -> n\n",
        "        err(m) -> 0 - len(m)\n",
    ))
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn catches_parse_f64_argument_type_mismatch() {
    // `parse_f64` requires a `string` argument; an `i64` is rejected with
    // the string-builtin family code `L0375`.
    let diagnostics = validate_source(concat!(
        "fn bad -> f64\n",
        "    match parse_f64(7)\n",
        "        ok(x) -> x\n",
        "        err(m) -> 0.0\n",
    ))
    .expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0375")
    );
}

#[test]
fn catches_system_builtin_argument_type_mismatch() {
    let diagnostics =
        validate_source("fn bad -> i64\n    sys_status(\"rustc\", [1])\n").expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0313")
    );
}

#[test]
fn executable_validation_requires_main_entrypoint() {
    let tokens = lex("fn add x i64 y i64 -> i64\n    x + y\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let diagnostics = validate_executable(&program).expect_err("entrypoint");

    assert_eq!(diagnostics[0].code, "L0329");
}

#[test]
fn executable_validation_rejects_main_parameters() {
    let tokens = lex("fn main arg i64 -> i64\n    arg\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let diagnostics = validate_executable(&program).expect_err("entrypoint");

    assert_eq!(diagnostics[0].code, "L0329");
    assert_eq!(diagnostics[0].function.as_deref(), Some("main"));
}

#[test]
fn validates_enum_declaration_and_construction() {
    let source = "enum Color\n    Red\n    Green\n    Blue\n\nenum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn main -> i64\n    let c Color = Green\n    let s Shape = Circle(2.0)\n    let r Shape = Rect(3.0, 4.0)\n    let e Shape = Empty\n    0\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn enum_construction_returns_owning_enum_type() {
    let source = "enum Shape\n    Circle f64\n    Empty\n\nfn area s Shape -> i64\n    0\n\nfn main -> i64\n    area(Circle(1.0)) + area(Empty)\n";
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_duplicate_variant_within_enum() {
    let source = "enum Color\n    Red\n    Red\n\nfn main -> i64\n    0\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0380")
    );
}

#[test]
fn rejects_enum_construction_arity_mismatch() {
    let source = "enum Shape\n    Circle f64\n    Empty\n\nfn main -> i64\n    let s Shape = Circle(1.0, 2.0)\n    0\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0381")
    );
}

#[test]
fn rejects_enum_construction_payload_type_mismatch() {
    let source = "enum Shape\n    Circle f64\n    Empty\n\nfn main -> i64\n    let s Shape = Circle(1)\n    0\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0381")
    );
}

#[test]
fn rejects_cross_enum_variant_collision() {
    let source = "enum A\n    Shared\n\nenum B\n    Shared\n\nfn main -> i64\n    0\n";
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "L0382")
    );
}

#[test]
fn validates_exhaustive_match_with_bindings() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r * r\n",
        "        Rect(w, h) -> w * h\n",
        "        Empty -> 0\n\n",
        "fn main -> i64\n    area(Circle(3))\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn validates_match_with_wildcard_arm() {
    let source = concat!(
        "enum Color\n    Red\n    Green\n    Blue\n\n",
        "fn rank c Color -> i64\n",
        "    match c\n",
        "        Green -> 10\n",
        "        _ -> 1\n\n",
        "fn main -> i64\n    rank(Blue)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_match_on_non_enum_scrutinee() {
    let source = concat!(
        "fn main -> i64\n",
        "    let x i64 = 1\n",
        "    match x\n",
        "        _ -> 0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0383"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_non_exhaustive_match() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r\n",
        "        Empty -> 0\n\n",
        "fn main -> i64\n    area(Empty)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0384"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_match_arm_with_wrong_binding_arity() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(a, b) -> a\n",
        "        Empty -> 0\n\n",
        "fn main -> i64\n    area(Empty)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0385"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_match_arm_with_unknown_variant() {
    let source = concat!(
        "enum Shape\n    Circle i64\n    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r\n",
        "        Square -> 0\n",
        "        Empty -> 0\n\n",
        "fn main -> i64\n    area(Empty)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0385"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_annotated_option_and_result_construction() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a option<i64> = some(3)\n",
        "    let b option<i64> = none\n",
        "    let r result<i64, string> = ok(3)\n",
        "    let e result<i64, string> = err(\"x\")\n",
        "    0\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn accepts_option_in_return_and_final_expression() {
    let source = concat!(
        "fn pick flag bool -> option<i64>\n",
        "    if flag\n",
        "        return none\n",
        "    some(1)\n\n",
        "fn main -> i64\n",
        "    match pick(true)\n",
        "        some(v) -> v\n",
        "        none -> 0\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn accepts_match_over_option_and_result() {
    let source = concat!(
        "fn unwrap_or o option<i64> fallback i64 -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> fallback\n\n",
        "fn describe r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n\n",
        "fn main -> i64\n",
        "    let o option<i64> = some(2)\n",
        "    let r result<i64, string> = ok(5)\n",
        "    unwrap_or(o, 0) + describe(r)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_none_without_expected_type() {
    let source = concat!("fn main -> i64\n", "    let x = none\n", "    0\n");
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0386"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_ok_without_expected_type() {
    let source = concat!("fn main -> i64\n", "    let x = ok(3)\n", "    0\n");
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0386"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_option_payload_type_mismatch() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a option<i64> = some(true)\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0303"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_list_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 10)\n",
        "    l = push(l, 20)\n",
        "    l = set(l, 0, 5)\n",
        "    let head i64 = get(l, 0)\n",
        "    let n i64 = len(l)\n",
        "    l = pop(l)\n",
        "    head + n + len(l)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_list_new_without_expected_type() {
    let source = concat!("fn main -> i64\n", "    let l = list_new()\n", "    0\n");
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_push_element_type_mismatch() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, true)\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_list_ext_builtins() {
    let source = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 10)\n",
        "    l = push(l, 20)\n",
        "    let r list<i64> = reverse(l)\n",
        "    let both list<i64> = concat(l, r)\n",
        "    let mid list<i64> = slice(both, 1, 3)\n",
        "    len(both) + len(mid)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_concat_element_type_mismatch() {
    let source = concat!(
        "fn main -> i64\n",
        "    let a list<i64> = list_new()\n",
        "    a = push(a, 1)\n",
        "    let b list<bool> = list_new()\n",
        "    b = push(b, true)\n",
        "    let c list<i64> = concat(a, b)\n",
        "    len(c)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_sort_on_i64_list_and_rejects_other_element_types() {
    let ok = concat!(
        "fn main -> i64\n",
        "    let l list<i64> = list_new()\n",
        "    l = push(l, 3)\n",
        "    len(sort(l))\n",
    );
    assert!(validate_source(ok).is_ok(), "{:?}", validate_source(ok));

    let bad = concat!(
        "fn main -> i64\n",
        "    let l list<bool> = list_new()\n",
        "    l = push(l, true)\n",
        "    len(sort(l))\n",
    );
    let diagnostics = validate_source(bad).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0387"),
        "{diagnostics:?}"
    );
}

#[test]
fn infers_nested_constructor_in_call_argument_position() {
    // Argument-position inference: a nested `list_new()`/`map_new()` inside a
    // collection-growing builtin, and a nested `ok`/`none`/`some` inside a
    // user call, take their type from the surrounding context.
    let source = concat!(
        "fn describe r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> 0 - len(m)\n\n",
        "fn count o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let data list<byte> = push(list_new(), byte(65))\n",
        "    let m map<string, i64> = map_set(map_new(), \"x\", 7)\n",
        "    let a i64 = describe(ok(5))\n",
        "    let b i64 = count(none)\n",
        "    byte_val(get(data, 0)) + map_len(m) + a + b\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn accepts_map_builtins() {
    let source = concat!(
        "fn lookup m map<string, i64> k string -> i64\n",
        "    match map_get(m, k)\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"a\", 1)\n",
        "    let has i64 = 0\n",
        "    if map_has(m, \"a\")\n",
        "        has = 1\n",
        "    let n i64 = map_len(m)\n",
        "    m = map_del(m, \"a\")\n",
        "    has + n + lookup(m, \"a\") + map_len(m)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn accepts_map_keys_and_values() {
    let source = concat!(
        "fn main -> i64\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"a\", 5)\n",
        "    let ks list<string> = map_keys(m)\n",
        "    let vs list<i64> = map_values(m)\n",
        "    len(ks) + get(vs, 0)\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source)
    );
}

#[test]
fn rejects_map_keys_on_non_map() {
    let diagnostics =
        validate_source("fn main -> i64\n    len(map_keys(3))\n").expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0388"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_map_new_without_expected_type() {
    let source = concat!("fn main -> i64\n", "    let m = map_new()\n", "    0\n");
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0388"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_map_with_unsupported_key_type() {
    let source = concat!(
        "fn main -> i64\n",
        "    let m map<bool, i64> = map_new()\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0388"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_result_payload_type_mismatch() {
    let source = concat!(
        "fn main -> i64\n",
        "    let r result<i64, string> = ok(\"x\")\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0303"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_non_exhaustive_option_match() {
    let source = concat!(
        "fn get o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n\n",
        "fn main -> i64\n",
        "    get(some(1))\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0384"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_user_variant_named_none() {
    let source = concat!(
        "enum Maybe\n    just i64\n    none\n\n",
        "fn main -> i64\n    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0382"),
        "{diagnostics:?}"
    );
}

#[test]
fn validates_generic_functions_at_several_types() {
    let source = concat!(
        "fn identity<T> x T -> T\n",
        "    x\n\n",
        "fn wrap<T> x T -> option<T>\n",
        "    some(x)\n\n",
        "fn choose<T> pick bool a T b T -> T\n",
        "    if pick\n",
        "        return a\n",
        "    b\n\n",
        "fn main -> i64\n",
        "    let n i64 = identity(41)\n",
        "    let s string = identity(\"hi\")\n",
        "    let picked i64 = choose(true, 10, 20)\n",
        "    let maybe option<i64> = wrap(1)\n",
        "    n + picked + len(s)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn generic_identity_return_type_is_the_argument_type() {
    // A `let string = identity("hi")` binding proves the call inferred and
    // substituted `T = string` into the return type; a mismatch would be an
    // `L0301`/binding error.
    let source = concat!(
        "fn identity<T> x T -> T\n",
        "    x\n\n",
        "fn main -> string\n",
        "    let s string = identity(\"hi\")\n",
        "    s\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_conflicting_generic_inference() {
    // `same(1, "x")` binds `T` to both `i64` and `string`: `L0395`.
    let source = concat!(
        "fn same<T> a T b T -> T\n",
        "    a\n\n",
        "fn main -> i64\n",
        "    same(1, \"x\")\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0395"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_arithmetic_on_bare_type_parameter() {
    // `a + b` where both are the bare type variable `T` has no bounds, so
    // arithmetic is rejected (`L0307`).
    let source = concat!(
        "fn plus<T> a T b T -> T\n",
        "    a + b\n\n",
        "fn main -> i64\n",
        "    plus(1, 2)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_ordering_on_bare_type_parameter() {
    let source = concat!(
        "fn less<T> a T b T -> bool\n",
        "    a < b\n\n",
        "fn main -> bool\n",
        "    less(1, 2)\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0327"),
        "{diagnostics:?}"
    );
}

#[test]
fn allows_equality_between_two_same_type_parameters() {
    let source = concat!(
        "fn eq<T> a T b T -> bool\n",
        "    a == b\n\n",
        "fn main -> bool\n",
        "    eq(1, 1)\n",
    );
    assert!(validate_source(source).is_ok());
}

#[test]
fn rejects_duplicate_type_parameter_list() {
    // A duplicate `<T, T>` list is caught at parse time as `L0394`, so the
    // program never reaches semantics.
    let source = concat!(
        "fn dup<T, T> a T -> T\n",
        "    a\n\n",
        "fn main -> i64\n",
        "    dup(1)\n",
    );
    let tokens = lex(source).expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0394"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_type_parameter_shadowing_builtin_type() {
    let source = concat!(
        "fn bad<i64> a i64 -> i64\n",
        "    a\n\n",
        "fn main -> i64\n",
        "    bad(1)\n",
    );
    let tokens = lex(source).expect("lex");
    let diagnostics = parse(&tokens).expect_err("parse");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0394"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_return_only_type_parameter() {
    // `T` appears only in the return type; nothing pins it: `L0396`.
    let source = concat!(
        "fn make<T> -> option<T>\n",
        "    none\n\n",
        "fn main -> i64\n",
        "    let _v option<i64> = make()\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("semantic");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0396"),
        "{diagnostics:?}"
    );
}

#[test]
fn infers_generic_over_list_argument() {
    let source = concat!(
        "fn first<T> xs list<T> -> option<T>\n",
        "    if len(xs) == 0\n",
        "        return none\n",
        "    some(get(xs, 0))\n\n",
        "fn main -> i64\n",
        "    let xs list<i64> = list_new()\n",
        "    let ys list<i64> = push(xs, 7)\n",
        "    match first(ys)\n",
        "        some(v) -> v\n",
        "        none -> 0\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source).err()
    );
}

#[test]
fn accepts_trait_impl_and_bounded_generic() {
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n",
        "    y i64\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "fn describe<T: Show> v T -> string\n",
        "    v.show()\n\n",
        "fn main -> i64\n",
        "    let p Point = Point(3, 4)\n",
        "    len(p.show()) + len(describe(p))\n",
    );
    assert!(
        validate_source(source).is_ok(),
        "{:?}",
        validate_source(source).err()
    );
}

#[test]
fn rejects_incomplete_impl_with_l0398() {
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n\n",
        "impl Show for Point\n",
        "    fn other self -> string\n",
        "        to_string(self.x)\n\n",
        "fn main -> i64\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("incomplete impl");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0398"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_duplicate_impl_with_l0399() {
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "fn main -> i64\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("duplicate impl");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0399"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_unbounded_type_at_bounded_call_with_l0400() {
    // `describe` requires `T: Show`, but `i64` does not implement `Show`.
    let source = concat!(
        "trait Show\n",
        "    fn show self -> string\n\n",
        "struct Point\n",
        "    x i64\n\n",
        "impl Show for Point\n",
        "    fn show self -> string\n",
        "        to_string(self.x)\n\n",
        "fn describe<T: Show> v T -> string\n",
        "    v.show()\n\n",
        "fn main -> i64\n",
        "    len(describe(7))\n",
    );
    let diagnostics = validate_source(source).expect_err("unimplemented bound");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0400"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_socket_builtins() {
    // The socket builtins type-check with a `Socket` handle threaded through
    // a `result` `match`; `tcp_connect` yields `result<Socket, string>`.
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", 80)\n",
        "    match outcome\n",
        "        ok(conn) -> use_conn(conn)\n",
        "        err(message) -> len(message)\n\n",
        "fn use_conn conn Socket -> i64\n",
        "    let sent result<i64, string> = tcp_write(conn, \"hi\")\n",
        "    tcp_close(conn)\n",
        "    match sent\n",
        "        ok(count) -> count\n",
        "        err(message) -> 0 - 1\n",
    );
    validate_source(source).expect("socket builtins type-check");
}

#[test]
fn rejects_wrong_type_socket_arg_with_l0335() {
    // `tcp_connect` expects `(string, i64)`; passing an i64 host is a type
    // error reported as L0335.
    let source = concat!(
        "fn main -> i64\n",
        "    let outcome result<Socket, string> = tcp_connect(5, 80)\n",
        "    match outcome\n",
        "        ok(conn) -> 0\n",
        "        err(message) -> 1\n",
    );
    let diagnostics = validate_source(source).expect_err("wrong socket arg type");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0335"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_non_blocking_socket_builtins() {
    // `set_nonblocking` yields `result<i64, string>`; the `*_nb` reads yield
    // a `result` whose `ok` arm is an `option` (a would-block `none`).
    // `tcp_accept_nb` -> `result<option<Socket>, string>`, `tcp_read_nb` and
    // `udp_recv_nb` -> `result<option<string>, string>`.
    let source = concat!(
        "fn main -> i64\n",
        "    let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n",
        "    match bound\n",
        "        ok(sock) -> probe(sock)\n",
        "        err(message) -> len(message)\n\n",
        "fn probe sock Socket -> i64\n",
        "    let toggled result<i64, string> = set_nonblocking(sock, true)\n",
        "    let got result<option<string>, string> = udp_recv_nb(sock)\n",
        "    tcp_close(sock)\n",
        "    match got\n",
        "        ok(maybe) ->\n",
        "            match maybe\n",
        "                some(data) -> len(data)\n",
        "                none -> 0\n",
        "        err(message) -> len(message)\n",
    );
    validate_source(source).expect("non-blocking socket builtins type-check");
}

#[test]
fn accepts_tcp_accept_nb_and_read_nb_option_results() {
    // `tcp_accept_nb` threads an `option<Socket>` through the `ok` arm and
    // `tcp_read_nb(conn, max i64)` an `option<string>`; both must type-check.
    let source = concat!(
        "fn main -> i64\n",
        "    let listener result<Socket, string> = tcp_listen(\"127.0.0.1\", 0)\n",
        "    match listener\n",
        "        ok(l) -> serve(l)\n",
        "        err(message) -> len(message)\n\n",
        "fn serve l Socket -> i64\n",
        "    let accepted result<option<Socket>, string> = tcp_accept_nb(l)\n",
        "    match accepted\n",
        "        ok(maybe) ->\n",
        "            match maybe\n",
        "                some(client) -> read_some(client)\n",
        "                none -> 0\n",
        "        err(message) -> len(message)\n\n",
        "fn read_some client Socket -> i64\n",
        "    let chunk result<option<string>, string> = tcp_read_nb(client, 1024)\n",
        "    tcp_close(client)\n",
        "    match chunk\n",
        "        ok(maybe) ->\n",
        "            match maybe\n",
        "                some(data) -> len(data)\n",
        "                none -> 0\n",
        "        err(message) -> len(message)\n",
    );
    validate_source(source).expect("tcp accept/read nb type-check");
}

#[test]
fn rejects_set_nonblocking_non_bool_flag_with_l0335() {
    // `set_nonblocking`'s second argument must be a `bool`; an i64 flag is a
    // type error reported as L0335.
    let source = concat!(
        "fn main -> i64\n",
        "    let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n",
        "    match bound\n",
        "        ok(sock) -> flip(sock)\n",
        "        err(message) -> len(message)\n\n",
        "fn flip sock Socket -> i64\n",
        "    let toggled result<i64, string> = set_nonblocking(sock, 1)\n",
        "    match toggled\n",
        "        ok(code) -> code\n",
        "        err(message) -> len(message)\n",
    );
    let diagnostics = validate_source(source).expect_err("non-bool set_nonblocking flag");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0335"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_tcp_accept_nb_wrong_arity_with_l0335() {
    // `tcp_accept_nb` takes exactly one `Socket`; an extra argument is an
    // arity error reported as L0335.
    let source = concat!(
        "fn main -> i64\n",
        "    let listener result<Socket, string> = tcp_listen(\"127.0.0.1\", 0)\n",
        "    match listener\n",
        "        ok(l) -> bad(l)\n",
        "        err(message) -> len(message)\n\n",
        "fn bad l Socket -> i64\n",
        "    let accepted result<option<Socket>, string> = tcp_accept_nb(l, 1)\n",
        "    match accepted\n",
        "        ok(maybe) -> 0\n",
        "        err(message) -> len(message)\n",
    );
    let diagnostics = validate_source(source).expect_err("tcp_accept_nb wrong arity");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0335"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_http_builtins() {
    // `http_get`/`http_post` both yield `result<string, string>`.
    let source = concat!(
        "fn main -> i64\n",
        "    let got result<string, string> = http_get(\"http://127.0.0.1/\")\n",
        "    let posted result<string, string> = http_post(\"http://127.0.0.1/\", \"body\")\n",
        "    match got\n",
        "        ok(body) -> len(body)\n",
        "        err(message) -> len(message)\n",
    );
    validate_source(source).expect("http builtins type-check");
}

#[test]
fn rejects_wrong_type_http_arg_with_l0336() {
    // `http_get` expects `(string)`; passing an i64 url is a type error
    // reported as L0336.
    let source = concat!(
        "fn main -> i64\n",
        "    let got result<string, string> = http_get(5)\n",
        "    match got\n",
        "        ok(body) -> 0\n",
        "        err(message) -> 1\n",
    );
    let diagnostics = validate_source(source).expect_err("wrong http arg type");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0336"),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_assert_with_bool_argument() {
    // `assert(cond bool) -> void` type-checks when its argument is a bool.
    let source = "fn main -> void\n    assert(2 + 2 == 4)\n";
    validate_source(source).expect("assert with bool argument");
}

#[test]
fn rejects_non_bool_assert_with_l0342() {
    // `assert` expects a single `bool`; passing an i64 is reported as L0342.
    let source = "fn main -> void\n    assert(5)\n";
    let diagnostics = validate_source(source).expect_err("non-bool assert argument");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0342"),
        "{diagnostics:?}"
    );
}

#[test]
fn try_operator_on_result_type_checks_and_types_as_payload() {
    // `checked(a)?` is `i64` (the `ok` payload) inside a `result<i64, string>`
    // function; the `?` desugars to a propagate-on-`err` early return.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    if n < 0\n",
        "        return err(\"bad\")\n",
        "    ok(n)\n\n",
        "fn use_it a i64 -> result<i64, string>\n",
        "    let x i64 = checked(a)?\n",
        "    ok(x + 1)\n",
    );
    validate_source(source).expect("`?` on result in a result-returning fn");
}

#[test]
fn try_operator_on_option_type_checks() {
    let source = concat!(
        "fn maybe present bool -> option<i64>\n",
        "    if present\n",
        "        return some(7)\n",
        "    none\n\n",
        "fn use_it p bool -> option<i64>\n",
        "    let x i64 = maybe(p)?\n",
        "    some(x + 1)\n",
    );
    validate_source(source).expect("`?` on option in an option-returning fn");
}

#[test]
fn try_operator_in_incompatible_return_type_is_l0427() {
    // `?` on a `result` inside a plain `i64`-returning function has no
    // compatible propagation target, so it is `L0427`.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    ok(n)\n\n",
        "fn bad a i64 -> i64\n",
        "    let x i64 = checked(a)?\n",
        "    x\n",
    );
    let diagnostics = validate_source(source).expect_err("`?` needs a compatible return type");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0427"),
        "expected L0427: {diagnostics:?}"
    );
}

#[test]
fn try_operator_with_mismatched_error_type_is_l0429() {
    // A `result<i64, string>` operand requires the function to return a
    // `result` with the SAME error type; returning `result<i64, i64>` is
    // `L0429`.
    let source = concat!(
        "fn checked n i64 -> result<i64, string>\n",
        "    ok(n)\n\n",
        "fn bad a i64 -> result<i64, i64>\n",
        "    let x i64 = checked(a)?\n",
        "    ok(x)\n",
    );
    let diagnostics = validate_source(source).expect_err("`?` error type must match");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0429"),
        "expected L0429: {diagnostics:?}"
    );
}

#[test]
fn try_operator_on_non_option_result_is_l0428() {
    // `?` on a plain `i64` value is `L0428`.
    let source = concat!(
        "fn bad -> result<i64, string>\n",
        "    let n i64 = 5\n",
        "    let x i64 = n?\n",
        "    ok(x)\n",
    );
    let diagnostics = validate_source(source).expect_err("`?` needs an option/result operand");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0428"),
        "expected L0428: {diagnostics:?}"
    );
}

#[test]
fn process_builtins_type_check_end_to_end() {
    // `proc_spawn` yields `result<process, string>`; the `process` handle
    // threads into `proc_wait`/`proc_stdout`/`proc_stderr`/`proc_kill`, each
    // returning the documented `result` type. This exercises the whole
    // process handle surface through the type checker.
    let source = concat!(
        "fn main -> i64\n",
        "    let spawned result<process, string> = proc_spawn(\"echo\", [\"hello\"])\n",
        "    match spawned\n",
        "        ok(p) -> drive(p)\n",
        "        err(message) -> 1\n",
        "\n",
        "fn drive p process -> i64\n",
        "    let waited result<i64, string> = proc_wait(p)\n",
        "    let out result<string, string> = proc_stdout(p)\n",
        "    let errs result<string, string> = proc_stderr(p)\n",
        "    let killed result<i64, string> = proc_kill(p)\n",
        "    0\n",
    );
    validate_source(source).expect("process builtins type-check");
}

#[test]
fn rejects_proc_spawn_wrong_arg_type_with_l0335() {
    // `proc_spawn` expects `(string, array<string>)`; passing an i64 command
    // is rejected with the socket/network handle diagnostic family `L0335`.
    let source = concat!(
        "fn main -> i64\n",
        "    let spawned result<process, string> = proc_spawn(5, [\"hello\"])\n",
        "    match spawned\n",
        "        ok(p) -> 0\n",
        "        err(message) -> 1\n",
    );
    let diagnostics = validate_source(source).expect_err("wrong proc_spawn arg type");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0335"),
        "{diagnostics:?}"
    );
}

#[test]
fn fixed_width_integer_arithmetic_and_conversions_type_check() {
    // i32/u32 values arithmetic and compare among themselves and convert to
    // and from i64 through the explicit `to_*` builtins.
    let source = concat!(
        "fn main -> i64\n",
        "    let a u32 = to_u32(10)\n",
        "    let b u32 = to_u32(3)\n",
        "    let c i32 = to_i32(0 - 1)\n",
        "    if c < to_i32(0)\n",
        "        return to_i64(a - b)\n",
        "    to_i64(a + b)\n",
    );
    validate_source(source).expect("i32/u32 arithmetic and conversions type-check");
}

#[test]
fn overflow_arith_builtins_type_check_and_reject_i64() {
    // checked/saturating/wrapping on a fixed-width integer type-check; the
    // checked form yields option<T>, the others yield T.
    let ok = concat!(
        "fn main -> i64\n",
        "    let s u32 = saturating_add(to_u32(1), to_u32(2))\n",
        "    let w u32 = wrapping_mul(to_u32(3), to_u32(4))\n",
        "    match checked_sub(to_u32(1), to_u32(2))\n",
        "        some(v) -> to_i64(v)\n",
        "        none -> to_i64(s) + to_i64(w)\n",
    );
    validate_source(ok).expect("overflow arithmetic on u32 type-checks");
    // i64 is rejected: its default arithmetic already traps on overflow.
    let bad = concat!("fn main -> i64\n", "    checked_add(5, 6)\n");
    let diagnostics = validate_source(bad).expect_err("checked_add on i64 rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn to_string_accepts_the_numeric_lattice() {
    // to_string renders every numeric type, not just i64/f64.
    let source = concat!(
        "fn main -> string\n",
        "    to_string(to_i32(1)) + to_string(to_u64(2)) + to_string(to_f32(3.0))\n",
    );
    validate_source(source).expect("to_string on fixed-width ints and f32 type-checks");
}

#[test]
fn f32_arithmetic_and_conversions_type_check() {
    // f32 values arithmetic and compare among themselves and convert to and
    // from f64 through `to_f32`/`to_f64`; f32 never mixes with f64 directly.
    let source = concat!(
        "fn main -> i64\n",
        "    let a f32 = to_f32(1.5)\n",
        "    let b f32 = to_f32(2.5)\n",
        "    let sum f32 = a + b\n",
        "    if to_f64(sum) > 3.0\n",
        "        return 1\n",
        "    0\n",
    );
    validate_source(source).expect("f32 arithmetic and conversions type-check");
}

#[test]
fn rejects_f32_f64_mixed_operands_with_l0307() {
    // No implicit float coercion: `f32 + f64` has no common numeric type.
    let source = concat!(
        "fn main -> i64\n",
        "    let a f32 = to_f32(1.0)\n",
        "    let bad f32 = a + 2.0\n",
        "    0\n",
    );
    let diagnostics = validate_source(source).expect_err("f32 + f64 must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn wide_fixed_width_conversions_type_check() {
    // The full integer lattice (i8/i16/u16/u64/isize/usize) converts from and
    // back to i64, and each width arithmetic and compares among itself.
    let source = concat!(
        "fn main -> i64\n",
        "    let a i8 = to_i8(127)\n",
        "    let b i16 = to_i16(32767)\n",
        "    let c u16 = to_u16(0 - 1)\n",
        "    let d u64 = to_u64(0 - 1)\n",
        "    let e isize = to_isize(0 - 5)\n",
        "    let f usize = to_usize(9)\n",
        "    let g u64 = d / to_u64(2)\n",
        "    if a < to_i8(0)\n",
        "        return to_i64(g)\n",
        "    to_i64(b) + to_i64(c) + to_i64(e) + to_i64(f)\n",
    );
    validate_source(source).expect("wide fixed-width conversions type-check");
}

#[test]
fn rejects_mixed_width_integer_operands_with_l0307() {
    // No implicit width mixing: `u32 + i32` has no common numeric type.
    let source = concat!(
        "fn main -> i64\n",
        "    let a u32 = to_u32(1)\n",
        "    let b i32 = to_i32(1)\n",
        "    let c u32 = a + b\n",
        "    to_i64(c)\n",
    );
    let diagnostics = validate_source(source).expect_err("u32 + i32 must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_fixed_width_mixed_with_i64_with_l0307() {
    // A fixed-width integer does not silently mix with the default `i64`.
    let source = concat!(
        "fn main -> i64\n",
        "    let a i32 = to_i32(1)\n",
        "    let total i32 = a + 5\n",
        "    to_i64(total)\n",
    );
    let diagnostics = validate_source(source).expect_err("i32 + i64 must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn rejects_to_i64_on_plain_i64_with_l0307() {
    // `to_i64` widens an i32/u32; a plain i64 argument is a type error.
    let source = concat!("fn main -> i64\n", "    to_i64(5)\n");
    let diagnostics = validate_source(source).expect_err("to_i64 needs an i32/u32 argument");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0307"),
        "{diagnostics:?}"
    );
}

#[test]
fn closure_literal_types_as_a_function_value() {
    // A closure `fn x i64 -> x + n` has value type `fn(i64) -> i64`, so it can
    // initialize a function-typed `let` and be called through it.
    let source = concat!(
        "fn main -> i64\n",
        "    let n i64 = 10\n",
        "    let f fn(i64) -> i64 = fn x i64 -> x + n\n",
        "    f(5)\n",
    );
    let checked = validate_source(source).expect("closure type-checks");
    let main = checked
        .program
        .functions
        .iter()
        .find(|function| function.name == "main")
        .expect("main");
    let Stmt::Let { value, .. } = &main.body[1] else {
        panic!("expected `let f` binding");
    };
    let ty = checked
        .info
        .expression_types
        .iter()
        .find(|entry| entry.span == value.span)
        .map(|entry| entry.ty.clone())
        .expect("closure expression type recorded");
    assert_eq!(
        ty,
        function_type(&[TypeRef::new("i64")], &TypeRef::new("i64"))
    );
}

#[test]
fn closure_interoperates_with_apply_and_parallel_map() {
    // A closure value flows into a user `apply` (a function-typed parameter)
    // and into the `parallel_map` builtin, both of which require `fn(i64)->i64`.
    let source = concat!(
        "fn apply f fn(i64) -> i64 v i64 -> i64\n",
        "    f(v)\n\n",
        "fn main -> i64\n",
        "    let k i64 = 3\n",
        "    let bump fn(i64) -> i64 = fn x i64 -> x + k\n",
        "    let xs list<i64> = list_new()\n",
        "    xs = push(xs, 2)\n",
        "    let mapped list<i64> = parallel_map(fn x i64 -> x * k, xs)\n",
        "    apply(bump, 1) + get(mapped, 0)\n",
    );
    validate_source(source).expect("closure interoperates with apply and parallel_map");
}

#[test]
fn closure_body_reports_unknown_free_variable() {
    // A closure body that references a name that is neither a parameter, an
    // enclosing local, nor a top-level name is an unknown-identifier error.
    let source = concat!(
        "fn main -> i64\n",
        "    let f fn(i64) -> i64 = fn x i64 -> x + missing\n",
        "    f(0)\n",
    );
    let diagnostics = validate_source(source).expect_err("unknown free variable rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "L0306"),
        "expected L0306 for an unknown free variable: {diagnostics:?}"
    );
}

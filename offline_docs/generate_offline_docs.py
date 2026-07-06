"""Generate a self-contained Lullaby documentation HTML bundle from Markdown.

This is the initial generator path for Epic 1.5. It intentionally uses only the
Python standard library so the docs build can run in release and installer
verification environments without a package manager.
"""

from __future__ import annotations

import argparse
import html
import re
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = REPO_ROOT / "target" / "offline_docs" / "index.html"

SOURCE_DOCS = [
    ("Project Overview", REPO_ROOT / "README.md"),
    ("Core Language Rules", REPO_ROOT / "documents" / "core_language_rules.md"),
    ("Alpha 1 Language Surface", REPO_ROOT / "documents" / "alpha1_language_surface.md"),
    ("Diagnostics Registry", REPO_ROOT / "documents" / "diagnostic_registry.md"),
    ("Release Notes", REPO_ROOT / "documents" / "alpha1_release_notes.md"),
    ("Post-Alpha Roadmap", REPO_ROOT / "documents" / "post_alpha_roadmap.md"),
]

EXAMPLE_FIXTURES = [
    ("Quick Start", REPO_ROOT / "tests" / "fixtures" / "valid" / "docs_quick_start.lby", "run"),
    ("Calculator", REPO_ROOT / "examples" / "valid" / "calculator.lby", "run"),
    ("Arrays And Control Flow", REPO_ROOT / "examples" / "valid" / "arrays_control_flow.lby", "run"),
]


def slugify(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    return slug or "section"


def inline_markdown(value: str) -> str:
    escaped = html.escape(value)
    escaped = re.sub(r"`([^`]+)`", r"<code>\1</code>", escaped)
    escaped = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", escaped)
    return escaped


def flush_list(output: list[str], list_items: list[str]) -> None:
    if not list_items:
        return
    output.append("<ul>")
    output.extend(f"<li>{item}</li>" for item in list_items)
    output.append("</ul>")
    list_items.clear()


def markdown_to_html(markdown: str) -> str:
    output: list[str] = []
    list_items: list[str] = []
    in_code = False
    code_lines: list[str] = []

    for raw_line in markdown.splitlines():
        line = raw_line.rstrip()

        if line.startswith("```"):
            if in_code:
                output.append(
                    f'<pre data-planned="true"><code>{html.escape(chr(10).join(code_lines))}</code></pre>'
                )
                code_lines.clear()
                in_code = False
            else:
                flush_list(output, list_items)
                in_code = True
            continue

        if in_code:
            code_lines.append(line)
            continue

        if not line:
            flush_list(output, list_items)
            continue

        heading = re.match(r"^(#{1,4})\s+(.+)$", line)
        if heading:
            flush_list(output, list_items)
            level = min(len(heading.group(1)) + 1, 5)
            text = heading.group(2).strip()
            output.append(f'<h{level} id="{slugify(text)}">{inline_markdown(text)}</h{level}>')
            continue

        bullet = re.match(r"^[-*]\s+(.+)$", line)
        if bullet:
            list_items.append(inline_markdown(bullet.group(1)))
            continue

        table_like = line.startswith("|") and line.endswith("|")
        if table_like:
            flush_list(output, list_items)
            cells = [inline_markdown(cell.strip()) for cell in line.strip("|").split("|")]
            if all(set(cell.replace(":", "").replace("-", "")) == set() for cell in cells):
                continue
            output.append("<table><tr>" + "".join(f"<td>{cell}</td>" for cell in cells) + "</tr></table>")
            continue

        flush_list(output, list_items)
        output.append(f"<p>{inline_markdown(line)}</p>")

    flush_list(output, list_items)
    if in_code:
        output.append(
            f'<pre data-planned="true"><code>{html.escape(chr(10).join(code_lines))}</code></pre>'
        )

    return "\n".join(output)


def render_document() -> str:
    nav_items = []
    sections = []

    for title, source in SOURCE_DOCS:
        if not source.exists():
            raise FileNotFoundError(f"required documentation source not found: {source}")
        slug = slugify(title)
        nav_items.append(f'<li><a href="#{slug}">{html.escape(title)}</a></li>')
        body = markdown_to_html(source.read_text(encoding="utf-8"))
        sections.append(
            f'<section id="{slug}"><h1>{html.escape(title)}</h1>'
            f'<p class="source">Source: <code>{html.escape(source.relative_to(REPO_ROOT).as_posix())}</code></p>'
            f"{body}</section>"
        )

    nav_items.append('<li><a href="#executable-examples">Executable Examples</a></li>')
    sections.append(render_examples_section())
    for title, section_id, body in alpha_user_sections():
        nav_items.append(f'<li><a href="#{section_id}">{html.escape(title)}</a></li>')
        sections.append(f'<section id="{section_id}"><h1>{html.escape(title)}</h1>{body}</section>')

    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Lullaby Generated Offline Documentation</title>
  <style>
    :root {{ color-scheme: light dark; font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    body {{ margin: 0; line-height: 1.55; }}
    header {{ padding: 2rem; background: #18212f; color: #f7fbff; }}
    main {{ display: grid; grid-template-columns: minmax(14rem, 18rem) minmax(0, 1fr); gap: 2rem; padding: 1.5rem; }}
    nav {{ position: sticky; top: 1rem; align-self: start; }}
    nav ul {{ list-style: none; padding: 0; }}
    nav li {{ margin: 0.4rem 0; }}
    a {{ color: #235fa7; }}
    section {{ max-width: 76rem; margin-bottom: 3rem; }}
    code, pre {{ font-family: "Cascadia Mono", Consolas, monospace; }}
    code {{ background: rgba(127, 127, 127, 0.14); padding: 0.08rem 0.25rem; border-radius: 0.2rem; }}
    pre {{ overflow-x: auto; padding: 1rem; background: #111827; color: #f9fafb; }}
    table {{ border-collapse: collapse; margin: 0.75rem 0; width: 100%; }}
    td {{ border: 1px solid rgba(127, 127, 127, 0.35); padding: 0.4rem; vertical-align: top; }}
    .source {{ color: #697386; }}
    @media (max-width: 760px) {{ main {{ display: block; }} nav {{ position: static; }} }}
  </style>
</head>
<body>
  <header>
    <h1>Lullaby Generated Offline Documentation</h1>
    <p>Self-contained HTML generated from canonical repository Markdown.</p>
  </header>
  <main>
    <nav aria-label="Documentation sections"><ul>{''.join(nav_items)}</ul></nav>
    <div>{''.join(sections)}</div>
  </main>
</body>
</html>
"""


def alpha_user_sections() -> list[tuple[str, str, str]]:
    return [
        (
            "Overview",
            "overview",
            """
            <p>Lullaby is implemented in Rust. The current alpha focuses on a clear, testable compiler frontend plus AST, IR, and instruction-bytecode execution paths before native code generation.</p>
            <p>The Alpha 1 language surface is frozen in <code>documents/alpha1_language_surface.md</code>. Broader Markdown design docs are planned material unless the feature appears in that file or on this page.</p>
            <ul>
              <li><code>.lby</code> is the canonical source file extension.</li>
              <li>Indentation defines scope. Curly braces are rejected as block delimiters.</li>
              <li><code>lullaby check</code> validates helper and library-style files without <code>main</code>.</li>
              <li><code>lullaby run</code> executes the supported subset with AST, IR, or bytecode backends.</li>
            </ul>
            """,
        ),
        (
            "Quick Start",
            "quick-start",
            """
            <p>Use the portable package from a shell without a development server or internet access.</p>
            <ol>
              <li>Run <code>bin/lullaby --version</code> or <code>bin\\lullaby.exe --version</code>.</li>
              <li>Open the local docs with <code>bin/lullaby docs</code> or <code>bin\\lullaby.exe docs</code>.</li>
              <li>Find packaged examples with <code>bin/lullaby examples</code>.</li>
              <li>Check a source file with <code>bin/lullaby check examples/valid/calculator.lby</code>.</li>
              <li>Run a source file with <code>bin/lullaby run examples/valid/calculator.lby</code>.</li>
              <li>Compile a bytecode artifact with <code>bin/lullaby compile --optimize alpha -o examples/valid/calculator.lbc examples/valid/calculator.lby</code>.</li>
              <li>Inspect and run the artifact with <code>bin/lullaby inspect examples/valid/calculator.lbc</code> and <code>bin/lullaby run examples/valid/calculator.lbc</code>.</li>
            </ol>
            """,
        ),
        (
            "Source Files",
            "source-files",
            """
            <ul>
              <li>Use the <code>.lby</code> extension.</li>
              <li>Use spaces for indentation. A deeper indent opens a block; dedenting closes it.</li>
              <li>Do not use <code>{</code> or <code>}</code> as block delimiters.</li>
              <li>Do not end statements with semicolons.</li>
              <li>Comments begin with <code>#</code> and run to the end of the line.</li>
            </ul>
            """,
        ),
        (
            "Functions",
            "functions",
            """
            <p>Functions start with <code>fn</code>, followed by the name, typed parameters, an optional return arrow, and an indented body.</p>
            <p>When a function body reaches its final expression normally, that expression is the return value. This is the Last-expression return rule.</p>
            <p>A function that returns nothing declares <code>-> void</code>. It may use an empty <code>return</code>, or simply finish with no value-producing final expression.</p>
            """,
        ),
        (
            "Variables And Assignment",
            "variables-assignment",
            """
            <p>Use <code>let</code> to bind a local name with an explicit type and initializer, or omit the type when the initializer has a concrete inferred local type.</p>
            <p>Existing locals can be updated with assignment or numeric compound assignment. Assignment targets must already be declared and the assigned value must match the local type.</p>
            """,
        ),
        (
            "Arrays",
            "arrays",
            """
            <p>The alpha supports homogeneous arrays with <code>array&lt;T&gt;</code> type spelling, non-empty array literals, and bounds-checked indexing.</p>
            <ul>
              <li>Array literal values must all have the same type.</li>
              <li>Empty array literals are not part of the Alpha 1 surface.</li>
              <li>Index expressions require an <code>i64</code> index.</li>
              <li>Out-of-bounds indexes are runtime errors.</li>
            </ul>
            """,
        ),
        (
            "Control Flow",
            "control-flow",
            """
            <p>The alpha supports <code>if</code>, <code>elif</code>, and <code>else</code> branches with boolean conditions.</p>
            <p><code>while</code> loops repeat while a boolean condition is true.</p>
            <p><code>for</code> loops iterate over inclusive <code>i64</code> ranges using <code>from</code>, <code>to</code>, and optional <code>by</code>. The default step is <code>1</code>, and runtime execution rejects step <code>0</code>.</p>
            <p><code>loop</code>, <code>break</code>, and <code>continue</code> support unconditional loops and loop control.</p>
            """,
        ),
        (
            "Boolean Logic",
            "boolean-logic",
            """
            <p>The alpha supports boolean logic with <code>and</code>, <code>or</code>, and unary <code>not</code>. Logical operands must be <code>bool</code>, and <code>and</code>/<code>or</code> short-circuit during runtime execution.</p>
            """,
        ),
        (
            "Memory Builtins",
            "memory-builtins",
            """
            <p>The alpha runtime includes heap-slot builtins. These are an interim executable model while the full region/ARC memory design is still being implemented.</p>
            <table>
              <tr><td><code>alloc(value)</code></td><td>Stores a value in a runtime heap slot and returns an interim pointer such as <code>ptr_i64</code>.</td></tr>
              <tr><td><code>load(ptr)</code></td><td>Loads the value from a valid pointer.</td></tr>
              <tr><td><code>store(ptr, value)</code></td><td>Replaces the value in a valid pointer slot. The stored value must match the pointer element type.</td></tr>
              <tr><td><code>dealloc(ptr)</code></td><td>Clears the heap slot. Invalid or double deallocation is a runtime error.</td></tr>
            </table>
            """,
        ),
        (
            "I/O And System Builtins",
            "io-system",
            """
            <p>The alpha runtime includes flat text file I/O builtins and a conservative system command abstraction. Dotted <code>io.*</code> APIs, streams, binary I/O, memory mapping, async, sockets, and IPC are planned rather than current syntax.</p>
            <table>
              <tr><td><code>read_file(path)</code></td><td>Reads a UTF-8 text file into a <code>string</code>.</td></tr>
              <tr><td><code>write_file(path, content)</code></td><td>Writes text to a file, replacing existing contents.</td></tr>
              <tr><td><code>append_file(path, content)</code></td><td>Appends text to a file, creating it if needed.</td></tr>
              <tr><td><code>file_exists(path)</code></td><td>Returns whether host metadata for the path can be read.</td></tr>
              <tr><td><code>sys_status(program, args)</code></td><td>Runs a program directly with an <code>array&lt;string&gt;</code> argv and returns its exit status.</td></tr>
              <tr><td><code>sys_output(program, args)</code></td><td>Runs a program directly with an <code>array&lt;string&gt;</code> argv and returns stdout.</td></tr>
            </table>
            """,
        ),
        (
            "CLI",
            "cli",
            """
            <table>
              <tr><td><code>cargo run -p lullaby_cli -- check path/to/file.lby</code><br><code>lullaby check [--verbose|--format json] file.lby</code></td><td>Validate extension, lex, parse, and run semantic checks. This can check helper/library-style functions without <code>main</code>.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- compile --optimize alpha -o path/to/file.lbc path/to/file.lby</code><br><code>lullaby compile [--optimize none|constant-fold|dead-code|alpha] -o file.lbc file.lby</code></td><td>Validate executable source with zero-argument main, lower through typed IR, run the current alpha optimizer pipeline, and write a versioned <code>.lbc</code> instruction-bytecode artifact with metadata, a function table, ordered memory operation metadata, and dedicated function instructions.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- build --optimize alpha -o path/to/file.lbc path/to/file.lby</code><br><code>lullaby build</code></td><td>Use the same artifact-generation path as <code>compile</code> with a build-oriented command name.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- inspect path/to/file.lbc</code><br><code>lullaby inspect file.lbc</code></td><td>Print bytecode artifact metadata, function table details, memory operation counts, target, payload, entry point, and function count without executing the program; verbose/JSON modes include memory operation sequence numbers.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run path/to/file.lby</code><br><code>lullaby run [--backend ast|ir|bytecode] file.lby</code></td><td>Execute source through the selected backend. Use <code>--backend ir</code> or <code>--backend bytecode</code> to select typed IR or bytecode execution.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run path/to/file.lbc</code><br><code>lullaby run file.lbc</code></td><td>Execute a compiled bytecode artifact.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run --backend ir --optimize constant-fold path/to/file.lby</code></td><td>Run the IR backend with only the opt-in constant folding optimizer.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run --backend bytecode --optimize dead-code path/to/file.lby</code></td><td>Run the bytecode backend with block-local dead-code elimination. Other optimizer coverage includes common subexpression elimination, loop-invariant motion, copy propagation, <code>--optimize alpha</code>, and <code>--optimize none</code>.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- docs</code><br><code>lullaby docs</code></td><td>Print the local offline documentation path.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- examples</code><br><code>lullaby examples</code></td><td>Print the packaged valid examples directory.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- check --verbose path/to/file.lby</code></td><td>Print source excerpts, caret markers, root cause, and suggested fix text for diagnostics.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- check --format json path/to/file.lby</code></td><td>Print deterministic JSON diagnostics for tools, CI, editors, and LLM agents. <code>--diagnostic-format json</code> is also accepted.</td></tr>
              <tr><td><code>powershell -ExecutionPolicy Bypass -File scripts\\package_windows_portable.ps1</code></td><td>Build the Alpha 1 Windows portable package with generated docs.</td></tr>
              <tr><td><code>powershell -ExecutionPolicy Bypass -File scripts\\verify_release.ps1</code></td><td>Run the full release gate and smoke-test the packaged toolchain.</td></tr>
              <tr><td><code>powershell -ExecutionPolicy Bypass -File scripts\\publish_github_release.ps1</code></td><td>Verify the package, tag the current commit, and create a GitHub prerelease.</td></tr>
            </table>
            """,
        ),
        (
            "Examples",
            "examples",
            """
            <p>Executable examples in this generated bundle are copied from repository fixtures and checked by the verifier. Packaged user examples include <code>examples\\valid\\calculator.lby</code> and invalid examples for inspecting diagnostics.</p>
            """,
        ),
        (
            "Package Layout",
            "package-layout",
            """
            <p>Portable archives use a stable layout so installers and user docs can share one contract.</p>
            <ul>
              <li><code>bin/lullaby</code> or <code>bin/lullaby.exe</code>: command-line tool.</li>
              <li><code>docs/index.html</code> or <code>docs\\index.html</code>: generated offline documentation.</li>
              <li><code>examples/</code>: valid examples and invalid diagnostic examples.</li>
              <li><code>RELEASE_NOTES.md</code>: release notes, supported surface, verification evidence, and limitations.</li>
              <li><code>README.txt</code>: package-local quick start.</li>
              <li><code>VERSION.txt</code> or <code>MANIFEST.json</code>: package metadata including target tag, commit, binary path, docs path, and archive name.</li>
              <li><code>install.cmd</code>, <code>install.ps1</code>, <code>uninstall.cmd</code>, and <code>uninstall.ps1</code>: optional Windows user PATH helpers.</li>
              <li><code>install.sh</code> and <code>uninstall.sh</code>: optional Linux/macOS user PATH helpers generated by the cross-platform package driver.</li>
              <li><code>lullaby-alpha1-windows-x64.zip.sha256</code> or <code>*.sha256</code>: SHA-256 checksum for the archive.</li>
            </ul>
            """,
        ),
        (
            "Diagnostics",
            "diagnostics",
            """
            <p>Diagnostics use stable <code>N####</code> codes and support concise, verbose, and JSON output.</p>
            <ul>
              <li><code>--verbose</code> includes source excerpts, root cause, suggested fix, and runtime traceback when available.</li>
              <li><code>--format json</code> and <code>--diagnostic-format json</code> produce deterministic machine-readable diagnostics.</li>
              <li>See <code>documents/diagnostic_registry.md</code> in this generated bundle for the registry source.</li>
              <li>Current examples include <code>N0324</code>, <code>N0326</code>, <code>N0327</code>, <code>N0328</code>, <code>N0329</code>, <code>N0211</code>, <code>N0501</code>, <code>N0502</code>, <code>N0601</code>, <code>N0413</code>, <code>N0414 [resource]</code>, <code>N0415 [resource]</code>, and <code>N0416 [resource]</code>.</li>
            </ul>
            <pre><code>{"status":"error","diagnostics":[{"code":"N0313","phase":"semantic","severity":"error","root_cause":"The argument expression type does not match the parameter type.","suggested_fix":"Pass a value of the expected type or change the called function signature.","traceback":[]}]}</code></pre>
            """,
        ),
        (
            "Current Limitations",
            "limitations",
            """
            <ul>
              <li>No native code generation yet. Execution currently supports AST, typed IR, an instruction-bytecode backend, and versioned <code>.lbc</code> bytecode artifacts. The optimizer currently exposes opt-in constant folding, conservative common subexpression elimination, conservative loop-invariant motion, conservative block-local copy propagation, block-local dead-code elimination, and the combined alpha pipeline.</li>
              <li>Version 5 <code>.lbc</code> artifacts preserve Alpha 1 memory operation metadata and sequence numbers in <code>memory_operations</code>; the full region memory model, ARC/reference counting, compiler-inserted cleanup, and lifetime analysis remain planned.</li>
              <li>Modules, imports, structs, try/catch, packages, and advanced generics are planned syntax and are rejected with <code>N0211</code> until implemented.</li>
              <li>Cross-platform portable package generation exists with platform PATH helpers, but release assets still need non-Windows host validation and active CI workflow runs.</li>
              <li>The Windows Alpha 1 package now generates offline docs during packaging; the checked-in hand-authored page remains as a maintained source-era reference until it is retired.</li>
            </ul>
            """,
        ),
        (
            "Maintainers",
            "maintainers",
            """
            <p>Keep the generated and shipped documentation updated whenever user-facing syntax, semantics, CLI behavior, examples, diagnostics, installation, or toolchain packaging changes. The entry point must remain self-contained and openable directly from disk.</p>
            <p>Verification commands: <code>python offline_docs/verify_offline_docs.py</code>, <code>python offline_docs/generate_offline_docs.py</code>, <code>python offline_docs/verify_offline_docs.py target/offline_docs/index.html --profile generated</code>, <code>python scripts/package_portable.py --verify</code>, and <code>powershell -ExecutionPolicy Bypass -File scripts\\verify_release.ps1</code>.</p>
            """,
        ),
    ]


def render_examples_section() -> str:
    parts = [
        '<section id="executable-examples">',
        "<h1>Executable Examples</h1>",
        "<p>These examples are copied from repository fixtures and verified by the offline docs verifier.</p>",
    ]
    for title, fixture, mode in EXAMPLE_FIXTURES:
        if not fixture.exists():
            raise FileNotFoundError(f"required example fixture not found: {fixture}")
        relative = fixture.relative_to(REPO_ROOT).as_posix()
        source = html.escape(fixture.read_text(encoding="utf-8").strip())
        parts.append(f"<h2>{html.escape(title)}</h2>")
        parts.append(f'<p>Fixture: <code>{html.escape(relative)}</code></p>')
        parts.append(
            f'<pre data-fixture="{html.escape(relative)}" data-mode="{html.escape(mode)}"><code>{source}</code></pre>'
        )
    parts.append("</section>")
    return "".join(parts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "output",
        nargs="?",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=f"output HTML path, default: {DEFAULT_OUTPUT}",
    )
    args = parser.parse_args()

    output = args.output
    if not output.is_absolute():
        output = REPO_ROOT / output
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(render_document(), encoding="utf-8", newline="\n")
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

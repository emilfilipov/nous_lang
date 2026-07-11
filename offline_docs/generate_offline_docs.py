"""Generate a self-contained Lullaby documentation HTML bundle from Markdown.

This is the initial generator path for Epic 1.5. It intentionally uses only the
Python standard library so the docs build can run in release and installer
verification environments without a package manager.
"""

from __future__ import annotations

import argparse
import base64
import html
import re
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = REPO_ROOT / "target" / "offline_docs" / "index.html"
BRAND_DIR = REPO_ROOT / "assets" / "brand"

SOURCE_DOCS = [
    ("Project Overview", REPO_ROOT / "README.md"),
    ("Core Language Rules", REPO_ROOT / "documents" / "core_language_rules.md"),
    ("Earliest Installable Language Surface", REPO_ROOT / "documents" / "language_surface.md"),
    ("Diagnostics Registry", REPO_ROOT / "documents" / "diagnostic_registry.md"),
    ("Release Notes", REPO_ROOT / "documents" / "release_notes.md"),
    ("Roadmap", REPO_ROOT / "documents" / "roadmap.md"),
]

EXAMPLE_FIXTURES = [
    ("Quick Start", REPO_ROOT / "tests" / "fixtures" / "valid" / "docs_quick_start.lby", "run"),
    ("Calculator", REPO_ROOT / "examples" / "valid" / "calculator.lby", "run"),
    ("Arrays And Control Flow", REPO_ROOT / "examples" / "valid" / "arrays_control_flow.lby", "run"),
    ("Math Builtins", REPO_ROOT / "tests" / "fixtures" / "valid" / "docs_math.lby", "run"),
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


# Branded offline-docs stylesheet (Lullaby visual identity: warm & friendly, soft
# pastel, Nunito). Plain string with literal braces — injected whole into the head,
# so it never touches the surrounding f-string's fields. Light and dark are both
# designed; no remote dependencies (the font is embedded as a data URI below).
_DOCS_CSS = """
:root{
  color-scheme: light dark;
  --bg:#fef9f3; --panel:#ffffff; --ink:#372a54; --muted:#7c6fa6; --line:#efe7f7;
  --lav:#8b6ff0; --lav-050:#f4f0ff; --peach:#fecaca; --sky:#bae6fd;
  --code-bg:#201a33; --code-fg:#f3eefe;
  --header-grad:linear-gradient(120deg,#f4f0ff,#eaf6ff 58%,#fdeeee);
  --sans:'Nunito',ui-rounded,'Segoe UI',system-ui,-apple-system,sans-serif;
  --mono:'Cascadia Code','JetBrains Mono',ui-monospace,Consolas,monospace;
}
@media (prefers-color-scheme:dark){
  :root{ --bg:#161020; --panel:#20182f; --ink:#f1ecff; --muted:#b6a8db;
    --line:#2e2440; --lav:#c9bafd; --lav-050:#241c38; --code-bg:#0f0b1a;
    --header-grad:linear-gradient(120deg,#1f1836,#141a2b 58%,#241a2a); }
}
*{box-sizing:border-box}
body{margin:0; font-family:var(--sans); line-height:1.62; color:var(--ink); background:var(--bg);}
a{color:var(--lav); text-decoration:none; font-weight:600}
a:hover{text-decoration:underline}
a:focus-visible{outline:2px solid var(--lav); outline-offset:2px; border-radius:4px}
.lb-header{display:flex; align-items:center; gap:16px; padding:1.7rem 2rem;
  background:var(--header-grad); border-bottom:1px solid var(--line);}
.lb-header .wordmark{flex:0 0 auto; font-family:var(--sans); font-weight:800;
  font-size:2.1rem; letter-spacing:-.03em; line-height:1; color:var(--lav)}
.lb-header h1{margin:0; font-size:1.45rem; font-weight:800; letter-spacing:-.02em}
.lb-header p{margin:.25rem 0 0; color:var(--muted); font-size:.92rem}
main{display:grid; grid-template-columns:minmax(13rem,16rem) minmax(0,1fr); gap:2rem;
  padding:1.8rem 2rem; max-width:82rem; margin:0 auto}
nav{position:sticky; top:1rem; align-self:start; max-height:calc(100vh - 2rem); overflow:auto}
nav ul{list-style:none; padding:0; margin:0}
nav li{margin:.12rem 0}
nav a{display:block; padding:.32rem .6rem; border-radius:9px; color:var(--muted); font-weight:600; font-size:.92rem}
nav a:hover{background:var(--lav-050); color:var(--ink); text-decoration:none}
section{max-width:76rem; margin-bottom:3rem}
h1,h2,h3{letter-spacing:-.01em; line-height:1.28}
section>h1{font-size:1.7rem; font-weight:800; padding-bottom:.4rem; border-bottom:2px solid var(--lav-050)}
p{max-width:70ch}
code{font-family:var(--mono); font-size:.9em; background:var(--lav-050); color:var(--ink); padding:.1rem .35rem; border-radius:6px}
pre{overflow-x:auto; padding:1rem 1.1rem; background:var(--code-bg); color:var(--code-fg); border-radius:12px; border:1px solid var(--line)}
pre code{background:none; color:inherit; padding:0}
table{border-collapse:collapse; margin:.9rem 0; width:100%; font-size:.94rem}
td{border:1px solid var(--line); padding:.5rem .6rem; vertical-align:top; text-align:left}
.source{color:var(--muted); font-size:.85rem}
.lb-footer{padding:1.5rem 2rem; border-top:1px solid var(--line); color:var(--muted); text-align:center; font-size:.9rem}
.lb-footer b{color:var(--lav); font-weight:800}
@media (max-width:760px){ main{display:block} nav{position:static; max-height:none} .lb-header{flex-wrap:wrap} }
"""

# The plain wordmark, inline for the header: the lowercase word "lullaby" in the
# bundled Nunito ExtraBold, tinted lavender (var(--lav)). No pictorial mark. The
# text carries the whole identity, so there is no SVG namespace URL to keep out of
# the bundle either.
_HEADER_MARK = '<span class="wordmark">lullaby</span>'


def render_brand() -> tuple[str, str, str]:
    """Return (style_block, favicon_link, header_html) for the branded bundle.

    The Nunito typeface and the favicon are embedded as data URIs so the bundle
    stays fully offline (no CDN, no remote font, no `@import`). Base64 encoding also
    keeps the SVG namespace URL out of the document as a literal string.
    """
    font_b64 = base64.b64encode((BRAND_DIR / "nunito.woff2").read_bytes()).decode("ascii")
    font_face = (
        "@font-face{font-family:'Nunito';font-style:normal;font-weight:400 800;"
        f"font-display:swap;src:url(data:font/woff2;base64,{font_b64}) format('woff2');}}"
    )
    style_block = font_face + _DOCS_CSS

    icon_b64 = base64.b64encode((BRAND_DIR / "lullaby-icon.svg").read_bytes()).decode("ascii")
    favicon_link = f'<link rel="icon" type="image/svg+xml" href="data:image/svg+xml;base64,{icon_b64}">'

    header_html = (
        '  <header class="lb-header">\n'
        f"    {_HEADER_MARK}\n"
        "    <div>\n"
        "      <h1>Lullaby Generated Offline Documentation</h1>\n"
        "      <p>Self-contained HTML generated from canonical repository Markdown.</p>\n"
        "    </div>\n"
        "  </header>"
    )
    return style_block, favicon_link, header_html


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
    for title, section_id, body in user_sections():
        nav_items.append(f'<li><a href="#{section_id}">{html.escape(title)}</a></li>')
        sections.append(f'<section id="{section_id}"><h1>{html.escape(title)}</h1>{body}</section>')

    style_block, favicon_link, header_html = render_brand()
    nav = "".join(nav_items)
    body = "".join(sections)
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Lullaby Generated Offline Documentation</title>
  {favicon_link}
  <style>{style_block}</style>
</head>
<body>
{header_html}
  <main>
    <nav aria-label="Documentation sections"><ul>{nav}</ul></nav>
    <div>{body}</div>
  </main>
  <footer class="lb-footer">Lullaby &mdash; <b>Serious systems code. Sweet dreams.</b></footer>
</body>
</html>
"""


def user_sections() -> list[tuple[str, str, str]]:
    return [
        (
            "Overview",
            "overview",
            """
            <p>Lullaby is implemented in Rust and in active development toward 1.0. It runs today through a clear, testable compiler frontend plus AST, typed IR, and instruction-bytecode execution paths, a scalar-subset WebAssembly backend, and an i64-scalar native x86-64 backend.</p>
            <p>The currently implemented language surface is recorded in <code>documents/language_surface.md</code>. Anything documented here is accepted by the compiler; features only in the broader Markdown design docs are planned material unless they appear on this page.</p>
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
              <li>Compile a bytecode artifact with <code>bin/lullaby compile --optimize full -o examples/valid/calculator.lbc examples/valid/calculator.lby</code>.</li>
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
            <p>Lullaby supports homogeneous arrays with <code>array&lt;T&gt;</code> type spelling, non-empty array literals, and bounds-checked indexing.</p>
            <ul>
              <li>Array literal values must all have the same type.</li>
              <li>Empty array literals are not accepted yet; use <code>list&lt;T&gt;</code> for growable collections.</li>
              <li>Index expressions require an <code>i64</code> index.</li>
              <li>Out-of-bounds indexes are runtime errors.</li>
            </ul>
            """,
        ),
        (
            "Control Flow",
            "control-flow",
            """
            <p>Lullaby supports <code>if</code>, <code>elif</code>, and <code>else</code> branches with boolean conditions.</p>
            <p><code>while</code> loops repeat while a boolean condition is true.</p>
            <p><code>for</code> loops iterate over inclusive <code>i64</code> ranges using <code>from</code>, <code>to</code>, and optional <code>by</code>. The default step is <code>1</code>, and runtime execution rejects step <code>0</code>.</p>
            <p><code>loop</code>, <code>break</code>, and <code>continue</code> support unconditional loops and loop control.</p>
            """,
        ),
        (
            "Boolean Logic",
            "boolean-logic",
            """
            <p>Lullaby supports boolean logic with <code>and</code>, <code>or</code>, and unary <code>not</code>. Logical operands must be <code>bool</code>, and <code>and</code>/<code>or</code> short-circuit during runtime execution.</p>
            """,
        ),
        (
            "Memory Builtins",
            "memory-builtins",
            """
            <p>Lullaby's runtime includes heap-slot builtins. These are an interim executable model while the full region/ARC memory design is still being implemented.</p>
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
            <p>Lullaby's runtime includes flat text file I/O builtins and a conservative system command abstraction. Dotted <code>io.*</code> APIs, buffered streams, stateful file handles, memory mapping, async, and IPC are planned rather than current syntax. Binary/directory file builtins and TCP/UDP sockets are also implemented (see the shipped page for the full surface).</p>
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
              <tr><td><code>cargo run -p lullaby_cli -- new my_app</code><br><code>lullaby new &lt;name&gt;</code></td><td>Scaffold a new project directory: a <code>lullaby.json</code> manifest, a runnable <code>src/main.lby</code>, and a <code>.gitignore</code>. The name must be a valid identifier and the directory must not already exist. Follow with <code>lullaby run &lt;name&gt;</code>.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- check path/to/file.lby</code><br><code>lullaby check [--verbose|--format json] file.lby</code></td><td>Validate extension, lex, parse, and run semantic checks. This can check helper/library-style functions without <code>main</code>.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- compile --optimize full -o path/to/file.lbc path/to/file.lby</code><br><code>lullaby compile [--optimize none|constant-fold|dead-code|full] -o file.lbc file.lby</code></td><td>Validate executable source with zero-argument main, lower through typed IR, run the current optimizer pipeline, and write a versioned <code>.lbc</code> instruction-bytecode artifact with metadata, a function table, ordered memory operation metadata, and dedicated function instructions.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- build --optimize full -o path/to/file.lbc path/to/file.lby</code><br><code>lullaby build</code></td><td>Use the same artifact-generation path as <code>compile</code> with a build-oriented command name.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- inspect path/to/file.lbc</code><br><code>lullaby inspect file.lbc</code></td><td>Print bytecode artifact metadata, function table details, memory operation counts, target, payload, entry point, and function count without executing the program; verbose/JSON modes include memory operation sequence numbers.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run path/to/file.lby</code><br><code>lullaby run [--backend ast|ir|bytecode] file.lby</code></td><td>Execute source through the selected backend. Use <code>--backend ir</code> or <code>--backend bytecode</code> to select typed IR or bytecode execution.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run path/to/file.lbc</code><br><code>lullaby run file.lbc</code></td><td>Execute a compiled bytecode artifact.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- native path/to/file.lby</code><br><code>lullaby native [--verbose] [--freestanding|--no-std] [--debug|-g] [-o out.exe] file.lby</code></td><td>Compile the i64-scalar subset to an x86-64 Windows COFF object and, best-effort, link it into a runnable <code>.exe</code>. <code>--freestanding</code> (alias <code>--no-std</code>) builds a no-C-runtime executable: it links <code>kernel32.lib</code> only (zero <code>ucrt</code>/<code>vcruntime</code>/<code>msvcrt</code>) and terminates through the minimal OS import <code>kernel32!ExitProcess</code>. It is still a Windows PE, not a bare-metal binary. A freestanding build that declares an <code>extern fn</code> (which needs the C runtime) is rejected with <code>L0426</code>. <code>--debug</code> (alias <code>-g</code>) emits native source-line debug info: a CodeView <code>.debug$S</code> section maps each compiled function's entry offset to its <code>.lby</code> declaration line (per-function granularity) so a debugger can break at a function and show its source line. <code>--debug</code> is opt-in; without it the object bytes are unchanged.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run --backend ir --optimize constant-fold path/to/file.lby</code></td><td>Run the IR backend with only the opt-in constant folding optimizer.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- run --backend bytecode --optimize dead-code path/to/file.lby</code></td><td>Run the bytecode backend with block-local dead-code elimination. Other optimizer coverage includes common subexpression elimination, loop-invariant motion, copy propagation, <code>--optimize full</code>, and <code>--optimize none</code>.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- docs</code><br><code>lullaby docs</code></td><td>Print the local offline documentation path.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- examples</code><br><code>lullaby examples</code></td><td>Print the packaged valid examples directory.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- check --verbose path/to/file.lby</code></td><td>Print source excerpts, caret markers, root cause, and suggested fix text for diagnostics.</td></tr>
              <tr><td><code>cargo run -p lullaby_cli -- check --format json path/to/file.lby</code></td><td>Print deterministic JSON diagnostics for tools, CI, editors, and LLM agents. <code>--diagnostic-format json</code> is also accepted.</td></tr>
              <tr><td><code>powershell -ExecutionPolicy Bypass -File scripts\\package_windows_portable.ps1</code></td><td>Build the Windows portable package with generated docs.</td></tr>
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
              <li><code>lullaby-windows-x64.zip.sha256</code> or <code>*.sha256</code>: SHA-256 checksum for the archive.</li>
            </ul>
            """,
        ),
        (
            "Diagnostics",
            "diagnostics",
            """
            <p>Diagnostics use stable <code>L####</code> codes and support concise, verbose, and JSON output.</p>
            <ul>
              <li><code>--verbose</code> includes source excerpts, root cause, suggested fix, and runtime traceback when available.</li>
              <li><code>--format json</code> and <code>--diagnostic-format json</code> produce deterministic machine-readable diagnostics.</li>
              <li>See <code>documents/diagnostic_registry.md</code> in this generated bundle for the registry source.</li>
              <li>Current examples include <code>L0324</code>, <code>L0326</code>, <code>L0327</code>, <code>L0328</code>, <code>L0329</code>, <code>L0211</code>, <code>L0501</code>, <code>L0502</code>, <code>L0601</code>, <code>L0413</code>, <code>L0414 [resource]</code>, <code>L0415 [resource]</code>, and <code>L0416 [resource]</code>.</li>
            </ul>
            <pre><code>{"status":"error","diagnostics":[{"code":"L0313","phase":"semantic","severity":"error","root_cause":"The argument expression type does not match the parameter type.","suggested_fix":"Pass a value of the expected type or change the called function signature.","traceback":[]}]}</code></pre>
            """,
        ),
        (
            "Current Limitations",
            "limitations",
            """
            <ul>
              <li>Execution runs on AST, typed IR, and instruction-bytecode interpreters plus versioned <code>.lbc</code> bytecode artifacts; a scalar-subset WebAssembly backend (<code>lullaby wasm</code>) and an i64-scalar native x86-64 backend (<code>lullaby native</code>) exist and cover only a subset, with non-eligible functions still running on the interpreters. Full native lowering of the whole language is still in progress. The optimizer currently exposes opt-in constant folding, conservative common subexpression elimination, conservative loop-invariant motion, conservative block-local copy propagation, block-local dead-code elimination, and the combined <code>--optimize full</code> pipeline.</li>
              <li>Version 5 <code>.lbc</code> artifacts preserve memory operation metadata and sequence numbers in <code>memory_operations</code>; the full region memory model, ARC/reference counting, compiler-inserted cleanup, and lifetime analysis remain planned.</li>
              <li>Modules and visibility (<code>import</code>/<code>pub</code>), structs, enums, pattern matching, traits with bounded generics, generic functions, structured error handling (<code>throw</code>/<code>try</code>/<code>catch</code>), multi-directory projects, and threads/channels/mutex concurrency are implemented. Still planned: capturing closures, trait objects and default methods, wider integer types (<code>i32</code>/<code>u32</code>), and byte arithmetic. Genuinely reserved keywords (<code>module</code>, <code>package</code>, <code>union</code>, <code>interface</code>, <code>class</code>, <code>switch</code>, standalone <code>catch</code>, <code>coroutine</code>) are rejected with <code>L0211</code> until implemented.</li>
              <li>Cross-platform portable package generation exists with platform PATH helpers, but release assets still need non-Windows host validation and active CI workflow runs.</li>
              <li>The Windows package now generates offline docs during packaging; the checked-in hand-authored page remains as a maintained reference until it is retired.</li>
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

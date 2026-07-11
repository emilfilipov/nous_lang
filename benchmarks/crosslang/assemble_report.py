#!/usr/bin/env python3
"""Assemble the self-contained benchmark artifact: inject the Nunito font
(base64 data URI) and the corpus/perf JSON into report_template.html, producing
report.html. JSON embedded in <script type="application/json"> is made safe by
escaping `</` so a `</script>` inside any code sample can't close the tag.

  python benchmarks/crosslang/assemble_report.py <template.html> <out.html>
"""
import base64
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent.parent


def main():
    template = Path(sys.argv[1])
    out = Path(sys.argv[2])
    html = template.read_text(encoding="utf-8")

    font_b64 = base64.b64encode((REPO / "assets" / "brand" / "nunito.woff2").read_bytes()).decode("ascii")
    corpus = (ROOT / "corpus_data.json").read_text(encoding="utf-8")
    perf = (ROOT / "perf_data.json").read_text(encoding="utf-8")

    def safe(js):  # keep valid JSON, but never emit a literal </script>
        return js.replace("</", "<\\/")

    html = html.replace("/*FONT_DATA*/", f"data:font/woff2;base64,{font_b64}")
    html = html.replace("/*CORPUS_DATA*/", safe(corpus))
    html = html.replace("/*PERF_DATA*/", safe(perf))

    out.write_text(html, encoding="utf-8")
    kb = len(html.encode("utf-8")) // 1024
    print(f"wrote {out} ({kb} KiB)")


if __name__ == "__main__":
    main()

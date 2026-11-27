"""Summarizes the localhost processes of saved Firefox memory reports.

Each `memory-report-*.json.gz` gets one block per process whose name
mentions localhost: resident and heap totals plus the layout and DOM
sizes a growing page leaves behind, in MB, and its top-level window
URLs.
"""

import argparse
import gzip
import json
import os
import re
import sys
from pathlib import Path

METRICS = [
    ("resident", lambda p: p == "resident"),
    ("peak", lambda p: p == "resident-peak"),
    ("heap", lambda p: p == "heap-allocated"),
    (
        "pres-arena",
        re.compile(r"^explicit/window-objects/.*/layout/pres-arena/").search,
    ),
    (
        "pres-shell",
        re.compile(r"^explicit/window-objects/.*/layout/pres-shell$").search,
    ),
    ("page-wasm", re.compile(r"^explicit/window-objects/.*elements/wasm$").search),
    ("worker-wasm", re.compile(r"^explicit/workers/.*elements/wasm$").search),
    (
        "element-nodes",
        re.compile(r"^explicit/window-objects/.*/dom/element-nodes$").search,
    ),
    (
        "text-nodes",
        re.compile(r"^explicit/window-objects/.*/dom/text-nodes$").search,
    ),
    (
        "orphan-nodes",
        re.compile(r"^explicit/window-objects/.*/dom/orphan-nodes$").search,
    ),
    (
        "js-other",
        lambda p: (
            p.startswith("explicit/window-objects/")
            and "/js-realm" in p
            and not p.endswith("elements/wasm")
        ),
    ),
]

TOP_WINDOW = re.compile(r"^explicit/window-objects/top\(([^,]*)")


def mb(reports: list[dict], want) -> int:
    """The reports' amounts for paths `want` accepts, in rounded MB."""
    total = sum(r["amount"] for r in reports if want(r["path"]))
    # jq's round goes half away from zero; amounts are never negative.
    return int(total / 1e6 + 0.5)


def summarize(data: dict) -> list[str]:
    """One output block per localhost process in a report's JSON."""
    by_process: dict[str, list[dict]] = {}
    for report in data.get("reports", []):
        if "localhost" in report.get("process", ""):
            by_process.setdefault(report["process"], []).append(report)
    blocks = []
    for process in sorted(by_process):
        reports = by_process[process]
        values = {name: mb(reports, want) for name, want in METRICS}
        urls = sorted(
            {
                m.group(1)
                for r in reports
                for m in [TOP_WINDOW.match(r["path"])]
                if m and "localhost" in m.group(1)
            }
        )
        blocks.append(
            f"  {process}\n"
            f"    resident={values['resident']}MB peak={values['peak']}MB "
            f"heap={values['heap']}MB\n"
            f"    pres-arena={values['pres-arena']}MB "
            f"pres-shell={values['pres-shell']}MB\n"
            f"    page-wasm={values['page-wasm']}MB "
            f"worker-wasm={values['worker-wasm']}MB\n"
            f"    element-nodes={values['element-nodes']}MB "
            f"text-nodes={values['text-nodes']}MB "
            f"orphan-nodes={values['orphan-nodes']}MB\n"
            f"    js-other={values['js-other']}MB\n"
            f"    windows: {' '.join(urls)}"
        )
    return blocks


def report_files(paths: list[str]) -> list[Path]:
    """Every `*.json.gz` named directly or inside a named directory."""
    base = Path(os.environ.get("BUILD_WORKING_DIRECTORY", "."))
    files = []
    for name in paths:
        p = Path(name)
        if not p.is_absolute():
            p = base / p
        if p.is_dir():
            files.extend(sorted(p.glob("*.json.gz")))
        else:
            files.append(p)
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "reports",
        nargs="+",
        help="memory-report-*.json.gz files, or directories holding them",
    )
    args = parser.parse_args()
    for path in report_files(args.reports):
        print(f"== {path.name}")
        with gzip.open(path, "rt") as f:
            data = json.load(f)
        for block in summarize(data):
            print(block)
    return 0


if __name__ == "__main__":
    sys.exit(main())

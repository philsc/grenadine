import gzip
import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from tools.firefox_memory import summarize


def report(process: str, entries: list[tuple[str, int]]) -> dict:
    return {
        "reports": [
            {"process": process, "path": path, "amount": amount}
            for path, amount in entries
        ]
    }


class SummarizeTest(unittest.TestCase):
    def test_sums_the_localhost_processes_only(self) -> None:
        data = {
            "reports": [
                *report(
                    "Web Content (pid 1, http://localhost:8000/)",
                    [
                        ("resident", 2_000_000),
                        ("resident-peak", 3_000_000),
                        ("heap-allocated", 1_000_000),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/layout/pres-arena/x",
                            500_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/layout/pres-shell",
                            250_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/dom/element-nodes",
                            100_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/dom/text-nodes",
                            50_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/dom/orphan-nodes",
                            25_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/elements/wasm",
                            400_000,
                        ),
                        (
                            "explicit/workers(chrome-worker)/elements/wasm",
                            60_000,
                        ),
                        (
                            "explicit/window-objects/top(http://localhost:8000/, id=1)/js-realm(a)/objects",
                            70_000,
                        ),
                    ],
                )["reports"],
                *report("Elsewhere (pid 2)", [("resident", 9_000_000)])["reports"],
            ]
        }
        blocks = summarize.summarize(data)
        self.assertEqual(len(blocks), 1)
        block = blocks[0]
        self.assertIn("localhost", block)
        self.assertIn("resident=2MB peak=3MB heap=1MB", block)
        self.assertIn("pres-arena=1MB pres-shell=0MB", block)
        self.assertIn("page-wasm=0MB worker-wasm=0MB", block)
        self.assertIn("element-nodes=0MB text-nodes=0MB orphan-nodes=0MB", block)
        self.assertIn("windows: http://localhost:8000/", block)
        self.assertNotIn("Elsewhere", block)

    def test_report_files_expands_directories(self) -> None:
        with TemporaryDirectory() as d:
            d = Path(d)
            for name in ("b.json.gz", "a.json.gz", "skip.txt"):
                (d / name).write_bytes(b"x")
            files = summarize.report_files([str(d)])
            self.assertEqual([f.name for f in files], ["a.json.gz", "b.json.gz"])

    def test_main_reads_gzip(self) -> None:
        with TemporaryDirectory() as d:
            path = Path(d) / "memory-report-x.json.gz"
            with gzip.open(path, "wt") as f:
                json.dump(
                    {
                        "reports": [
                            {
                                "process": "proc (http://localhost/)",
                                "path": "resident",
                                "amount": 5_000_000,
                            }
                        ]
                    },
                    f,
                )
            with gzip.open(path, "rt") as f:
                self.assertIn("resident", f.read())


if __name__ == "__main__":
    unittest.main()

import os
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from tools.firefox_memory import capture


def fake_proc(root: Path, pid: int, comm: str, cmdline: str, start: int = 1) -> None:
    d = root / str(pid)
    d.mkdir()
    (d / "comm").write_text(f"{comm}\n")
    (d / "cmdline").write_bytes(cmdline.replace(" ", "\0").encode() + b"\0")
    (d / "stat").write_text(f"{pid} ({comm}) S {' '.join(['0'] * 18)} {start} 0")


class CaptureTest(unittest.TestCase):
    def test_finds_the_parent_firefox_process(self) -> None:
        with TemporaryDirectory() as d:
            root = Path(d)
            fake_proc(root, 10, "firefox", "/usr/lib/firefox/firefox", start=5)
            fake_proc(root, 20, "firefox", "firefox -contentproc -childID 1", start=9)
            fake_proc(root, 30, "bash", "bash", start=1)
            self.assertEqual(capture.find_firefox(root), 10)

    def test_picks_the_oldest_parent(self) -> None:
        with TemporaryDirectory() as d:
            root = Path(d)
            fake_proc(root, 50, "firefox", "firefox", start=9)
            fake_proc(root, 40, "firefox-bin", "firefox-bin", start=2)
            self.assertEqual(capture.find_firefox(root), 40)
            fake_proc(root, 60, "firefox-esr", "firefox-esr", start=1)
            self.assertEqual(capture.find_firefox(root), 60)

    def test_no_firefox_means_none(self) -> None:
        with TemporaryDirectory() as d:
            self.assertIsNone(capture.find_firefox(Path(d)))

    def test_wait_for_dump_ignores_old_and_incomplete(self) -> None:
        with TemporaryDirectory() as d:
            tmp = Path(d)
            old = tmp / "memory-report-old.json.gz"
            old.write_bytes(b"old")
            os.utime(old, (time.time() - 100, time.time() - 100))
            (tmp / "incomplete-x.json.gz").write_bytes(b"partial")
            new = tmp / "memory-report-new.json.gz"
            new.write_bytes(b"new")
            got = capture.wait_for_dump(tmp, time.time() - 10, timeout=5)
            self.assertEqual(got.name, "memory-report-new.json.gz")

    def test_wait_for_dump_times_out(self) -> None:
        with TemporaryDirectory() as d, self.assertRaises(TimeoutError):
            capture.wait_for_dump(Path(d), 0, timeout=0.1)


if __name__ == "__main__":
    unittest.main()

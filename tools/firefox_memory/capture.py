"""Captures Firefox about:memory reports into timestamped files.

Finds the Firefox parent process and signals it the way about:memory's
"Measure and save" does: SIGRTMIN writes a full report, SIGRTMIN+1 one
taken after minimizing memory first. Each run's reports land in `--out`
as `<timestamp>-plain.json.gz` and `<timestamp>-minimized.json.gz`.

By default a pair is taken every 60 minutes until Ctrl-C; `--hours`
bounds the run. Firefox installed as a snap or flatpak writes its
reports into a private temp dir; pass it with `--tmp-dir`.
"""

import argparse
import os
import signal
import sys
import tempfile
import time
from pathlib import Path


def processes(proc_root: Path) -> list[tuple[int, str, str]]:
    """(pid, comm, cmdline) for every readable process under `proc_root`."""
    out = []
    for entry in proc_root.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            comm = (entry / "comm").read_text().strip()
            cmdline = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode()
        except OSError:
            continue
        out.append((int(entry.name), comm, cmdline))
    return out


def find_firefox(proc_root: Path) -> int | None:
    """The oldest Firefox parent process's pid, or None.

    Content processes carry -contentproc on their command line; the
    oldest remaining candidate is the browser process.
    """
    candidates = [
        pid
        for pid, comm, cmdline in processes(proc_root)
        if comm in ("firefox", "firefox-bin", "firefox-esr")
        and "-contentproc" not in cmdline
    ]
    if not candidates:
        return None
    return min(candidates, key=lambda pid: _start_time(pid, proc_root) or 0)


def _start_time(pid: int, proc_root: Path) -> int | None:
    """The process's start time in jiffies after boot (stat's field 22)."""
    try:
        after_comm = (proc_root / str(pid) / "stat").read_text().rpartition(")")[2]
        return int(after_comm.split()[19])
    except (OSError, IndexError, ValueError):
        return None


def wait_for_dump(tmp_dir: Path, since: float, timeout: float = 300.0) -> Path:
    """The next finished memory report in `tmp_dir`, written after `since`.

    Firefox writes `incomplete-…` first and renames it when done, so only
    a `memory-report-*.json.gz` whose size stays the same across two
    polls counts.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        new = [
            p
            for p in tmp_dir.glob("memory-report-*.json.gz")
            if not p.name.startswith("incomplete-") and p.stat().st_mtime >= since
        ]
        for p in sorted(new):
            size = p.stat().st_size
            time.sleep(0.5)
            if p.exists() and p.stat().st_size == size:
                return p
        time.sleep(1)
    raise TimeoutError(f"no memory report appeared in {tmp_dir} within {timeout:.0f}s")


def capture_round(pid: int, out_dir: Path, tmp_dir: Path, stamp: str) -> list[Path]:
    """Signals `pid` for a full and a minimized report and files them."""
    saved = []
    for sig, suffix in ((signal.SIGRTMIN, "plain"), (signal.SIGRTMIN + 1, "minimized")):
        since = time.time()
        os.kill(pid, sig)
        dump = wait_for_dump(tmp_dir, since)
        dest = out_dir / f"{stamp}-{suffix}.json.gz"
        dest.write_bytes(dump.read_bytes())
        dump.unlink()
        saved.append(dest)
        print(dest)
    return saved


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        required=True,
        type=Path,
        help="Directory the timestamped reports land in (created).",
    )
    parser.add_argument(
        "--every",
        type=float,
        default=60,
        help="Minutes between captures (default: 60).",
    )
    parser.add_argument(
        "--hours",
        type=float,
        default=None,
        help="Stop after this many hours (default: run until Ctrl-C).",
    )
    parser.add_argument("--pid", type=int, default=None, help="Firefox pid override.")
    parser.add_argument(
        "--tmp-dir",
        type=Path,
        default=Path(tempfile.gettempdir()),
        help="Where Firefox writes memory reports (default: $TMPDIR or /tmp). "
        "Snap and flatpak builds use a private temp dir.",
    )
    args = parser.parse_args()

    out_dir = args.out
    if not out_dir.is_absolute() and os.environ.get("BUILD_WORKING_DIRECTORY"):
        out_dir = Path(os.environ["BUILD_WORKING_DIRECTORY"]) / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    pid = args.pid or find_firefox(Path("/proc"))
    if pid is None:
        print(
            "no Firefox process found in /proc; pass --pid, or --tmp-dir if "
            "Firefox is a snap/flatpak and reports land elsewhere",
            file=sys.stderr,
        )
        return 1

    started = time.monotonic()
    try:
        while True:
            stamp = time.strftime("%Y%m%d-%H%M%S")
            try:
                capture_round(pid, out_dir, args.tmp_dir, stamp)
            except TimeoutError as e:
                print(
                    f"{e}; a snap or flatpak Firefox uses a private temp dir, "
                    "point --tmp-dir at it",
                    file=sys.stderr,
                )
                return 1
            if (
                args.hours is not None
                and time.monotonic() - started >= args.hours * 3600
            ):
                return 0
            time.sleep(args.every * 60)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())

"""Browser tests for grenadine's web UI.

Each run starts the real server against an empty local clone. The clone's
remote names a GitHub repository, but the sandbox has no network, so syncing
fails and the UI shows no PRs. A stub `gh` hands the server a dummy token so
that it starts at all.
"""

import os
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

# How long the server may take to start listening.
STARTUP_TIMEOUT = 30


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class UiTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        tmp = Path(tempfile.mkdtemp(dir=os.environ.get("TEST_TMPDIR")))

        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text("#!/bin/sh\necho dummy-token\n")
        gh.chmod(0o755)

        clone = tmp / "clone"
        subprocess.run(["git", "init", "--quiet", clone], check=True)
        subprocess.run(
            ["git", "-C", clone, "remote", "add", "origin", "https://github.com/test/test"],
            check=True,
        )

        port = free_port()
        cls.url = f"http://127.0.0.1:{port}/"
        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}")
        cls.server = subprocess.Popen(
            [
                os.environ["GRENADINE_SERVER"],
                f"--repo={clone}",
                f"--port={port}",
                f"--db={tmp / 'grenadine.db'}",
            ],
            env=env,
        )
        cls.addClassCleanup(cls.stop_server)
        cls.wait_for_server()

        cls.playwright = sync_playwright().start()
        cls.addClassCleanup(cls.playwright.stop)
        cls.browser = cls.playwright.chromium.launch(executable_path=os.environ["CHROME"])
        cls.addClassCleanup(cls.browser.close)

    @classmethod
    def stop_server(cls) -> None:
        cls.server.terminate()
        cls.server.wait(timeout=15)

    @classmethod
    def wait_for_server(cls) -> None:
        deadline = time.monotonic() + STARTUP_TIMEOUT
        while True:
            if cls.server.poll() is not None:
                raise RuntimeError(f"server exited with {cls.server.returncode}")
            try:
                with urllib.request.urlopen(cls.url):
                    return
            except (urllib.error.URLError, ConnectionError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)

    def setUp(self) -> None:
        self.page = self.browser.new_page()
        self.addCleanup(self.page.close)

    def test_shows_wordmark_top_left(self) -> None:
        self.page.goto(self.url)
        brand = self.page.locator(".topbar-brand")
        expect(brand).to_have_text("grenadine")

        box = brand.bounding_box()
        viewport = self.page.viewport_size
        assert box is not None and viewport is not None
        self.assertLess(box["x"], viewport["width"] / 4)
        self.assertLess(box["y"], 100)

    def test_theme_picker_switches_themes(self) -> None:
        # Auto follows the OS, so pin the OS to light to tell Auto from Dark.
        self.page.emulate_media(color_scheme="light")
        self.page.goto(self.url)
        html = self.page.locator("html")
        body = self.page.locator("body")
        picker = self.page.get_by_label("Theme")
        expect(picker).to_have_value("auto")
        expect(html).to_have_attribute("data-theme", "light")

        picker.select_option("dark")
        expect(html).to_have_attribute("data-theme", "dark")
        expect(body).to_have_css("background-color", "rgb(13, 17, 23)")

        picker.select_option("light")
        expect(html).to_have_attribute("data-theme", "light")
        expect(body).to_have_css("background-color", "rgb(255, 255, 255)")

        # Auto tracks the OS's scheme as it changes.
        picker.select_option("auto")
        expect(html).to_have_attribute("data-theme", "light")
        self.page.emulate_media(color_scheme="dark")
        expect(html).to_have_attribute("data-theme", "dark")

    def test_theme_survives_reload(self) -> None:
        self.page.emulate_media(color_scheme="light")
        self.page.goto(self.url)
        self.page.get_by_label("Theme").select_option("dark")

        self.page.reload()
        expect(self.page.get_by_label("Theme")).to_have_value("dark")
        expect(self.page.locator("html")).to_have_attribute("data-theme", "dark")


if __name__ == "__main__":
    unittest.main()

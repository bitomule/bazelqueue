"""PTY contracts through a real interactive shell; development-only Python."""
import json
import os
import pathlib
import pty
import select
import shlex
import signal
import subprocess
import tempfile
import time
import unittest

BINARY = pathlib.Path(os.environ["BAZELQUEUE_TEST_BINARY"]).resolve()
BACKEND = pathlib.Path(os.environ["BAZELQUEUE_TEST_FIXTURE"]).resolve()
PROMPT = b"BQ_PROMPT>"

class TerminalContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="bqt-", dir="/private/tmp")
        self.root = pathlib.Path(self.directory.name)
        self.state = self.root / "state"
        self.state.mkdir()
        (self.state / "pressure").write_text("1")
        (self.state / "config.toml").write_text(
            f'backend = "{BACKEND}"\ncpu_capacity = 8\nmemory_capacity_mib = 4096\n'
            'action_memory_mib = 2048\npressure_recovery_samples = 1\n')
        self.env = dict(os.environ, HOME=str(self.root), BAZELQUEUE_HOME=str(self.state), PS1=PROMPT.decode(), TERM="dumb")
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            os.chdir(self.root)
            os.execve("/bin/bash", ["bash", "--noprofile", "--norc", "-i"], self.env)
        self.buffer = b""
        self.wait_output(PROMPT)
        command = " ".join(map(shlex.quote, [str(BINARY), "exec", "--", str(BACKEND), "tty"]))
        os.write(self.master, command.encode() + b"\n")
        self.wait_output(b"tty:true")

    def wait_output(self, needle, start=0):
        deadline = time.monotonic() + 30
        while needle not in self.buffer[start:]:
            remaining = deadline - time.monotonic()
            self.assertGreater(remaining, 0, self.buffer.decode(errors="replace"))
            ready, _, _ = select.select([self.master], [], [], remaining)
            self.assertTrue(ready, self.buffer.decode(errors="replace"))
            self.buffer += os.read(self.master, 65536)

    def exit_code(self, expected):
        start = len(self.buffer)
        os.write(self.master, b"printf '__BQ_EXIT__:%s\\n' \"$?\"\n")
        self.wait_output(f"__BQ_EXIT__:{expected}".encode(), start)

    def tearDown(self):
        try:
            result = subprocess.run([BINARY, "status", "--json"], env=self.env, capture_output=True, timeout=5)
            snapshot = json.loads(result.stdout)
            for job in snapshot["jobs"]:
                for owner in (job.get("child"), job["request"]["owner"]):
                    if owner:
                        try:
                            os.kill(owner["pid"], signal.SIGCONT)
                            os.kill(owner["pid"], signal.SIGKILL)
                        except (ProcessLookupError, PermissionError):
                            pass
            os.kill(snapshot["daemon"]["pid"], signal.SIGTERM)
        except (OSError, subprocess.TimeoutExpired, ValueError):
            pass
        os.close(self.master)
        try:
            os.kill(self.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        deadline = time.monotonic() + 5
        while os.waitpid(self.pid, os.WNOHANG)[0] == 0:
            if time.monotonic() >= deadline:
                raise RuntimeError("fixture shell could not be reaped")
            select.select([], [], [], 0.05)
        self.directory.cleanup()

    def test_terminal_is_inherited_and_stdin_reaches_target(self):
        start = len(self.buffer)
        os.write(self.master, b"hello terminal\n")
        self.wait_output(b"echo:hello terminal", start)
        self.wait_output(PROMPT, start)
        self.exit_code(0)

    def test_ctrl_c_reaches_target_and_returns_interrupt(self):
        start = len(self.buffer)
        os.write(self.master, b"\x03")
        self.wait_output(PROMPT, start)
        self.exit_code(130)

    def test_ctrl_z_preserves_reservation_and_fg_completes(self):
        start = len(self.buffer)
        os.write(self.master, b"\x1a")
        self.wait_output(b"Stopped", start)
        self.wait_output(PROMPT, start)
        result = subprocess.run([BINARY, "status", "--json"], env=self.env, capture_output=True, check=True, timeout=5)
        self.assertEqual(sum(job["state"] == "running" for job in json.loads(result.stdout)["jobs"]), 1)
        os.write(self.master, b"fg\n")
        start = len(self.buffer)
        os.write(self.master, b"resumed\n")
        self.wait_output(b"echo:resumed", start)
        self.wait_output(PROMPT, start)
        self.exit_code(0)

if __name__ == "__main__":
    unittest.main()

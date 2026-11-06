"""Drive the real dashboard in a Unix terminal against a loopback board."""

import argparse
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import time

LEFT, RIGHT, UP, DOWN = (b"\x1b[D", b"\x1b[C", b"\x1b[A", b"\x1b[B")
ENTER = b"\r"
ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


@contextmanager
def running(command, **kwargs):
    process = subprocess.Popen(command, **kwargs)
    try:
        yield process
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)


def drain(master, duration):
    output = bytearray()
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        remaining = max(0, deadline - time.monotonic())
        if select.select([master], [], [], remaining)[0]:
            output.extend(os.read(master, 65536))
    return bytes(output)


def exercise_terminal(binary, port, directory, columns, rows):
    master, slave = pty.openpty()

    def resize():
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))

    try:
        resize()
        original = termios.tcgetattr(slave)
        environment = dict(os.environ, TERM="xterm-256color")
        environment.pop("NO_COLOR", None)
        command = [binary, "tui", "--host", "127.0.0.1", "--port", port]
        with running(command, cwd=directory, stdin=slave, stdout=slave, stderr=slave,
                     start_new_session=True, env=environment) as dashboard:

            def expect(label, key=b""):
                nonlocal columns
                if key:
                    os.write(master, key)
                # A resize forces a full repaint. Retry while the board connects or reboots.
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline and dashboard.poll() is None:
                    drain(master, 0.15)
                    columns ^= 1
                    resize()
                    dashboard.send_signal(signal.SIGWINCH)
                    rendered = b" ".join(ANSI.sub(b" ", drain(master, 0.2)).split())
                    if label in rendered:
                        return
                raise AssertionError(f"Dashboard did not show {label!r} after {key!r}")

            expect(b"firmware old")
            expect(b"Enter Run", DOWN)
            expect(b"Select board", UP)
            expect(b"Enter Run", DOWN)
            expect(b"Enter Live", RIGHT)
            expect(b"Enter Run", LEFT)
            expect(b"Firmware library", ENTER)
            expect(b"b.bin")
            expect(b"2/2", DOWN)
            expect(b"Update mock", ENTER)
            expect(b"Enter Run", ENTER)  # Cancel is selected by default.
            expect(b"firmware old")
            expect(b"Firmware library", ENTER)
            expect(b"Update mock", ENTER)
            expect(b"Update & reboot", RIGHT)
            expect(b"mock is running v1 from slot 0x310000", ENTER)
            os.write(master, b"q")
            drain(master, 0.2)
            assert dashboard.wait(timeout=5) == 0
            assert termios.tcgetattr(slave) == original, "terminal mode was not restored"
    finally:
        os.close(master)
        os.close(slave)


def exercise(binary, fixture, columns, rows, fault=None):
    with tempfile.TemporaryDirectory() as directory:
        images = Path(directory, "target", "ota")
        images.mkdir(parents=True)
        image = images / "a.bin"
        command = [fixture, image]
        if fault:
            command.append(fault)
        with running(command, stdout=subprocess.PIPE) as board:
            if not select.select([board.stdout], [], [], 5)[0]:
                raise AssertionError("Mock board did not publish its port")
            port = board.stdout.readline().decode().strip()
            assert port.isdecimal(), f"Invalid mock board port {port!r}"
            shutil.copyfile(image, images / "b.bin")
            exercise_terminal(binary, port, directory, columns, rows)
    print(f"PASS {columns}x{rows}: {fault or 'normal activation'}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dashboard", type=Path)
    parser.add_argument("fixture", type=Path)
    args = parser.parse_args()
    exercise(args.dashboard.resolve(), args.fixture.resolve(), 80, 24)
    exercise(args.dashboard.resolve(), args.fixture.resolve(), 224, 27, "lost-commit")

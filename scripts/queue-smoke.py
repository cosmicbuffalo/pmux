#!/usr/bin/env python3
"""Exercise real queue delivery on a throwaway HOME and tmux socket.
Run after cargo build: python3 scripts/queue-smoke.py
Requires tmux and Python 3; never connects to the normal pmux server.
"""
import concurrent.futures
import contextlib
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import shlex
import shutil
import struct
import subprocess
import tempfile
import termios
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/pmux"


def eventually(check, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("condition did not become true")


@contextlib.contextmanager
def scratch_dir(prefix):
    """A temporary directory, removed afterwards even while stragglers write.

    Killing the tmux server fires hooks that run short-lived pmux commands
    (snapshot, status-labels) writing into this HOME, so a single removal
    can race them and fail with "Directory not empty". Retry for a while.
    """
    path = tempfile.mkdtemp(prefix=prefix)
    try:
        yield path
    finally:
        for _ in range(50):
            try:
                shutil.rmtree(path)
                break
            except FileNotFoundError:
                break
            except OSError:
                time.sleep(0.1)
        else:
            shutil.rmtree(path)


def main():
    with scratch_dir("pmux-queue-smoke-") as temp:
        home = Path(temp)
        socket = "queue-smoke-" + uuid.uuid4().hex[:12]
        # The generated binding runs `${PMUX_BIN:-pmux}`; this server is started
        # without PMUX_BIN, so the test's own pmux must be first on PATH (rather
        # than whatever pmux happens to be installed on the machine).
        env = {
            **os.environ,
            "HOME": temp,
            "PATH": f"{temp}/.local/bin:{os.environ.get('PATH', '')}",
            "PMUX_SOCKET": socket,
            "TERM": "xterm-256color",
        }
        env.pop("TMUX", None)
        # pmux exports PMUX_BIN into every pane; never use the installed binary.
        env.pop("PMUX_BIN", None)
        env.pop("TMUX_PANE", None)
        worker = None
        daemon_pid = None
        clients = []

        def tmux(*args, check=True):
            return subprocess.run(["tmux", "-L", socket, "-f", "/dev/null", *args], env=env, text=True, capture_output=True, check=check)

        def cli(*args, text=None, check=True):
            return subprocess.run([str(BINARY), "queue", *args], env=env, input=text, text=True, capture_output=True, check=check)

        def batches():
            return json.loads(cli("list", "--json").stdout)

        def last():
            return batches()[-1]

        def start_worker():
            proc = subprocess.Popen([str(BINARY), "queue", "worker"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            lock_path = home / ".local/state/pmux/queues" / socket.encode().hex() / "worker.lock"

            def ready():
                # A worker exits cleanly when nothing is scheduled or another
                # worker already owns the queue; only a crash is a failure.
                code = proc.poll()
                if code is not None:
                    assert code == 0, proc.stderr.read().decode()
                    return True
                if not lock_path.exists():
                    return False
                with lock_path.open("r+") as handle:
                    try:
                        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        return True
                    return False
            eventually(ready)
            return proc

        capture = home / "capture.bin"
        receiver = home / "receiver.py"
        receiver.write_text("import os,sys,tty\ntty.setraw(0)\nos.write(1,b'\\x1b[?2004hREADY')\nwith open(sys.argv[1],'ab',buffering=0) as f:\n while True:\n  f.write(os.read(0,65536))\n")
        command = shlex.join(["python3", str(receiver), str(capture)])
        try:
            tmux("new-session", "-d", "-s", "agent", command)
            eventually(lambda: capture.exists())
            worker = start_worker()
            # Use the generated prefix binding, with a real attached popup client.
            (home / ".local/bin").mkdir(parents=True)
            (home / ".local/bin/pmux").symlink_to(BINARY)
            tmux("set", "-g", "prefix", "C-p")
            tmux("source-file", str(ROOT / "templates/queue-binding.conf"))
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 110, 0, 0))
            client = subprocess.Popen(["tmux", "-L", socket, "attach", "-t", "agent"], env=env, stdin=slave, stdout=slave, stderr=slave)
            clients.append(client)
            os.close(slave)

            def screen_contains(text, timeout=6):
                data = bytearray()
                plain = b""
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    # The attach client may not be connected yet; refresh-client
                    # exits 1 until it is, which is not a failure of the popup.
                    tmux("refresh-client", check=False)
                    if select.select([master], [], [], 0.1)[0]:
                        data.extend(os.read(master, 65536))
                        plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(data))
                        if text.encode() in plain:
                            return
                raise AssertionError(f"popup did not show {text!r}: {plain[-1500:]!r}")

            screen_contains("READY")
            # Keys typed before the server has registered the client can be
            # lost (seen on tmux 3.3a in CI), so wait until it is listed.
            eventually(lambda: tmux("list-clients", "-F", "#{client_name}", check=False).stdout.strip() != "")
            time.sleep(0.3)
            os.write(master, b"\x10q")
            screen_contains("Message queue", timeout=12)
            os.write(master, b"a")
            screen_contains("Add instruction")
            os.write(master, "\x1b[200~First line\nSecond 🦀 line\x1b[201~".encode())
            time.sleep(0.1)
            os.write(master, b"\x1b")
            eventually(lambda: len(batches()) == 1)
            assert last()["messages"] == ["First line\nSecond 🦀 line"]
            os.write(master, b"t")
            screen_contains("Send after")
            # Arrow-key picker: left to the day field, bump it, then back to the
            # hour and bump that too, so both halves of the carry are exercised.
            os.write(master, b"\x1b[D")          # hour -> day
            screen_contains("Editing day")
            os.write(master, b"\x1b[A")          # +1 day
            os.write(master, b"\x1b[C")          # back to hour
            screen_contains("Editing hour")
            os.write(master, b"\x1b[A")          # +1 hour
            os.write(master, b"\r")
            eventually(lambda: last()["status"] == "scheduled")
            armed = last()["after"]
            assert 24 * 3600 < armed - time.time() < 26 * 3600, armed
            os.write(master, b"q")
            cli("cancel", last()["id"])
            client.kill()
            client.wait(timeout=3)
            os.close(master)
            print("PASS prefix popup, multiline paste, arrow-key time picker", flush=True)

            first = "Continue work.\n\n```sh\n$HOME `literal`\n```\n"
            second = "Then test it 🦀."
            cli("add", "agent", "--stdin", text=first)
            cli("add", "agent", second)
            cli("move", "agent", "2", "1")
            cli("move", "agent", "1", "2")
            expected = cli("preview", "agent").stdout[:-1].encode()
            cli("schedule", "agent", "in 3s")
            assert capture.read_bytes() == b""
            tmux("rename-session", "-t", "agent", "renamed")
            eventually(lambda: last()["status"] == "sent")
            eventually(lambda: capture.read_bytes().endswith(b"\r"))
            assert capture.read_bytes() == b"\x1b[200~" + expected + b"\x1b[201~\r", repr(capture.read_bytes())
            print("PASS deferred combined delivery, exact bytes, one submit, session rename", flush=True)
            baseline = capture.read_bytes()
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                list(pool.map(lambda i: cli("add", "renamed", f"message {i}"), range(12)))
            assert len(last()["messages"]) == 12
            assert last()["status"] == "draft" and last()["after"] is None
            cli("schedule", "renamed", "in 2s")
            cli("pause", "renamed")
            time.sleep(3)
            assert capture.read_bytes() == baseline
            assert cli("resume", "renamed", check=False).returncode != 0
            cli("cancel", last()["id"])
            print("PASS concurrent additions, fresh batch, pause past deadline, cancellation", flush=True)

            cli("add", "renamed", "--after", "in 2s", "must not reach replacement")
            tmux("respawn-pane", "-k", "-t", "renamed", command)
            eventually(lambda: last()["status"] == "held")
            assert capture.read_bytes() == baseline
            cli("cancel", last()["id"])
            print("PASS process replacement holds delivery", flush=True)

            cli("add", "renamed", "--after", "in 3s", "after worker restart")
            worker.terminate(); worker.wait(timeout=3)
            worker = start_worker()
            eventually(lambda: last()["status"] == "sent")
            assert capture.read_bytes().endswith(b"\x1b[200~after worker restart\x1b[201~\r")
            baseline = capture.read_bytes()
            print("PASS durable scheduler restart", flush=True)

            cli("add", "renamed", "unconfirmed delivery")
            worker.terminate(); worker.wait(timeout=3)
            store_path = home / ".local/state/pmux/queues" / socket.encode().hex() / "batches.json"
            store = json.loads(store_path.read_text())
            store["batches"][-1]["status"] = "sending"
            store_path.write_text(json.dumps(store))
            worker = start_worker()
            eventually(lambda: last()["status"] == "uncertain")
            assert capture.read_bytes() == baseline
            print("PASS interrupted submission is never replayed", flush=True)

            # Exercise automatic detached launch instead of a test-owned worker.
            worker.terminate(); worker.wait(timeout=3)
            cli("add", "renamed", "--after", "in 3s", "automatic worker")
            lock_path = store_path.with_name("worker.lock")
            eventually(lambda: lock_path.read_text().strip() != str(worker.pid))
            daemon_pid = int(lock_path.read_text())
            eventually(lambda: last()["status"] == "sent")
            assert capture.read_bytes().endswith(b"\x1b[200~automatic worker\x1b[201~\r")
            print("PASS automatic detached worker launch", flush=True)
            baseline = capture.read_bytes()
            cli("add", "renamed", "--after", "in 3s", "must not survive server replacement")
            tmux("kill-server")
            eventually(lambda: tmux("new-session", "-d", "-s", "renamed", command, check=False).returncode == 0)
            eventually(lambda: last()["status"] == "held")
            assert capture.read_bytes() == baseline
            print("PASS server replacement cannot inherit old scheduled input", flush=True)
        finally:
            for client in clients:
                if client.poll() is None:
                    client.kill(); client.wait(timeout=3)
            if worker and worker.poll() is None:
                worker.terminate(); worker.wait(timeout=3)
            if daemon_pid is not None:
                try:
                    os.kill(daemon_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            tmux("kill-server", check=False)
            if daemon_pid is not None:
                # The detached worker isn't our child, so it can't be reaped;
                # wait for it to exit so it can't write into the queue store
                # while TemporaryDirectory is removing it.
                def daemon_gone():
                    try:
                        os.kill(daemon_pid, 0)
                    except ProcessLookupError:
                        return True
                    return False
                eventually(daemon_gone, seconds=5)


if __name__ == "__main__":
    main()

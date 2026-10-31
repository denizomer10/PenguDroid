#!/usr/bin/env python3
"""Bounded launch of one bundle (apk/apkm/xapk/apks) against a private daemon.

Usage: python3 -B tools/smoke_bundle.py <bundle> [SMOKE_SECONDS=45] [-- launch args...]

Writes daemon.log, app.log and result.json next to the bundle run directory
(target/bundle-smoke by default). Process groups are always reaped.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

repo = Path(__file__).resolve().parents[1]
work = Path(os.environ.get("SMOKE_DIR", repo / "target" / "bundle-smoke"))
bundle = Path(sys.argv[1]).resolve()
extra = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv[2:] else []
duration = int(os.environ.get("SMOKE_SECONDS", "45"))
socket = work / "daemon.sock"
env = dict(os.environ)
env.setdefault("PENGUDROID_SOCKET", str(socket))
processes = []
result = {"bundle": str(bundle), "runtime": "Android 17 / API 37 x86_64", "samples": []}


def sample(group):
    members = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            if os.getpgid(int(entry.name)) != group:
                continue
            data = {"pid": int(entry.name)}
            for line in (entry / "status").read_text().splitlines():
                key, _, value = line.partition(":")
                if key in ("Name", "VmRSS", "Threads"):
                    data[key] = value.strip()
            members.append(data)
        except (OSError, ValueError):
            pass
    return members


try:
    work.mkdir(parents=True, exist_ok=True)
    with (work / "daemon.log").open("wb") as dlog, (work / "app.log").open("wb") as alog:
        daemon = subprocess.Popen(
            [str(repo / "target/debug/pengudroid-daemon"), "--socket", str(socket)],
            cwd=repo, env=env, stdin=subprocess.DEVNULL,
            stdout=dlog, stderr=subprocess.STDOUT, start_new_session=True)
        processes.append(daemon)
        deadline = time.monotonic() + 5
        while not socket.exists():
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("private daemon socket unavailable")
            time.sleep(0.05)
        args = [str(repo / "target/debug/pengudroid"), "launch", str(bundle),
                "--backend", os.environ.get("SMOKE_BACKEND", "auto"),
                "--sysroot", str(repo / ".toolchain/sysroot-android17"),
                "--prefix", str(work / "prefix")] + extra
        app = subprocess.Popen(args, cwd=repo, env=env, stdin=subprocess.DEVNULL,
                               stdout=alog, stderr=subprocess.STDOUT, start_new_session=True)
        processes.append(app)
        started = time.monotonic()
        while time.monotonic() - started < duration:
            result["samples"].append({"elapsed": round(time.monotonic() - started, 2),
                                      "app": sample(app.pid)})
            if app.poll() is not None:
                break
            time.sleep(1)
        result.update(elapsed=round(time.monotonic() - started, 2), app_exit=app.poll(),
                      daemon_alive=daemon.poll() is None)
except Exception as error:
    result["error"] = str(error)
    raise
finally:
    for process in reversed(processes):
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    time.sleep(1)
    for process in reversed(processes):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)
    result["cleanup_complete"] = True
    (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "samples"}, indent=2))
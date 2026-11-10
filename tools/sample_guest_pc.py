#!/usr/bin/env python3
"""Sample the guest CPU's program counter with ptrace.

The translator can leave a guest thread spinning inside a single region, where
nothing gets logged and no syscall is made.  A Java stack dump (`kill -3`) shows
the topmost Java frame but not the native instruction, and a plain `gdb -p`
fails because `/proc/sys/kernel/yama/ptrace_scope` is 1 on most hosts.

Yama rule 1 only forbids tracing processes that are *not* descendants of the
tracer, so this tool does the opposite of the obvious thing: it becomes the
parent of the runner and attaches to the guest it spawns.  Each attach yields
the register file of every thread, which is exactly what is needed to name the
module and offset of a spin.

Usage:
    python3 tools/sample_guest_pc.py --samples 6 --interval 2 -- \
        python3 -B tools/smoke_bundle.py bundle.apkm
"""

from __future__ import annotations

import argparse
import ctypes
import os
import signal
import subprocess
import sys
import time

REGS = (
    "r15 r14 r13 r12 rbp rbx r11 r10 r9 r8 rax rcx rdx rsi rdi orig_rax "
    "rip cs eflags rsp ss fs_base gs_base ds es fs gs"
).split()


class UserRegs(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in REGS]


PTRACE_ATTACH = 16
PTRACE_DETACH = 17
PTRACE_GETREGS = 12

# Names a bundled app takes as its `/proc/<pid>/comm`.  The guest truncates
# `comm` to 15 bytes, so these are prefixes of the package name.
GUEST_COMM_PREFIXES = ("SDLActivity", "balt.googleplay", "pengudroid-app", "cobalt", "vectorunit")


def comm_of(pid: int) -> str:
    try:
        with open(f"/proc/{pid}/comm", encoding="utf-8", errors="replace") as handle:
            return handle.read().strip()
    except OSError:
        return ""


def matches_cmdline(pid: int, needle: str) -> bool:
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            return needle.encode() in handle.read()
    except OSError:
        return False


def descendants(root: int) -> set[int]:
    """Return every pid whose parent chain leads back to `root`."""
    parents: dict[int, int] = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", encoding="utf-8", errors="replace") as handle:
                stat = handle.read()
        except OSError:
            continue
        # The comm field is parenthesised and may contain spaces.
        fields = stat.rsplit(")", 1)[-1].split()
        if len(fields) > 1:
            try:
                parents[int(entry)] = int(fields[1])
            except ValueError:
                continue
    found: set[int] = set()
    for pid in parents:
        walk = pid
        for _ in range(64):
            walk = parents.get(walk, 0)
            if walk == root:
                found.add(pid)
                break
            if walk <= 1:
                break
    return found


def find_guests() -> list[tuple[int, str]]:
    """Return every process that is running guest code.

    Neither `comm` nor a command-line substring is enough: the host side of the
    runtime also carries the bundle path in its command line.  The reliable
    marker is the translatable guest libraries in the process address space,
    because only a guest maps them.
    """
    markers = os.environ.get(
        "PENGUDROID_SAMPLE_MAPS", "sysroot-android17,libhwui.so,base.apk"
    ).split(",")
    mine = descendants(os.getpid())
    found: list[tuple[int, str]] = []
    for pid in sorted(mine):
        comm = comm_of(pid)
        if not comm:
            continue
        paths = [path for _, _, _, path in read_maps(pid)]
        if not paths:
            continue
        if any(marker in path for marker in markers for path in paths):
            found.append((pid, comm))
    return found


def cmdline_of(pid: int) -> str:
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            return " ".join(part.decode("utf-8", "replace") for part in handle.read().split(b"\0") if part)
    except OSError:
        return ""


def read_maps(pid: int) -> list[tuple[int, int, int, str]]:
    ranges: list[tuple[int, int, int, str]] = []
    try:
        with open(f"/proc/{pid}/maps", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                fields = line.split()
                if len(fields) < 6:
                    continue
                start_s, end_s = fields[0].split("-")
                try:
                    start, end = int(start_s, 16), int(end_s, 16)
                    offset = int(fields[2], 16)
                except ValueError:
                    continue
                ranges.append((start, end, offset, fields[5]))
    except OSError:
        pass
    return ranges


def describe(addr: int, ranges: list[tuple[int, int, int, str]]) -> str:
    for start, end, offset, path in ranges:
        if start <= addr < end:
            # Load bias of an ELF mapping: mapping start minus file offset.
            return f"{path or 'anon'}+{addr - (start - offset):#x}"
    return "unmapped"


def sample(pid: int, libc) -> list[tuple[int, int, str]]:
    """Attach to each thread of `pid` and return (tid, rip, description)."""
    registers = UserRegs()
    found: list[tuple[int, int, str]] = []
    ranges = read_maps(pid)
    try:
        tids = [int(t) for t in os.listdir(f"/proc/{pid}/task")]
    except OSError:
        return found
    for tid in sorted(tids):
        if libc.ptrace(PTRACE_ATTACH, tid, None, None) != 0:
            continue
        try:
            os.waitpid(tid, 0)
            if libc.ptrace(PTRACE_GETREGS, tid, None, ctypes.byref(registers)) != 0:
                continue
            rip = int(registers.rip)
            found.append((tid, rip, describe(rip, ranges)))
        finally:
            # Detach without a signal so the thread resumes where it stopped.
            libc.ptrace(PTRACE_DETACH, tid, None, None)
    return found


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=5, help="how many rounds to take")
    parser.add_argument(
        "--stable",
        type=int,
        default=0,
        help="stop after the main thread's PC repeats this many rounds in a row",
    )
    parser.add_argument("--interval", type=float, default=2.0, help="seconds between rounds")
    parser.add_argument("--delay", type=float, default=0.0, help="wait before the first round")
    parser.add_argument("--out", default="", help="write samples here as well as to stdout")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="runner to launch after --")
    args = parser.parse_args(argv)

    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("a runner command is required after --")

    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    sink = open(args.out, "w", encoding="utf-8") if args.out else None

    def emit(line: str) -> None:
        print(line, flush=True)
        if sink is not None:
            sink.write(line + "\n")
            sink.flush()

    child = subprocess.Popen(command)
    emit(f"launched {command} as {child.pid}")
    if args.delay:
        time.sleep(args.delay)

    taken = 0
    previous: list[tuple[int, int]] = []
    repeated = 0
    guests: list[tuple[int, str]] = []
    deadline = time.time() + max(600.0, args.samples * (args.interval + 30.0))
    while taken < args.samples and time.time() < deadline:
        if child.poll() is not None:
            emit(f"runner exited with {child.returncode} before sampling finished")
            break
        fresh = find_guests()
        known = {pid for pid, _ in guests}
        for pid, comm in fresh:
            if pid not in known:
                emit(f"guest pid={pid} comm={comm} cmdline={cmdline_of(pid)[:400]}")
                if args.out:
                    with open(f"{args.out}.maps-{pid}", "w", encoding="utf-8") as handle:
                        with open(f"/proc/{pid}/maps", encoding="utf-8", errors="replace") as source:
                            handle.write(source.read())
        if fresh:
            guests = fresh
        if not guests:
            time.sleep(0.5)
            continue
        taken += 1
        alive = []
        main_pcs = []
        for pid, comm in guests:
            rounds = sample(pid, libc)
            if not rounds:
                emit(f"sample {taken}: guest {pid} vanished or refused ptrace")
                continue
            alive.append((pid, comm))
            for tid, rip, where in rounds:
                emit(f"sample {taken}: pid={pid} tid={tid} rip={rip:#x} {where}")
                if tid == pid:
                    main_pcs.append((pid, rip))
        guests = alive
        if not guests:
            time.sleep(0.5)
            continue
        if args.stable and main_pcs and main_pcs == previous:
            repeated += 1
            if repeated >= args.stable:
                emit(f"main thread unchanged for {repeated + 1} rounds; stopping")
                break
        else:
            repeated = 0
        previous = main_pcs
        if taken < args.samples:
            time.sleep(args.interval)

    if child.poll() is None:
        child.send_signal(signal.SIGTERM)
        try:
            child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
    if sink is not None:
        sink.close()
    return 0 if taken else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env python3
"""Bounded lifecycle reproducer; preserve output before killing a hung child."""
import argparse
import concurrent.futures
import os
from pathlib import Path
import signal
import subprocess
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("binary")
parser.add_argument("--count", type=int, default=30)
parser.add_argument("--parallel", type=int, default=1)
parser.add_argument("--timeout", type=int, default=45)
parser.add_argument("--out", required=True)
parser.add_argument("--sudo-gdb", action="store_true")
parser.add_argument("--continue-fast-failures", action="store_true")
args = parser.parse_args()
out = Path(args.out)
out.mkdir(parents=True, exist_ok=True)
stop = threading.Event()
test = "meta_node::meta_node_lifecycle::test_meta_node_join_with_state"


def capture_threads(pid, path):
    with path.open("w") as f:
        for task in sorted(Path(f"/proc/{pid}/task").glob("*")):
            for name in ("comm", "wchan", "stack"):
                try:
                    f.write(f"{task.name}/{name}: {task.joinpath(name).read_text()}\n")
                except OSError as e:
                    f.write(f"{task.name}/{name}: {e}\n")
        try:
            subprocess.run(
                (["sudo", "-n"] if args.sudo_gdb else []) +
                ["gdb", "-q", "-batch", "-ex", "set pagination off",
                 "-ex", "thread apply all bt", "-p", str(pid)],
                stdout=f, stderr=subprocess.STDOUT, timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired) as e:
            f.write(f"gdb: {e}\n")


def run(i):
    if stop.is_set():
        return "skipped"
    path = out / f"run-{i:03}.log"
    env = os.environ.copy()
    env["RUST_BACKTRACE"] = "1"
    start = time.monotonic()
    with path.open("w") as f:
        p = subprocess.Popen([args.binary, "--exact", test, "--nocapture"],
                             stdout=f, stderr=subprocess.STDOUT, env=env,
                             start_new_session=True)
        try:
            code = p.wait(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            stop.set()
            capture_threads(p.pid, out / f"run-{i:03}-threads.log")
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            code = "TIMEOUT"
    print(f"run={i} pid={p.pid} result={code} elapsed={time.monotonic()-start:.3f}s log={path}", flush=True)
    if code != 0:
        text = path.read_text(errors="replace")
        if (not args.continue_fast_failures or code == "TIMEOUT" or
                "LIFECYCLE TIMEOUT" in text):
            stop.set()
        lines = text.splitlines()
        phase = [line for line in lines if any(x in line for x in
                 ("LIFECYCLE", "STOP ", "panicked", "test result:", "shutdown"))]
        print("\n".join(phase[-60:]), flush=True)
    else:
        path.unlink()
    return code


with concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel) as pool:
    results = list(pool.map(run, range(1, args.count + 1)))
passed = results.count(0)
failed = sum(r not in (0, "skipped") for r in results)
print(f"SUMMARY passed={passed} failed={failed} skipped={results.count('skipped')}", flush=True)
raise SystemExit(1 if failed else 0)

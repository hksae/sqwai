#!/usr/bin/env python3
"""G0 matrix runner: 12 runs (3 tasks x 2 arms x 2 repeats), one at a time.

Resumable: a cell is done when bench/<task>/<arm>.eval.jsonl holds 2 valid
lines. Rerun this script after an interruption and it continues where it
stopped. A failed run (gate assert, timeout, crash) is logged and SKIPPED
forward — a red run is data, never a reason to stop the matrix or to
replace runs (see bench/prereg.md).

Usage:  python bench/run_matrix.py   (from the repo root)
Env set by the script itself: SQWAI_BENCH_DEBUG=1, SQWAI_BENCH_SUMMARY=short.
SQWAI_BENCH_THRESHOLD / SQWAI_BENCH_BASELINE are scrubbed: per-task windows
come from the code, arms from the test names.
"""

import datetime
import json
import os
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "bench")
LOGS = os.path.join(BENCH, "logs")
REPEATS = 2
RUN_TIMEOUT = 70 * 60  # harness wall cap is 60 min; give it slack
COOLDOWN = 30  # seconds between runs

TESTS = [
    ("T1", "mechanism", "bench_t1_mechanism"),
    ("T1", "baseline", "bench_t1_baseline"),
    ("T2", "mechanism", "bench_t2_mechanism"),
    ("T2", "baseline", "bench_t2_baseline"),
    ("T3", "mechanism", "bench_t3_mechanism"),
    ("T3", "baseline", "bench_t3_baseline"),
]


def cargo() -> str:
    found = shutil.which("cargo")
    if found:
        return found
    fallback = r"C:\Users\Asus\.cargo\bin\cargo.exe"
    if os.path.exists(fallback):
        return fallback
    sys.exit("cargo not found on PATH and no fallback exists")


def eval_lines(task: str, arm: str) -> int:
    path = os.path.join(BENCH, task, f"{arm}.eval.jsonl")
    try:
        with open(path, encoding="utf-8") as f:
            return sum(1 for line in f if line.strip())
    except OSError:
        return 0


def bench_running() -> bool:
    # another live run holds the model and the money; don't double-book
    try:
        out = subprocess.run(
            ["tasklist", "/FI", "IMAGENAME eq sqwai-*.exe", "/FO", "CSV"],
            capture_output=True, text=True, timeout=30,
        ).stdout
        return "sqwai-" in out and ".exe" in out
    except OSError:
        return False


def run_one(cargo_bin: str, task: str, arm: str, test: str, rep: int) -> bool:
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    log_path = os.path.join(LOGS, f"{task}-{arm}-{rep}-{stamp}.log")
    env = dict(os.environ)
    env["SQWAI_BENCH_DEBUG"] = "1"
    env["SQWAI_BENCH_SUMMARY"] = "short"
    env.pop("SQWAI_BENCH_THRESHOLD", None)  # per-task windows from code
    env.pop("SQWAI_BENCH_BASELINE", None)  # arms from test names
    cmd = [cargo_bin, "test", "--", "--ignored", test,
           "--test-threads=1", "--nocapture"]
    print(f"[{stamp}] {task}/{arm} repeat {rep}: {' '.join(cmd)}", flush=True)
    print(f"[{stamp}] log: {log_path}", flush=True)
    HEARTBEAT = 60
    POLL = 5
    start = time.time()
    last_hb = start
    offset = 0
    last_state = ""
    try:
        with open(log_path, "w", encoding="utf-8") as log:
            log.write(f"$ {' '.join(cmd)}\n\n")
            log.flush()
            proc = subprocess.Popen(
                cmd, cwd=ROOT, env=env,
                stdout=log, stderr=subprocess.STDOUT,
            )
            while True:
                try:
                    rc = proc.wait(timeout=POLL)
                    print(f"[{task}/{arm} #{rep}] "
                          f"{'PASS' if rc == 0 else 'FAIL rc=' + str(rc)}", flush=True)
                    return rc == 0
                except subprocess.TimeoutExpired:
                    pass
                elapsed = int(time.time() - start)
                if elapsed > RUN_TIMEOUT:
                    proc.kill()
                    print(f"[{task}/{arm} #{rep}] TIMEOUT after "
                          f"{RUN_TIMEOUT // 60} min (see log)", flush=True)
                    return False
                offset, last_state = watch_log(
                    log_path, offset, task, arm, rep, elapsed, last_state)
                if time.time() - last_hb >= HEARTBEAT:
                    last_hb = time.time()
                    try:
                        kb = os.path.getsize(log_path) // 1024
                    except OSError:
                        kb = 0
                    print(f"[{task}/{arm} #{rep}] +{elapsed}s alive, "
                          f"log {kb}KB{last_state}", flush=True)
    except OSError as e:
        print(f"[{task}/{arm} #{rep}] LAUNCH FAIL: {e}", flush=True)
        return False


def watch_log(path: str, offset: int, task: str, arm: str, rep: int,
              elapsed: int, last_state: str):
    """Print fresh compaction events; track last measured/msgs. Best-effort."""
    import re
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            f.seek(offset)
            new = f.read()
            offset = f.tell()
    except OSError:
        return offset, last_state
    for line in new.splitlines():
        if "triggered=true" in line:
            print(f"[{task}/{arm} #{rep}] +{elapsed}s {line.strip()[:110]}", flush=True)
        m = re.search(r"measured=(\d+).*msgs=(\d+)", line)
        if m:
            last_state = f", measured={m.group(1)} msgs={m.group(2)}"
    return offset, last_state


def summary() -> None:
    print("\n=== matrix status ===")
    total = done = 0
    for task, arm, _ in TESTS:
        have = eval_lines(task, arm)
        total += REPEATS
        done += min(have, REPEATS)
        print(f"{task}/{arm}: {min(have, REPEATS)}/{REPEATS}")
    print(f"total: {done}/{total}")


def main() -> int:
    if "--help" in sys.argv or "-h" in sys.argv or "--dry-run" in sys.argv:
        print(__doc__)
        if "--dry-run" in sys.argv:
            for task, arm, test in TESTS:
                print(f"{task}/{arm}: {min(eval_lines(task, arm), REPEATS)}/{REPEATS}  ({test})")
        return 0
    os.makedirs(LOGS, exist_ok=True)
    if bench_running():
        sys.exit("a sqwai bench process is already running — not double-booking")
    cargo_bin = cargo()
    failures = []
    for task, arm, test in TESTS:
        while eval_lines(task, arm) < REPEATS:
            rep = eval_lines(task, arm) + 1
            if not run_one(cargo_bin, task, arm, test, rep):
                failures.append(f"{task}/{arm} #{rep}")
            time.sleep(COOLDOWN)
    summary()
    if failures:
        print("\nnon-green runs (DATA, not reruns):", flush=True)
        for f in failures:
            print(f"  {f}")
    return 0


if __name__ == "__main__":
    main()

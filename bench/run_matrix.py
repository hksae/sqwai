#!/usr/bin/env python3
"""G1 matrix runner: 1 task x 2 arms x 2 models x 1 repeat, one at a time.

Cell identity is task/arm/MODEL (models differ per config: adjust MODELS
to your config keys). Resumable: a cell is done when its model has a line
in bench/<task>/<arm>.eval.jsonl. Rerun after interruption to continue.
A failed run is logged and SKIPPED forward - a red run is data, never a
reason to stop or to replace runs (see bench/prereg.md + bench/g1-plan.md).

Usage:  python bench/run_matrix.py   (from the repo root)
Env set by the script itself: SQWAI_BENCH_DEBUG=1, SQWAI_BENCH_SUMMARY=short,
SQWAI_BENCH_FIXTURE + SQWAI_BENCH_CONTEXT (G1 regime).
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
RUN_TIMEOUT = 130 * 60  # harness wall cap is 2h; give it slack
COOLDOWN = 30  # seconds between runs

# G1 regime (locked in bench/g1-plan.md)
FIXTURE = r"C:\Users\Asus\kaiwai-frozen"
CONTEXT = "256000"
# (model key as in YOUR config) x test names. Adjust keys, not structure.
CELLS = [
    # task, arm, model-key, test
    ("T4", "mechanism", "muse spark 1.3", "bench_t4_mechanism"),
    ("T4", "baseline", "muse spark 1.3", "bench_t4_baseline"),
    ("T4", "mechanism", "ms 1.2", "bench_t4_mechanism"),
    ("T4", "baseline", "ms 1.2", "bench_t4_baseline"),
]


def cargo() -> str:
    found = shutil.which("cargo")
    if found:
        return found
    fallback = r"C:\Users\Asus\.cargo\bin\cargo.exe"
    if os.path.exists(fallback):
        return fallback
    sys.exit("cargo not found on PATH and no fallback exists")


def eval_done(task: str, arm: str, model: str) -> bool:
    path = os.path.join(BENCH, task, f"{arm}.eval.jsonl")
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    if json.loads(line).get("model", "") == model:
                        return True
                except ValueError:
                    continue
    except OSError:
        pass
    return False


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


def run_one(cargo_bin: str, task: str, arm: str, model: str, test: str) -> bool:
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    tag = model.replace(" ", "")
    log_path = os.path.join(LOGS, f"{task}-{arm}-{tag}-{stamp}.log")
    env = dict(os.environ)
    env["SQWAI_BENCH_DEBUG"] = "1"
    env["SQWAI_BENCH_SUMMARY"] = "short"
    env["SQWAI_BENCH_FIXTURE"] = FIXTURE
    env["SQWAI_BENCH_CONTEXT"] = CONTEXT
    env["SQWAI_BENCH_MODEL"] = model
    env.pop("SQWAI_BENCH_THRESHOLD", None)  # per-task windows from code
    env.pop("SQWAI_BENCH_BASELINE", None)  # arms from test names
    cmd = [cargo_bin, "test", "--", "--ignored", test,
           "--test-threads=1", "--nocapture"]
    print(f"[{stamp}] {task}/{arm}/{model}: {' '.join(cmd)}", flush=True)
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
                    print(f"[{task}/{arm}/{model}] "
                          f"{'PASS' if rc == 0 else 'FAIL rc=' + str(rc)}", flush=True)
                    return rc == 0
                except subprocess.TimeoutExpired:
                    pass
                elapsed = int(time.time() - start)
                if elapsed > RUN_TIMEOUT:
                    proc.kill()
                    print(f"[{task}/{arm}/{model}] TIMEOUT after "
                          f"{RUN_TIMEOUT // 60} min (see log)", flush=True)
                    return False
                offset, last_state = watch_log(
                    log_path, offset, task, arm, model, elapsed, last_state)
                if time.time() - last_hb >= HEARTBEAT:
                    last_hb = time.time()
                    try:
                        kb = os.path.getsize(log_path) // 1024
                    except OSError:
                        kb = 0
                    print(f"[{task}/{arm}/{model}] +{elapsed}s alive, "
                          f"log {kb}KB{last_state}", flush=True)
    except OSError as e:
        print(f"[{task}/{arm}/{model}] LAUNCH FAIL: {e}", flush=True)
        return False


def watch_log(path: str, offset: int, task: str, arm: str, model: str,
              elapsed: int, last_state: str):
    """Print fresh compaction/retry events; track last measured/msgs."""
    import re
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            f.seek(offset)
            new = f.read()
            offset = f.tell()
    except OSError:
        return offset, last_state
    for line in new.splitlines():
        if "triggered=true" in line or "[bench-retry]" in line:
            print(f"[{task}/{arm}/{model}] +{elapsed}s {line.strip()[:120]}", flush=True)
        m = re.search(r"measured=(\d+).*msgs=(\d+)", line)
        if m:
            last_state = f", measured={m.group(1)} msgs={m.group(2)}"
    return offset, last_state


def summary() -> None:
    print("\n=== matrix status ===")
    total = done = 0
    for task, arm, model, _ in CELLS:
        total += 1
        have = eval_done(task, arm, model)
        done += 1 if have else 0
        print(f"{task}/{arm}/{model}: {'1/1' if have else '0/1'}")
    print(f"total: {done}/{total}")


def main() -> int:
    if "--help" in sys.argv or "-h" in sys.argv or "--dry-run" in sys.argv:
        print(__doc__)
        if "--dry-run" in sys.argv:
            for task, arm, model, test in CELLS:
                st = "1/1" if eval_done(task, arm, model) else "0/1"
                print(f"{task}/{arm}/{model}: {st}  ({test})")
        return 0
    os.makedirs(LOGS, exist_ok=True)
    if bench_running():
        sys.exit("a sqwai bench process is already running - not double-booking")
    cargo_bin = cargo()
    failures = []
    for task, arm, model, test in CELLS:
        if eval_done(task, arm, model):
            continue
        if not run_one(cargo_bin, task, arm, model, test):
            failures.append(f"{task}/{arm}/{model}")
        time.sleep(COOLDOWN)
    summary()
    if failures:
        print("\nnon-green runs (DATA, not reruns):", flush=True)
        for f in failures:
            print(f"  {f}")
    return 0


if __name__ == "__main__":
    main()

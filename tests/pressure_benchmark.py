#!/usr/bin/env python3
"""Opt-in ABBA compiler measurement. All state stays under --output-directory."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time

p = argparse.ArgumentParser()
p.add_argument("--binary", required=True, type=Path)
p.add_argument("--bazel", required=True, type=Path)
p.add_argument("--output-directory", required=True, type=Path)
a = p.parse_args()
binary, bazel = a.binary.resolve(), a.bazel.resolve()
root = a.output_directory.resolve()
root.mkdir(parents=True, exist_ok=True)
results = []

def gauges():
    vm = subprocess.check_output(["/usr/bin/vm_stat"], text=True)
    page = int(re.search(r"page size of (\d+)", vm)[1])
    counters = {k.strip().strip('"'): int(v) for k, v in re.findall(r"^([^:\n]+):\s+(\d+)\.", vm, re.M)}
    native = subprocess.check_output(["/usr/sbin/sysctl", "-n", "kern.memorystatus_vm_pressure_level", "kern.memorystatus_level", "vm.swapusage"], text=True).splitlines()
    return {"at": time.monotonic(), "pressure": int(native[0]), "free_percent": int(native[1]), "swapusage": native[2], "page_size": page, **counters}

for index, level in enumerate([1, 2, 2, 1]):
    work = root / str(index)
    work.mkdir()
    (work / "s").mkdir()
    (work / "s/pressure").write_text(str(level))
    (work / "s/config.toml").write_text(f'backend = {json.dumps(str(bazel))}\ncpu_capacity = 8\nmemory_capacity_mib = 9216\nmax_builds = 1\njobs = 64\naction_memory_mib = 6144\npressure_recovery_samples = 1\n')
    (work / "MODULE.bazel").write_text('module(name="pressure_calibration")\n')
    (work / ".bazelversion").write_text("8.4.2\n")
    source = "\n".join(f"__attribute__((noinline)) unsigned f{i}(unsigned x) {{ for(int j=0;j<20;++j) x=(x*1664525u+1013904223u)^((x>>3)+{i}u); return x; }}" for i in range(1800))
    (work / "work.c").write_text(source)
    (work / "BUILD.bazel").write_text("\n".join(f'genrule(name="compile{i}",srcs=["work.c"],outs=["out{i}.o"],tags=["local"],cmd="/usr/bin/clang -O2 -c $(location work.c) -o $@")' for i in range(16)))
    shim = work / "bazel"
    shim.symlink_to(binary)
    env = {**os.environ, "HOME": str(work), "BAZELQUEUE_HOME": "s", "BAZELQUEUE_FIXTURE_COORDINATOR": "1"}
    startup = ["--ignore_all_rc_files", f"--output_user_root={work / 'cache'}", "--host_jvm_args=-Xmx256m"]
    def control(*args):
        return subprocess.check_output([str(binary), *args], cwd=work, env=env, text=True)
    stop = threading.Event()
    samples = []
    def sample():
        while not stop.is_set():
            samples.append(gauges())
            stop.wait(1)
    daemon = None
    try:
        with open(work / "warm.log", "wb") as log:
            subprocess.run([str(bazel), *startup, "info", "output_base"], cwd=work, env=env, stdout=log, stderr=log, check=True)
        daemon = subprocess.Popen([str(binary), "daemon", "run"], cwd=work, env=env, stdout=subprocess.DEVNULL, stderr=open(work / "daemon.log", "wb"))
        thread = threading.Thread(target=sample)
        thread.start()
        before = time.monotonic()
        with open(work / "build.log", "wb") as log:
            result = subprocess.run([str(shim), *startup, "build", *[f"//:compile{i}" for i in range(16)], "--disk_cache=", "--noremote_accept_cached", "--profile=build-profile.json"], cwd=work, env=env, stdout=log, stderr=log)
        elapsed = time.monotonic() - before
        stop.set()
        thread.join()
        samples.append(gauges())
        snapshot = json.loads(control("status", "--json"))
        record = {"profile": "full" if level == 1 else "reduced", "injected_pressure": level, "elapsed_seconds": elapsed, "exit": result.returncode, "budgets": [j["request"]["budget"] for j in snapshot["jobs"]], "samples": samples}
        results.append(record)
        (root / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps({k: v for k, v in record.items() if k != "samples"}), flush=True)
        if result.returncode:
            raise RuntimeError((work / "build.log").read_text())
        expected = (8, 9216) if level == 1 else (2, 2304)
        if not record["budgets"] or any((b["cpu"], b["memory_mib"]) != expected for b in record["budgets"]):
            raise RuntimeError("reservation mismatch: this experiment requires the baseline v0.1.9 test-fixtures binary")
    finally:
        stop.set()
        if daemon is not None:
            daemon.send_signal(signal.SIGINT)
            daemon.wait(timeout=30)
        subprocess.run([str(bazel), *startup, "shutdown"], cwd=work, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

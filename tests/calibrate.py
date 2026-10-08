#!/usr/bin/env python3
"""Opt-in compiler throughput experiment; never edits the installed queue."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True, type=Path)
parser.add_argument("--bazel", required=True, type=Path)
parser.add_argument("--version", default="8.4.2")
parser.add_argument("--output", required=True, type=Path)
args = parser.parse_args()
results = []
with tempfile.TemporaryDirectory(prefix="bqc-", dir="/private/tmp") as directory:
    root = Path(directory)
    source = "\n".join(f"__attribute__((noinline)) unsigned f{i}(unsigned x) {{ for(int j=0;j<20;++j) x=(x*1664525u+1013904223u)^((x>>3)+{i}u); return x; }}" for i in range(1800))
    for cpu, parallel in [(8, 1), (8, 2), (10, 2)]:
        run = root / f"cpu{cpu}-parallel{parallel}"
        run.mkdir()
        state = run / "state"
        state.mkdir()
        (state / "config.toml").write_text(f'backend = {json.dumps(str(args.bazel.resolve()))}\ncpu_capacity = {cpu}\nmemory_capacity_mib = 9216\nmax_builds = {parallel}\njobs = {cpu}\naction_memory_mib = 6144\nworker_instances = 2\npressure_recovery_samples = 1\n')
        shim = run / "bazelisk"
        shim.symlink_to(args.binary.resolve())
        env = {**os.environ, "HOME": str(run), "BAZELQUEUE_HOME": str(state), "BAZELQUEUE_PROGRESS": "quiet"}
        control = lambda *command: subprocess.run([str(args.binary.resolve()), *command], env=env, check=True, capture_output=True)
        startup = ["--ignore_all_rc_files", f"--output_user_root={run / 'cache'}", "--host_jvm_args=-Xmx256m", "--max_idle_secs=5"]
        children = []
        logs = []
        try:
            control("drain")
            for label in ["one", "two"]:
                workspace = run / label
                workspace.mkdir()
                (workspace / "MODULE.bazel").write_text('module(name="compiler_calibration")\n')
                (workspace / ".bazelversion").write_text(args.version + "\n")
                (workspace / "work.c").write_text(source)
                (workspace / "BUILD.bazel").write_text("\n".join(f'genrule(name="compile{i}",srcs=["work.c"],outs=["out{i}.o"],tags=["local"],cmd="/usr/bin/clang -O2 -c $(location work.c) -o $@")' for i in range(8)))
                subprocess.run([str(args.bazel.resolve()), *startup, "info", "output_base"], cwd=workspace, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                log = open(run / f"{label}.log", "wb")
                logs.append(log)
                children.append(subprocess.Popen([str(shim), *startup, "build", "//..."], cwd=workspace, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log))
            deadline = time.monotonic() + 60
            while True:
                snapshot = json.loads(control("status", "--json").stdout)
                if sum(job["state"] == "queued" for job in snapshot["jobs"]) == 2:
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError("requests failed to reach the drained queue")
            pressure_before = subprocess.check_output(["/usr/sbin/sysctl", "-n", "kern.memorystatus_vm_pressure_level"], text=True).strip()
            start = time.monotonic()
            control("resume")
            for child in children:
                if child.wait(timeout=180) != 0:
                    raise RuntimeError("compiler experiment failed: " + "\n".join((run / f"{label}.log").read_text() for label in ["one", "two"]))
            elapsed = time.monotonic() - start
            pressure_after = subprocess.check_output(["/usr/sbin/sysctl", "-n", "kern.memorystatus_vm_pressure_level"], text=True).strip()
            result = {"cpu_capacity": cpu, "max_builds": parallel, "memory_capacity_mib": 9216, "cold_compilations": 16, "functions_per_translation_unit": 1800, "makespan_seconds": round(elapsed, 3), "pressure_before": pressure_before, "pressure_after": pressure_after}
            results.append(result)
            print(json.dumps(result), flush=True)
        finally:
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=20)
            for log in logs:
                log.close()
            try:
                snapshot = json.loads(control("status", "--json").stdout)
                os.kill(snapshot["daemon"]["pid"], 15)
            except (OSError, subprocess.SubprocessError):
                pass
args.output.write_text(json.dumps({"workload": "16 fresh clang -O2 genrule actions across two isolated workspaces; JVMs warm; native pressure; no cache hits", "results": results}, indent=2) + "\n")

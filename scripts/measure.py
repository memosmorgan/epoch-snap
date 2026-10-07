#!/usr/bin/env python3
"""Run the benchmark matrix and record the host environment; stdlib only."""
import argparse
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import time


def capture(args):
    result = subprocess.run(args, text=True, capture_output=True, check=False)
    return {"command": args, "returncode": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr}


def environment(binary):
    commands = [
        ["uname", "-a"], ["lscpu"], ["lscpu", "-e=CPU,CORE,SOCKET"],
        ["free", "-b"], ["rustc", "-Vv"], ["cargo", "-V"],
        ["git", "rev-parse", "HEAD"], ["git", "status", "--short"],
    ]
    return {
        "recorded_unix_seconds": time.time(),
        "affinity": sorted(os.sched_getaffinity(0)),
        "scheduler": os.sched_getscheduler(0),
        "nice": os.getpriority(os.PRIO_PROCESS, 0),
        "loadavg": os.getloadavg(),
        "base_page_bytes": os.sysconf("SC_PAGE_SIZE"),
        "thp_enabled": Path("/sys/kernel/mm/transparent_hugepage/enabled").read_text(),
        "cargo_lock_sha256": hashlib.sha256(Path("Cargo.lock").read_bytes()).hexdigest(),
        "bench_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "build": "cargo build --locked --release --examples; default Cargo release profile; no RUSTFLAGS override",
        "rustflags": os.environ.get("RUSTFLAGS"),
        "commands": [capture(command) for command in commands],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--contention-cpus", required=True,
                        help="two allowed logical CPUs, applied equally to every mode")
    args = parser.parse_args()
    cpus = [int(cpu) for cpu in args.contention_cpus.split(",")]
    if len(cpus) != 2 or len(set(cpus)) != 2 or not set(cpus) <= os.sched_getaffinity(0):
        parser.error("choose two distinct available logical CPUs")
    root = Path(__file__).resolve().parent.parent
    os.chdir(root)
    destination = args.output_dir.resolve()
    destination.mkdir(parents=True, exist_ok=False)
    subprocess.run(["cargo", "build", "--locked", "--release", "--examples"], check=True)
    binary = root / "target/release/examples/bench"
    (destination / "environment-before.json").write_text(json.dumps(environment(binary), indent=2) + "\n")
    protocol = {
        "seed": 1, "shuffle_seed": 20261004, "trials_per_mode_case": 10,
        "idle_sizes_mib": [16, 64, 256],
        "idle_workloads": ["read-heavy", "sparse", "clustered", "sequential"],
        "idle_placement": "inherited affinity; no intentional competing load; desktop services remain",
        "contention_sizes_mib": [64], "contention_workloads": ["sparse", "sequential"],
        "pinned_idle_control": "same two-CPU affinity as contention, without yes processes",
        "contention_affinity": cpus,
        "contention_load": "one continuously writing 'yes' process per selected CPU, stdout /dev/null",
        "optional_1gib": "not run; optional larger-memory experiment",
        "csv_files": [],
    }
    loads = []
    with (destination / "commands.jsonl").open("w") as command_log, (destination / "run.log").open("w") as log:
        def case(phase, mib, workload, prefix=()):
            name = f"{phase}-{mib}-{workload}.csv"
            command = [*prefix, str(binary), "--arena-mib", str(mib), "--workload", workload,
                       "--seed", "1", "--shuffle-seed", "20261004", "--trials", "10",
                       "--output", str(destination / name)]
            command_log.write(json.dumps(command) + "\n")
            command_log.flush()
            print(f"running {name}", flush=True)
            if phase == "contention" and any(load.poll() is not None for load in loads):
                raise RuntimeError("contention process exited; refusing a false loaded result")
            # A private session contains bench and its trial descendants. On
            # timeout, terminate the entire tree before reaping its leader.
            process = subprocess.Popen(command, stdout=log, stderr=log, start_new_session=True)
            try:
                code = process.wait(timeout=1800)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
            if code:
                raise subprocess.CalledProcessError(code, command)
            if phase == "contention" and any(load.poll() is not None for load in loads):
                raise RuntimeError("contention process exited during the case")
            protocol["csv_files"].append(name)
        try:
            for mib in protocol["idle_sizes_mib"]:
                for workload in protocol["idle_workloads"]:
                    case("idle", mib, workload)
            for workload in protocol["contention_workloads"]:
                case("pinned-idle", 64, workload, ["taskset", "-c", args.contention_cpus])
            for cpu in cpus:
                loads.append(subprocess.Popen(["taskset", "-c", str(cpu), "yes"],
                                              stdout=subprocess.DEVNULL, stderr=log))
            for workload in protocol["contention_workloads"]:
                case("contention", 64, workload, ["taskset", "-c", args.contention_cpus])
        finally:
            for load in loads:
                if load.poll() is None:
                    load.terminate()
            for load in loads:
                load.wait(timeout=10)
            (destination / "protocol.json").write_text(json.dumps(protocol, indent=2) + "\n")
            (destination / "environment-after.json").write_text(json.dumps(environment(binary), indent=2) + "\n")
    print(f"archived {len(protocol['csv_files'])} cases in {destination}")


if __name__ == "__main__":
    main()

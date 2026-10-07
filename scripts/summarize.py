#!/usr/bin/env python3
"""Validate archived data and regenerate per-case/mode trial distributions."""
import csv
import hashlib
from pathlib import Path
import statistics
import sys


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    directory = Path(sys.argv[1])
    files = sorted(directory.glob("idle-*.csv")) + sorted(directory.glob("pinned-idle-*.csv")) + sorted(directory.glob("contention-*.csv"))
    require(len(files) == 16, "expected 12 matrix cases, 2 placement controls and 2 contention cases")
    summaries = []
    total = 0
    for path in files:
        with path.open() as source:
            rows = list(csv.DictReader(source))
        require(len(rows) == 30, f"{path}: expected 30 successful trials")
        identity = path.stem.removeprefix("pinned-idle-").removeprefix("contention-").removeprefix("idle-")
        mib, workload = identity.split("-", 1)
        require(all(int(row["arena_bytes"]) == int(mib) * 1024 * 1024 and row["workload"] == workload for row in rows), f"{path}: filename/case mismatch")
        require({int(row["run_index"]) for row in rows} == set(range(30)), f"{path}: run order incomplete")
        require(len({row["state_hash"] for row in rows}) == 1, f"{path}: cross-mode/trial state mismatch")
        require(len({row["image_hash"] for row in rows if row["mode"] != "none"}) == 1, f"{path}: boundary image mismatch")
        for mode in ["none", "stopped", "wp"]:
            group = [row for row in rows if row["mode"] == mode]
            require(len(group) == 10, f"{path}: missing {mode} trials")
            require({int(row["trial"]) for row in group} == set(range(10)), f"{path}: duplicate trial")
            for row in group:
                require(None not in row and None not in row.values(), f"{path}: malformed schema")
                require(row["seed"] == "1" and row["shuffle_seed"] == "20261004", f"{path}: seed mismatch")
                pages = int(row["arena_bytes"]) // int(row["page_bytes"])
                require(int(row["operations"]) == 3 * pages and int(row["checkpoint_at"]) == pages, f"{path}: schedule mismatch")
                require(row["poll_batch_ops"] == "64", f"{path}: poll mismatch")
                require(int(row["child_wall_ns"]) >= int(row["trial_total_ns"]) >= int(row["application_ns"]), f"{path}: total timing mismatch")
                require(int(row["trial_peak_rss_kib"]) >= int(row["peak_rss_kib"]), f"{path}: peak memory mismatch")
                if mode == "none":
                    for field in ["checkpoint_call_ns", "capture_complete_ns", "t_to_ready_ns", "worker_cpu_ns", "restore_ns", "copied_bytes", "first_write_pending_pages"]:
                        require(row[field] == "NA", f"{path}: no-checkpoint fabricated {field}")
                else:
                    require(int(row["fault_pages"]) + int(row["scan_pages"]) == pages, f"{path}: page accounting")
                    require(int(row["copied_bytes"]) == int(row["arena_bytes"]), f"{path}: copy accounting")
                    require(int(row["capture_complete_ns"]) - int(row["boundary_offset_ns"]) == int(row["t_to_ready_ns"]), f"{path}: timing offsets")
                    if mode == "stopped":
                        require(row["worker_cpu_ns"] == "NA" and row["worker_elapsed_ns"] == "NA" and row["first_write_pending_pages"] == "0", f"{path}: stopped worker/pending fields")
                    else:
                        require(int(row["worker_cpu_ns"]) > 0 and int(row["worker_elapsed_ns"]) <= int(row["t_to_ready_ns"]), f"{path}: missing WP worker timing")
                        require(int(row["fault_pages"]) <= int(row["first_write_pending_pages"]), f"{path}: first-write accounting")
            summary = {"case": path.stem, "mode": mode, "trials": len(group)}
            for field in rows[0]:
                if field in {"mode", "workload", "state_hash", "image_hash"}:
                    continue
                values = [float(row[field]) for row in group if row[field] != "NA"]
                if not values:
                    for stat in ["min", "median", "p95", "max"]:
                        summary[f"{field}_{stat}"] = "NA"
                    continue
                values.sort()
                summary[f"{field}_min"] = values[0]
                summary[f"{field}_median"] = statistics.median(values)
                summary[f"{field}_p95"] = values[-1]  # nearest rank ceil(.95 * 10)
                summary[f"{field}_max"] = values[-1]
            summaries.append(summary)
        total += len(rows)
    with (directory / "summary.csv").open("w") as destination:
        writer = csv.DictWriter(destination, fieldnames=list(summaries[0]))
        writer.writeheader()
        writer.writerows(summaries)
    paths = sorted(path for path in directory.iterdir() if path.is_file() and path.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in paths))
    print(f"validated {total} successful trials in {len(files)} cases; generated summary.csv and SHA256SUMS")


if __name__ == "__main__":
    main()

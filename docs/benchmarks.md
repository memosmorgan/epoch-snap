# Measurement and demo protocol

The harness has 480 validated host trials. [The contract](design.md) defines
correctness. Performance never substitutes for exact-image validation.

## Comparisons

Run identical initialized atomic-slot workloads in three configurations:
no checkpoint, stopped checkpoint, and synchronous WP checkpoint. Stopped and
WP capture use the same atomic-load image-copy routine and complete-image
semantics. Never label a fallback result as WP. Optional raw `memcpy` bandwidth
calibration is a separate result, not the stopped atomic baseline.

Use one mutation owner, fixed seeds, a fixed operation count, and a checkpoint
at a specified operation boundary. Each trial starts from the same initial state.
Keep validation hashes/image comparison outside timed intervals, then verify the
result. Do not create the stopped oracle inside a WP performance trial.

Time application throughput from the first to last scheduled operation, including
the checkpoint call at its fixed boundary. Exclude constructor initialization and
the separate restore/replay phase. Publish that constructor cost alongside the
trial. Do not subtract individual fault stalls from application throughput.

Observe status after each fixed batch of 64 operations during the trial, using
the same polling schedule for both capture modes. At the trial's end, wait for
completion. Record the polling schedule and compare WP worker completion time
with owner-observed readiness; delayed observation/join belongs in the latter.
Keep checkpoint-call pause and ordinary operation percentiles separate.

## Initial matrix

- Rounded arena sizes: 16, 64, and 256 MiB. Add 1 GiB only when available memory
  comfortably accommodates engine, process, and measurement overhead.
- Workloads: read-heavy/no writes during capture; sparse uniform writes; clustered
  hot-page writes; sequential full-arena writes.
- Ten trials per configuration/case, with a recorded shuffle seed for run order.
- First run otherwise idle; repeat selected cases with documented CPU contention.
  Record thread placement; do not silently pin only the WP case.

Use fixed operation schedules, not the assumption that a requested dirty fraction
was realized. Report distinct pages actually first-written during the API's
owner-observed pending window (see the sampling limitation below),
along with operation counts and checkpoint frequency. Start with one checkpoint
per trial; add repeated checkpoints only by explicitly discarding between them.

## Required fields and timing boundaries

Each raw CSV row records mode, workload/seed, trial, rounded arena bytes, operation
count, checkpoint boundary, distinct first-written pages, and:

- `arena_init_ns`: allocation/population of arena, reusable image, and bookkeeping.
- `checkpoint_call_ns`: full owner pause, including state reset, worker/context
  setup/registration, arming, and acknowledgement; stopped capture includes copying.
- `capture_complete_ns`: from checkpoint-call entry until ready after worker join
  and final context close. Also report time from T to ready and worker completion
  separately for WP, using the ready status's `CaptureMetrics`.
- `restore_ns`: full exclusive restore call; not a claim about process recovery.
- Application operations/second and operation p50/p95/p99/max latency, with the
  sampling method and instrumentation overhead documented.
- Fault-serviced pages, scanner-serviced pages, total image bytes copied, worker
  CPU time from `CaptureMetrics`, process CPU/context switches, and peak process
  memory. Worker-only fields are not applicable in stopped/no-checkpoint cases.
- First-write operation latency distribution during pending capture. If instrumenting
  worker fault service separately, do not present it as the entire writer stall.

Absent checkpoint/restore fields in the no-checkpoint case are marked not applicable,
not measured zero-cost operations. Use monotonic timing. Treat the arena's state
hash as correctness output, not as the snapshot completion timestamp.

Save hardware/CPU/RAM, kernel, base page size, THP advice, compiler/version/build
settings, dependency lockfile revision, scheduler/thread placement, and code
revision beside each published dataset. Publish individual trials and distributions
rather than only a mean or a selected best run. Use the same memory accounting
method across configurations and describe its sampling limitations.

## Interpretation

The hypothesis is reduced global checkpoint pause at the expense of background
work and individual write stalls. V1 prepares one image buffer at construction
and reuses it, so report initialization and steady capture costs distinctly.
Worker creation, registration, arming, and reclaim may still erase the benefit;
publish that outcome if observed. Neither shorter total capture time nor bounded
tail latency is an acceptance assumption.

Fault-priority scanning can be delayed by write traffic; completion time and
owner latency both matter. Include full-write and CPU-contention cases, not just
read-heavy results. Explain kernel capability failures instead of silently
dropping them from compatibility claims.

## Replay demo

Use a deterministic simulation of fixed records encoded in slots. Store logical
step, RNG state, and all simulation state in the arena. Keep rendering, fault
service, and control metadata outside it. Display the checkpoint's step, capture
progress, and live advancing step. After readiness, advance further, restore,
and replay the same input sequence. Compare every final slot and display the
matching state hash. The demo must fail on mismatch and clearly describe its
same-process, in-memory rewind contract.

## Implemented harness

```sh
epochsnap_bench_dir=$(mktemp -d)
cargo run --locked --release --example bench -- --arena-mib 64 --workload sparse --seed 1 --trials 10 --output "$epochsnap_bench_dir/sparse.csv"
cargo run --locked --release --example rewind
```

The default benchmark runs all three modes (`none`, `stopped`, `wp`) in shuffled
order, with workload seed 1 and shuffle seed 1. Each trial is a fresh child
process, with a 120-second external kill/reap deadline. An error fails the run;
previous completed rows remain in the CSV, and no replacement row is emitted.
Output files use exclusive creation to avoid overwriting evidence. Explicit
`--modes none,stopped` supports ordinary CI on a host without UFFD; it is never
reported as WP coverage. Seeds, trial number and actual run index accompany rows.

At P base pages, defaults are 3P operations and a checkpoint before operation P
(zero based), leaving two passes after the boundary. All modes allocate the same
`Arena`, including its prepared image, and initialize every slot to a nonuniform
seeded value. There is one checkpoint per capture trial. `--operations` and
`--checkpoint-at` can change the fixed schedule explicitly. The workloads are:

| CLI workload | One logical operation |
| --- | --- |
| `read-heavy` | Eight atomic slot loads from a seeded uniform page, no writes |
| `sparse` | One atomic slot overwrite on a seeded uniform page and slot |
| `clustered` | One overwrite inside the first ceil(P/100) hot pages |
| `sequential` | Overwrite every slot on page `operation % P`; covers the entire arena per pass |

Values are deterministic functions of seed, operation and slot. The timed path
uses only the existing owner API. After the timed application interval and final
wait, a separate ordinary-memory model reconstructs the boundary and final
state, compares every snapshot/live slot, and computes hashes. No stopped oracle
or validation allocation is made inside a WP performance interval. Restore is
timed separately and every restored slot is compared with the retained image.
Cross-mode final hashes are checked by the parent in addition to each child's
full comparisons. A validation failure produces no successful result row.

The first-write bitmap resets at the checkpoint boundary and counts distinct
pages actually selected by subsequent write operations. `first_write_pending_pages`
and its latency percentiles include only the first such write while the owner
has not yet observed readiness at the scheduled 64-operation poll or final wait.
This is an **owner-observed pending window**, which can include time after worker
closure and before the next join. It is not a count of kernel-faulted pages or
proof that each counted write blocked. Fault and scan page counts come separately
from `CaptureMetrics`. Polling every first write would change the agreed polling
schedule; the harness does not do so. Empty first-write samples have `NA`
percentiles; stopped has zero pending first writes, and no-checkpoint has `NA`.

Every operation is sampled with an `Instant` pair (100% sampling), nearest-rank
p50/p95/p99/max. The operation interval encloses the complete page/slot operation,
including checked API calls and any writer stall; schedule selection, bitmap
maintenance and sample recording are outside that individual interval but inside
application time. Application time also includes checkpoint calls, scheduled
status polls, loop overhead and recording. It ends after the last operation's
scheduled poll and excludes the separate final wait. Thus throughput is
instrumented application throughput, not a bare store bandwidth measurement.
`clock_pair_p50_ns` records 10,000 empty clock pairs before application timing;
it estimates clock overhead only, not all instrumentation costs. No overhead is
subtracted. Instrumentation is identical across modes except the required capture
calls and polls; no-checkpoint cannot poll a nonexistent epoch.

`checkpoint_call_ns` is the external complete call duration. Completion and T
offsets use the engine's monotonic call-entry metrics: `capture_complete_ns` is
`ready_offset`, `boundary_offset_ns` is `boundary_offset`, `t_to_ready_ns` is their
difference. External call timing includes the tiny wrapper overhead absent from
engine offsets. WP `worker_elapsed_ns` runs from T through descriptor close;
`worker_cpu_ns` includes setup/copy/cleanup. Neither hashes nor the end of the
application interval is used as a completion timestamp. The final explicit wait
is separately charged as `wait_ns`, even if an earlier poll already joined.

CSV fields additionally expose `workload_init_ns`, `observer_setup_ns`, full
`application_ns`, validation/model allocation (`validation_ns`), restore checking
(`restore_validation_ns`), discard and mapping/buffer destruction (`arena_drop_ns`).
`trial_total_ns` includes all child work through latency sorting and final resource
measurement; `child_wall_ns` includes process launch, exit and the parent watchdog's
10-ms observation granularity. The latter is not application throughput. CSV
formatting and parent writing are outside child trial timing.

Process CPU and voluntary/involuntary context switches use `getrusage(RUSAGE_SELF)`:
`process_*` fields cover the complete child trial, `application_process_cpu_ns` and
`application_*switches` cover the instrumented operation interval and include
both owner and worker. CPU resolution is one microsecond. Peak memory is Linux
`ru_maxrss` in KiB for each fresh process. `peak_rss_kib` is read after wait and
before the validation model; `trial_peak_rss_kib` also includes that extra one-arena
model. Both include executable/runtime, image, page state and instrumentation;
neither is a sampled instantaneous arena allocation. Fresh processes prevent an
earlier mode's high-water mark from contaminating another. Validation memory is
additional to the engine's approximate 2x arena budget.

The demo defaults to real WP and 16 MiB, prints live progress, waits for final
close/join, advances to step 1408, restores step 128 and replays the same inputs.
Logical step, RNG, accumulator and four-slot records all live in the arena;
every final slot, including unused/padding slots, must match before success.
`--mode stopped --arena-mib 1` supplies the ordinary CI replay case. WP failure
exits nonzero without fallback.

## Measured results, 2026-10-04

The [archived dataset](results/m4/2026-10-04/protocol.json) contains **480 successful
trial rows**: 360 in the initial matrix, 60 placement controls and 60 with injected
CPU contention. Every child's full boundary/final/restore comparisons passed;
final hashes agree across modes/trials and capture image hashes agree per case.
The [summary CSV](results/m4/2026-10-04/summary.csv) retains min/median/p95/max
across individual trials for every numeric field. With ten trials, nearest-rank
trial p95 is the maximum; the data does not estimate population confidence.
Tables below use trial medians. Operation p99 columns are the median of each
trial's p99, not a pooled percentile across all operations.

Measured benchmark revision: `4dc0ca76f42b9bfa0c0f2d3e95b7406c69ca09da`.
The later process-tree watchdog fix changes error containment only; the measured
benchmark source/binary and engine are unchanged. Raw CSV, exact commands,
stdout/stderr logs, [before](results/m4/2026-10-04/environment-before.json)/
[after](results/m4/2026-10-04/environment-after.json) metadata and
[checksums](results/m4/2026-10-04/SHA256SUMS) are archived together. Environment:

- Ryzen 5 5600, 6 physical cores / 12 logical CPUs, about 15.5 GiB RAM.
- Fedora Linux `7.2.8-100.fc43.x86_64`; 4096-byte pages; arena `MADV_NOHUGEPAGE`.
  System THP mode remained `madvise`.
- Rust `1.96.1 (31fca3adb)`, LLVM 22.1.2, stable edition 2024; default Cargo
  release profile, committed lockfile, no `RUSTFLAGS` override.
- Ordinary scheduler (`SCHED_OTHER`), nice 0, AMD pstate EPP `powersave` governor;
  CPU boost/frequency and desktop services remained uncontrolled.
- Initial matrix inherited affinity CPUs 0–11, with no intentional competing
  workload. The desktop was not isolated; initial load averages were 2.45/1.88/1.92.
  "Idle" filenames mean no injected load, not a demonstrably idle machine.
- Selected 64 MiB controls and contention use CPUs 2,8, the SMT siblings of
  physical core 2, for **all modes and their workers**. Contention adds one `yes`
  process per selected CPU, continuously writing to `/dev/null`. Placement
  controls precede contention with that same affinity and no `yes` processes.
- The optional 1 GiB case was not run. This is one-host evidence, not validation
  of every intended Linux 6.6+ environment.

### Reproduce the matrix

Run from the repository root on a UFFD-capable host, after `doctor` and the
[explicit kernel tests](testing.md#host-and-commands) pass. The runner needs
Python 3 (standard library only), Rust/Cargo, Git, `lscpu`/`taskset` (util-linux),
`free` (procps) and `yes` (coreutils). Leave enough memory for the 256 MiB arena,
its image, latency samples and validation model; measured full-trial peak RSS
reached about 774 MiB. Avoid unrelated load for the initial matrix.

Inspect topology and the shell's allowed CPUs. Replace `2,8` with two allowed
logical CPUs; use SMT siblings to match the archived contention placement.
The output directory must not exist, so use a new subdirectory of a temporary
directory:

```sh
lscpu -e=CPU,CORE,SOCKET
taskset -pc $$
epochsnap_matrix_root=$(mktemp -d)
python3 scripts/measure.py --output-dir "$epochsnap_matrix_root/matrix" --contention-cpus 2,8
python3 scripts/summarize.py "$epochsnap_matrix_root/matrix"
```

The runner builds locked release examples, records environment and exact commands,
then runs all cases sequentially. Seed is 1; shuffle seed is 20261004. Each CSV
contains all ten trials per mode and run indices. The runner never changes
sysctls, THP policy, governor or system-wide scheduler policy. On an outer timeout
it kills the private benchmark process group before reaping; competing processes
are stopped and reaped in `finally`. The summary script requires the complete
16-case matrix; a single smoke CSV is not sufficient. A failed matrix run is
incomplete evidence and must not be summarized as a successful result.

To validate the published archive without writing into it:

```sh
(cd docs/results/m4/2026-10-04 && sha256sum --check SHA256SUMS)
epochsnap_archive_copy=$(mktemp -d)
cp -a docs/results/m4/2026-10-04 "$epochsnap_archive_copy/data"
python3 scripts/summarize.py "$epochsnap_archive_copy/data"
cmp docs/results/m4/2026-10-04/summary.csv "$epochsnap_archive_copy/data/summary.csv"
```

For public release, the original personal checkout prefix in `commands.jsonl`
and `correctness.log` was replaced with repository-relative paths. Command
arguments, recorded output apart from that prefix, environment metadata and all
numerical CSV results are preserved. Archive checksums were regenerated after
this text-only normalization. Original unnormalized text is retained in private
development history; this public repository begins with the release snapshot.

### Checkpoint pause and readiness

Checkpoint pause and owner-observed readiness, milliseconds:

| MiB | Workload | Stopped pause | WP pause | Stopped ready | WP ready |
| --- | --- | ---: | ---: | ---: | ---: |
| 16 | read-heavy | 1.787 | 0.711 | 1.787 | 13.946 |
| 16 | sparse | 1.734 | 0.723 | 1.734 | 19.148 |
| 16 | clustered | 1.660 | 0.688 | 1.659 | 13.834 |
| 16 | sequential | 1.641 | 0.687 | 1.640 | 17.022 |
| 64 | read-heavy | 8.092 | 2.288 | 8.092 | 56.301 |
| 64 | sparse | 8.633 | 2.575 | 8.632 | 75.398 |
| 64 | clustered | 8.808 | 2.286 | 8.807 | 56.523 |
| 64 | sequential | 8.908 | 2.416 | 8.908 | 72.208 |
| 256 | read-heavy | 34.290 | 8.233 | 34.289 | 219.472 |
| 256 | sparse | 32.276 | 8.243 | 32.275 | 278.632 |
| 256 | clustered | 34.885 | 8.380 | 34.884 | 212.223 |
| 256 | sequential | 34.515 | 9.093 | 34.515 | 275.939 |

Instrumented application throughput, million operations/second, and ordinary
operation p99, microseconds. A sequential operation writes a whole base page;
the other write workloads overwrite one slot. Compare modes within a workload.

| MiB | Workload | None Mops/s | Stopped Mops/s | WP Mops/s | Stopped p99 us | WP p99 us |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 16 | read-heavy | 8.241 | 3.585 | 4.100 | 0.310 | 1.027 |
| 16 | sparse | 10.729 | 4.193 | 0.662 | 0.201 | 15.094 |
| 16 | clustered | 12.753 | 4.553 | 5.821 | 0.070 | 0.602 |
| 16 | sequential | 0.598 | 0.556 | 0.409 | 3.571 | 8.726 |
| 64 | read-heavy | 6.784 | 2.983 | 3.691 | 0.456 | 1.112 |
| 64 | sparse | 8.749 | 3.576 | 0.661 | 0.260 | 14.348 |
| 64 | clustered | 11.987 | 3.706 | 5.729 | 0.080 | 0.612 |
| 64 | sequential | 0.596 | 0.536 | 0.399 | 3.732 | 11.021 |
| 256 | read-heavy | 5.942 | 2.860 | 3.452 | 0.420 | 1.077 |
| 256 | sparse | 9.898 | 3.698 | 0.722 | 0.311 | 13.541 |
| 256 | clustered | 12.351 | 3.785 | 6.288 | 0.120 | 0.822 |
| 256 | sequential | 0.597 | 0.554 | 0.407 | 2.886 | 10.695 |

WP shortened median checkpoint pause by 2.4–4.2x in every initial-matrix case,
but owner-ready completion took 6.1–11.0x longer. Sparse WP throughput fell to
15.8–19.5% of stopped, and sequential WP fell to 73.5–74.4%. Read-heavy and
clustered WP throughput improved over stopped, but every WP case remained below
its no-checkpoint reference and had worse operation p99 than stopped. These
results support a narrower global-pause claim, not faster total capture or
better writer tails. In read-heavy/clustered trials the application can finish
before the worker; the separate wait/readiness cost must remain visible.

At 64 MiB, sparse WP's median first-write-pending p99 was 18.085 us and its median
trial maximum 551.677 us; ordinary-operation p99 was 14.348 us. Its median
owner-observed distinct first-written page count was 14,217 while fault-origin
pages were 5,128.5 across ten trials (individual counts are integers). Clustered
WP first-wrote all 164 hot pages, with median 51.5 fault-origin pages. Sequential
WP first-wrote 16,384 pages while only median 13.5 were fault-origin pages.
This shows why first-write samples must not be equated with individual UFFD
fault-service time. Read-heavy had zero first-write samples and fault pages.

Selected same-placement controls and contention:

| 64 MiB case | Stopped pause ms | WP pause ms | None Mops/s | Stopped Mops/s | WP Mops/s | Stopped p99 us | WP p99 us |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pinned-idle sparse | 8.197 | 2.223 | 10.330 | 3.650 | 0.572 | 0.275 | 14.357 |
| pinned-idle sequential | 8.126 | 2.282 | 0.626 | 0.559 | 0.369 | 2.760 | 5.486 |
| contention sparse | 17.731 | 6.084 | 3.110 | 1.468 | 0.264 | 0.141 | 15.705 |
| contention sequential | 17.688 | 5.826 | 0.167 | 0.157 | 0.145 | 3.011 | 5.446 |

On the shared physical core, injected contention reduced median WP throughput
from 0.572 to 0.264 Mops/s for sparse and 0.369 to 0.145 Mops/s for sequential.
Readiness rose from 87.899 to 191.349 ms and 80.151 to 146.734 ms respectively.
Compare contention with the pinned control, not the unrestricted matrix, to
separate placement from injected load. Frequency variation, desktop activity,
SMT effects and scheduler delays limit interpretation; no bounded tail is claimed.

Constructor and memory costs are substantial and not included in application
throughput. For sparse WP at 16/64/256 MiB, median arena initialization was
18.798/71.567/274.018 ms; peak before validation was 34.621/131.113/518.057 MiB.
Including the external validation model raised full-trial peaks to
50.621/194.990/774.059 MiB. Raw data records workload initialization, observer
setup, total CPU, context switches, restore, validation, discard and destruction
per trial. Clock-pair median was 30 ns in every group, a material fraction of
fast unstalled slot operations; no instrumentation correction is applied.

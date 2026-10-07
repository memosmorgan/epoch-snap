# Independent benchmark and correctness review

Read-only review completed on 2026-10-04 for measured harness `4dc0ca7` and
watchdog/correctness revision `e8bd91c`.

Decision at the reviewed revisions: **accepted; no unresolved material findings.**

The reviewer examined the contract, examples, measurement/summary scripts,
ordinary/kernel test evidence, existing engine unsafe/lifecycle exits, CI
disclosure and public documentation.

Two findings were resolved before acceptance:

1. Pending first-write measurements needed explicit owner-observed-window
   terminology. The fixed 64-operation observation/join schedule may include
   writes after worker closure; these counts/latencies are separate from actual
   fault-origin pages and are not fault-service or bounded-stall claims.
2. Parent-only outer watchdog kills could leave a benchmark trial alive and
   holding output pipes. Tests now create private process groups and the matrix
   runner creates private sessions, killing the whole group before reaping on
   deadline. A real descendant regression rejected parent-only termination and
   passed after correction.

Independent reruns passed:

- Ordinary example integration suite: 5 passed, 1 WP case explicitly ignored.
- Explicit WP benchmark/replay smoke in debug and release: passed.
- Release descendant-termination regression: passed.

Independent audit verified the 16 cases / 480 raw rows, ten trials per mode/case,
all case identities and schedules, boundary/final state hashes, 48 derived
summary groups, 28 published table rows and constructor/RSS/first-write values.
All 25 archive checksum entries existing at review time matched. This review
record was added to the regenerated checksum manifest afterward. Public-release
path normalization in `commands.jsonl` and `correctness.log` subsequently changed
those two checksums; numerical trial data and the summary remain unchanged.

Recomputed initial-matrix ranges:

- Median stopped/WP pause: 2.3864–4.1649x.
- Median WP/stopped owner-ready duration: 6.0836–11.0445x.
- Sparse WP/stopped throughput: 15.7936–19.5250%.
- Sequential WP/stopped throughput: 73.4646–74.3870%.

Measured versus corrected engine/examples/lockfile contents are identical, and
the current release executable matches the archived SHA-256. The correctness
log supports all twelve recorded commands with zero exits, including both
39-case actual-kernel suites. Local documentation links and script syntax were
checked. No measurement rerun was required for test/watchdog/status corrections.

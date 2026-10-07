# Testing and safety evidence

Development revision identifiers below refer to private development history.
The public repository begins with the reviewed release snapshot; numerical
evidence and archived validation output are preserved.

Validation recorded on 2026-10-04 covers stopped capture, exact concurrent WP
images, failure cleanup, epoch reuse and the benchmark/replay examples. These
results apply to one host; Linux 6.6 compatibility and WP-capable CI remain
unverified. The [archived correctness log](results/m4/2026-10-04/correctness.log)
contains the command output for revision `e8bd91c`. The verification matrix below
was also run on the source reviewed on 2026-10-04; every command passed. Numerical
measurements are unchanged. Public-release path normalization and updated archive
checksums are described in [benchmarks](benchmarks.md#reproduce-the-matrix).

## Host and commands

- Fedora Linux `7.2.8-100.fc43.x86_64`, x86-64, 4096-byte base pages.
- AMD Ryzen 5 5600, 6 cores / 12 logical CPUs, about 15.5 GiB RAM.
- Rust `1.96.1 (31fca3adb 2026-06-26)`, LLVM 22.1.2; Cargo 1.96.1.
- Stable Rust, edition 2024, default release profile, no `RUSTFLAGS` override.
- `libc` 0.2.190 is the sole dependency, pinned in `Cargo.lock`.

Run the full verification matrix after changes to the engine or examples:

```sh
cargo fmt --all -- --check
cargo build --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --doc
cargo run --locked --bin epochsnap -- doctor
cargo run --locked --example rewind -- --mode stopped --arena-mib 1
cargo test --locked --test linux_uffd -- --ignored
cargo test --locked --release --test linux_uffd -- --ignored
cargo test --locked --test examples -- --ignored
cargo test --locked --release --test examples -- --ignored
cargo run --locked --release --example rewind
```

Archived command results:

| Command | Actual result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --lib` | 12 passed |
| `cargo test --doc` | 1 example and 5 compile-fail cases passed |
| `cargo test --test baseline` | 13 passed without UFFD |
| `cargo run --bin epochsnap -- doctor` | Synchronous anonymous WP registration available |
| `cargo test --test linux_uffd -- --ignored` | 39 actual kernel cases passed |
| `cargo test --release --test linux_uffd -- --ignored` | 39 actual kernel cases passed |
| `cargo test --all-targets` | Ordinary suites, 5 example integration cases and 1 corruption unit case passed; 40 WP cases ignored |
| `cargo test` | Ordinary suites/doctests passed; 39 kernel cases and 1 WP example case ignored |
| `cargo test --test examples -- --ignored` | Actual WP benchmark and replay smoke passed |
| `cargo test --release --test examples -- --ignored` | Actual WP benchmark and replay smoke passed |

The fresh release replay also matched all 2,097,152 slots, with hash
`21347a96d40238a4`. The summary script revalidated all 480 trials in 16 cases.

`doctor` reported features `0x1ffff`, generic ioctls `0x8000000000000003`
and range ioctls `0x17c`. That probe is separate from protected-write tests.
Kernel cases and the WP example smoke are deliberately ignored in ordinary
runs. Explicit runs must fail with a useful diagnostic when unavailable, without
silently skipping or substituting stopped capture. The integration target also
compiles 12 private-module unit cases; these duplicate ordinary coverage.
Child-local seccomp tests deny UFFD only within their child process.

## Stopped capture and API restrictions

The 13 baseline cases run without UFFD. They check zero/overflowing sizes,
page rounding and initialized padding, valid/invalid slot bounds, full nonuniform
images, repeated same-mapping restore, buffer reuse, retention and busy capture.
Wrong-arena, discarded and previous-generation tokens are rejected before any
restore store. Identity/generation exhaustion and image reservation failure have
unit coverage. Ownership transfer moves a retained epoch between threads.

Replay keeps logical step, RNG, accumulator and records in the slots. It captures
a boundary, runs fixed inputs, restores the complete boundary state and replays
those inputs; every final slot must match. One runnable doctest and five
compile-fail checks enforce private epoch fields, absent `Clone`/`Sync`, and
image-borrow exclusion for restore/discard.

## Concurrent image tests

WP images are compared with an independently saved stopped image at the same
boundary. Controlled schedules force fault-first and scanner-first saves,
repeated writes, delayed real notifications and pending image/restore rejection.
Arming must cover the entire range before T. A page must be fully saved before
unprotect; a delayed fault must never recopy its newer live contents. Readiness
waits for final descriptor closure and worker join. Reused nonzero images are
replaced even when the new arena contents are zero.

## Failure stages and terminal states

The 39 kernel cases cover page preservation, concurrent images and lifecycle
exits. Tests in `tests/support/lifecycle.rs` are part of the `linux_uffd` target.
All test hooks and injection state are `cfg(test)` and absent from the shipped
library. Errors bypass a selected syscall attempt or interrupt a real atomic
copy; subsequent retries, teardown and blocked-write release use the actual
engine and kernel. Malformed-message injections are protocol-validation evidence,
not claims that Linux generated malformed notifications.

| Cases | Evidence |
| --- | --- |
| Preflight panic and partial-arm resource checks | A panic before protection leaves the arena usable and its buffer reusable; a genuinely protected partial range fails without an epoch, poisons owner access and becomes writable after close/join |
| Partial page-copy error | Half a page is read atomically before an injected error; the faulted owner returns poisoning, and no partial image is exposed |
| Page and final-range unprotect errors | Injected permanent ioctl errors invalidate the image; final context closure releases the real fault service, with terminal failure already published |
| Publication error after close | Even a fully copied, closed context cannot publish ready after worker publication fails; subsequent owner operations report poisoning |
| Interrupted read and transient WP ioctl errors | Multiple read `EINTR` outcomes and protect/page-unprotect/final-unprotect `EINTR`/`EAGAIN` outcomes retry through real operations and produce the exact oracle image; nonblocking read `EAGAIN` returns no event |
| Permanent read error | A real queued WP fault survives until failure cleanup; the released owner store reports poisoning |
| Malformed and out-of-range messages | Unexpected event, missing/extra WP flags, below-start/end/max addresses, short read and EOF are rejected during active capture; no ready image is published |
| Out-of-range read diagnostic | The private read path reports the explicit protocol error before page indexing, while a real writer remains blocked until final close |
| Blocked-owner worker panic | Contained unwind publishes failure before closing fault service; owner and lifecycle/image operations report poisoning, with no retained worker/context resources |
| Queued cancellation/drop resources | Intentional cancellation and pending drop release actual queued writes, join workers and restore the original descriptor/task sets |
| Cancellation cleanup error | Cancellation cannot clear poisoning when teardown fails; its queued writer is released, the token is invalidated and the arena remains unusable |
| Cancellation close/join boundary | Discard cannot return while the worker is gated before descriptor close; owner usability follows close/join |
| Owner unwind with pending capture | Arena drop during owner panic joins the worker; a weak mapping reference cannot be upgraded afterwards |
| Owner load failure boundary | Failure is published between the payload load and its post-access health check; the load returns poisoning, complementing blocked-store tests |
| Final close failure and escaped worker panic | Isolated children must terminate with `SIGABRT`; continuing, merely panicking in the owner, or detaching the worker fails the parent assertion |
| 1001 WP epochs | 500 queued-fault cancellations and 501 complete captures isolate contexts, images, page state and tokens across reuse |

Poison checks include valid and invalid slot indices, image/status/wait/restore,
new stopped/WP captures and discard. A failed in-flight store may have physically
written its value when final close releases it; the API must report poisoning.
Pre-access checks reject later stores before mutation. Successful writes before
a later publication failure are permitted; tests require poisoning once that
failure has been observed/joined.

Each kernel case runs in a subprocess with an external 10-second kill/reap
deadline. Fatal-exit children disable core dumps only within the child. Channel
acknowledgements, kernel fault readability and acquire/release control flags
establish schedules; no protocol assertion relies on sleeping. The parent
watchdog alone polls with a sleep.

## Resource and epoch isolation

Resource snapshots compare descriptor numbers/targets and thread IDs under
`/proc/self/fd` and `/proc/self/task` before and after cleanup. The observation
directory's own descriptor is excluded. A bounded yielding observation allows
kernel task entries to disappear after pthread join; it does not promise an
engine timeout. Every WP stress cycle returns to the original resource sets.
Mapping Arc counts return to the owner alone after worker join; the owner-unwind
case additionally verifies actual mapping retirement through a weak reference.

The two-page WP stress fixture performs 1000 alternating cycles, then one final
complete capture after the last queued-fault cancellation. The 500 complete
cycles compare every slot with independently assigned values, restore twice,
and check fault-origin counts. The 500 cancellations each leave a real queued
fault in the retiring context; the fixture writer is joined before the next
owner mutation. Only that writer mutates while the owner performs cancellation;
this is a private adversarial fixture, not an added public multi-mutator API.

Image/page-buffer pointers and mapping addresses stay fixed. First/previous and
wrong-arena tokens are rejected during the new pending epoch, all 1000 discarded
tokens are rejected afterwards, and the final image includes the completed
cancelled write. Zero-filled pages replace old nonzero buffer contents. A separate
1000-cycle stopped test covers full-image replacement, restore and stale tokens
without kernel capabilities. Caller-owned expected images are extra test storage,
outside the engine's live mapping plus single reusable image budget.

## Atomic access and ownership

Release IR in `target/release/deps/linux_uffd-*.ll` and the library IR retains
`load atomic i64 ... monotonic, align 8` in `Mapping::copy_page`; library owner
store/restore retain `store atomic i64 ... monotonic, align 8`. Matching assembly
uses scalar `movq` source loads and separate image stores, without bulk or wider
non-atomic live-payload copying. The test-only partial-copy seam adds an atomic
metadata comparison outside the arena; it is absent from the shipped copy loop.

The unsafe review covers every block and the two private mapping trait impls:
checked page-rounded bounds/alignment and Arc lifetime, uniformly atomic views,
private immutable mapping metadata, C-layout ioctl buffers, packed-message byte
decoding, owned descriptor creation/final consumption, stack poll/time buffers,
and thread-local errno injection. The child-only resource-limit call uses an
initialized `rlimit` outside arena storage. New test metadata is ordinary private
storage or atomics; no hook exports live payload pointers through the library.

The worker owns page state, image and its sole context. Armed context drop stores
terminal failure with release ordering before final close. Owner operations use
acquire checks before/after payload access. Failed or cancelled images expose no
ready metrics. Storage returns only through join; successful cancellation reuses
it only after full teardown. Mapping ownership outlasts worker/context access,
including owner unwind. Unconfirmed final close or an escaped worker panic aborts
rather than continuing with unknown ownership. The worker requires no lock held
by an owner that can fault.

The protection ioctl completes before T and the arming acknowledgement.
No scanner or fault copy runs before acknowledgement. Linux synchronous WP
prevents each unsaved page's owner writes until that entire page has been read;
a compiler fence prevents source/image operations from crossing its release.
Uniform atomic access separately supplies Rust race freedom, without treating
WP as a Rust happens-before primitive. The same worker owns saved-state checks,
so repeated/delayed notifications cannot select a second copy.

`Arena` retains an Arc to the mapping and remains non-cloneable/non-`Sync`.
Exclusive owner methods enforce operation boundaries. The worker and registered
context retain private Arcs; mapping unmap therefore cannot precede their last
access. Its prepared image/page state is transferred only after successful
thread creation and recovered from a joined worker, including contained panic.
No second full image or backend abstraction was added.

Control atomics, channels, worker stack, image and fault buffers reside outside
the arena. The worker never needs a lock held by an owner in a protected store.
`Registered` shares the worker's failure atomic; its armed Drop stores terminal
failure with release ordering before final close. Owner loads/stores acquire
that failure both before and after payload access. Error/panic thus cannot
release a blocked store and report it as successful owner progress.
Preflight failures retain usability; failures once protection may have started
poison. Successful cancellation confirms unprotect/close/join before reuse.
Drop always joins before mapping retirement. An escaped worker panic or failed
final close/unmap aborts rather than leaving unestablished cleanup running.

Release inspection was recorded for both the library and kernel-test target.
Recheck when changing the payload path or toolchain:

```sh
cargo rustc --release --test linux_uffd -- --emit=llvm-ir,asm
cargo rustc --release --lib -- --emit=llvm-ir,asm
```

Inspect `Mapping::copy_page`, `Arena::store_word` and `Arena::restore` in the
matching files under `target/release/deps/`. Live accesses must remain aligned
64-bit atomic loads/stores; private-image copies do not authorize bulk copying
of live payload. Compiler output and Miri do not establish real kernel WP
correctness.

## Examples and measurement archive

Ordinary example tests check invalid arguments before output creation, CLI/CSV
fields, absent-field semantics, stopped replay, denied WP and process-tree
deadlines. The watchdog regression acknowledges a real descendant's creation,
then verifies that timeout cleanup kills the entire process group. A replay unit
test corrupts the last slot and requires full-state rejection. The explicit WP
smoke checks actual worker metrics, page/byte accounting and full replay.

The archived matrix contains 480 successful trials in 16 cases. The summary
script verifies case identities, mode/trial coverage, seeds, schedules, hashes,
page/byte accounting, timing relationships and absent fields. See
[benchmarks](benchmarks.md) for the protocol, distributions and limitations.
The [demo log](results/m4/2026-10-04/demo.log) records 2,097,152 equal slots after
restore to step 128 and replay to step 1408, with hash `21347a96d40238a4`.

These are one-host results. Injected errors validate controlled retry/failure
paths; they do not claim that the kernel naturally produced those errors.
Subprocess deadlines contain tests and do not promise production recovery from
kernel/scheduler stalls. OOM survival, durable recovery and whole-process rewind
are outside the contract. The CI workflow covers ordinary tests only; a remote
passing run remains unverified.

## Public-release validation, 2026-10-07

The release-preparation tree was copied into a fresh local clone with no `target/`
directory or local development metadata. The matrix above passed on Fedora Linux
`7.2.9-100.fc43.x86_64`, Ryzen 5 5600, 4096-byte pages and Rust/Cargo 1.96.1.
Ordinary suites passed with the 40 WP cases explicitly ignored; all six doctests
passed. Separate runs passed all 39 actual UFFD cases in both debug and release,
plus the WP example smoke in both profiles. This is local kernel validation,
not evidence of remote UFFD CI or Linux 6.6 compatibility.

Stopped replay matched all 131,072 slots. Default release WP replay matched all
2,097,152 slots with hash `21347a96d40238a4`. The README's 64 MiB sparse benchmark
and stopped-only benchmark commands passed. The published 480-row archive was
revalidated, all 26 checksums passed, and regeneration produced a byte-identical
summary. The README's pause table was independently recomputed from raw trials.
The full matrix runner also completed all 16 cases and 480 fresh trials with
CPUs 2,8 for placement/contention controls; summary validation passed. This rerun
verified the workflow without replacing the published 2026-10-04 measurements.

The existing [Linux workflow](../.github/workflows/linux.yml) runs formatting,
Clippy with `-D warnings`, `cargo test --locked --all-targets` and
`cargo test --locked --doc`. Those checks passed locally. Initial release-preparation
validation did not have authenticated access to inspect remote Actions status.

The initial release-preparation audit used Gitleaks 8.30.1 (checksum-verified
release binary, default rules and redacted reports) and found no secrets in the
tracked tree, all 15 then-reachable commits, or all 119 historical file blobs.
A separate file, URL and path review found no
credential-bearing/private service URLs or accidentally tracked private files.
Personal checkout paths in the current archive were normalized; the original
paths and author contact metadata remain in private development history. The
public snapshot does not include those commits.
Local development metadata is ignored. Documentation links and English source
comments were checked; engine changes are comment translations only.

Final pre-push validation also passed Clippy with `--all-targets --all-features
-- -D warnings`, the ordinary and explicit kernel suites above, and regeneration
of the README GIF with `python3 scripts/render_demo.py`. The GIF was
byte-identical. Both README images rendered in a local browser preview, and
relative documentation links and anchors passed validation. Gitleaks rescanned
the tracked tree and complete final history with no findings.

After pushing, both README images and the updated clone command were verified
on GitHub. The pre-release Actions run was inspected on 2026-10-07 and showed
a startup failure before any job ran. No remote ordinary-test or UFFD pass is
claimed; remote CI validation remains pending.

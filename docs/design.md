# EpochSnap v1 design

This document defines the consistency contract, ownership rules and Linux
capture protocol. [Testing](testing.md) records correctness coverage and atomic
access inspection; [benchmarks](benchmarks.md) cover measurements and replay.

## Purpose and limits

EpochSnap provides precise cooperative checkpoints for a Rust/Linux application
arena. Concurrent capture aims to reduce global checkpoint pause compared with
the stopped-copy reference. Linux/x86-64 is supported; Linux 6.6+ is the intended
baseline, subject to runtime capability checks, and compatibility with 6.6 remains
unverified. The mapping uses runtime base pages and disables THP before population.

V1 manages one fixed-size, private anonymous mapping per `Arena`. Its entire
payload is initialized and interpreted as uniform `AtomicU64` slots. The API
uses checked slot indices and copied `u64` values. Application records may encode
plain data in slots; no generic typed-object or allocator API is provided.

There is one application access/mutation owner. `Arena` is non-cloneable and
not `Sync`; payload mutation and lifecycle operations require exclusive owner
access. Moving ownership between threads is permissible if exclusive ownership
is preserved. The capture worker is the sole additional live-payload accessor.
No general admission gate or public access-session system is needed in v1.

Exclude arbitrary objects/pointers, simultaneous mutators, durable storage,
restart recovery, CRIU-style process state, shared/file mappings, huge pages,
dynamic growth, incremental/multiple checkpoints, lazy restore, and alternate
fault backends. Application fork, remap, unmap, protection changes, page discard,
alias writes, direct syscall-output buffers, and kernel/device/DMA writes into
the arena violate the contract. Stage I/O outside it.

## Precise consistency contract

The owner calls checkpoint between complete logical application operations.
For WP capture, it remains paused while setup and full write protection succeed.
Define T immediately after full arming succeeds and before the owner resumes.
For stopped capture, T is the owner-exclusive boundary at which copying starts.
The arena is unchanged by the owner throughout either setup boundary.

For every payload byte i, a successful checkpoint image S satisfies
`S[i] = arena_at_T[i]`. Padding outside the slot payload is not an unspecified
region: mapping length is page-rounded, all its slots are initialized and included.
The constructor rejects zero size, overflow, and sizes exceeding `isize::MAX`;
it reports the rounded slot capacity. It never silently wraps size arithmetic.
It allocates and populates the live mapping, one reusable full-image buffer, and
page bookkeeping before returning. Failure to prepare either payload or image
is a constructor failure, not a partially usable arena.

WP capture returns after T is established; the image may still be pending.
Application writes can block until their page is copied. A ready image is
immutable and complete. Readiness requires all pages saved, no remaining capture
WP, the worker joined, and the epoch's final UFFD descriptor closed. A pending
or failed image cannot be inspected or restored.

This is application-consistent only if the chosen operation boundary preserves
application invariants. Atomic slots alone do not make compound records atomic.
State required for replay, including RNG state and logical time, belongs in the
arena. External effects, local variables, stacks, heap allocations, and thread
execution do not rewind. The caller resumes from an appropriate control point.

## Storage and Rust safety

All concurrent payload loads and stores use `AtomicU64` with one consistent
access width and aligned addresses. Relaxed payload operations are sufficient
for race freedom; worker publication/control synchronization must provide the
ordering needed for its separate state. The checkpoint's byte precision also
depends on Linux protection semantics and compiler/call ordering; document that
argument and inspect the release capture loop when its code or toolchain changes.

No live `&[u64]`, `&mut [u64]`, byte slices, typed references, or raw access escapes.
The worker loads each slot atomically into an ordinary private image. It must not
copy live payload with non-atomic `memcpy`. Restore uses atomic stores for a
uniform discipline even though it has exclusive access. An immutable slice of
the completed image may be borrowed; it is separate storage and its borrow
prevents conflicting owner control calls through normal Rust borrowing.

`UnsafeCell`, volatile access, raw pointers, and an `unsafe` API do not establish
the missing synchronization/aliasing proof for ordinary stores plus concurrent
copying. That transparent mode is not part of v1. Uniform atomic storage avoids
that proof obligation without assuming that WP establishes Rust happens-before.

Keep mapping ownership and lifetime, atomic views, bounds/alignment arithmetic,
packed UFFD message decoding, and ioctl ABI access in a small audited unsafe
boundary. Internal mapping sharing with the worker may require an unsafe trait
implementation; its proof must cover atomic-only payload access and destruction
only after all worker references are gone. Do not manufacture integer pointers
for payload access when an offset from the original mapping pointer suffices.

## Minimal architecture and API

Use one package with private mapping/UFFD machinery, the owner-facing `Arena`,
and a capture worker. No backend trait: `CaptureMode::{Stopped, WriteProtected}`
selects two concrete paths sharing the same atomic image-copy routine.

Public operations, with `Result<T>` using the crate's error type:

- `Arena::new(requested_words: usize) -> Result<Arena>`
- `Arena::len_words(&self) -> usize`
- `Arena::load_word(&self, slot: usize) -> Result<u64>`
- `Arena::store_word(&mut self, slot: usize, value: u64) -> Result<()>`
- `Arena::checkpoint(&mut self, mode: CaptureMode) -> Result<Epoch>`
- `Arena::checkpoint_status(&mut self, epoch: Epoch) -> Result<CheckpointStatus>`
- `Arena::wait_checkpoint(&mut self, epoch: Epoch) -> Result<()>`
- `Arena::checkpoint_words(&self, epoch: Epoch) -> Result<&[u64]>`
- `Arena::restore(&mut self, epoch: Epoch) -> Result<()>`
- `Arena::discard_checkpoint(&mut self, epoch: Epoch) -> Result<()>`

`Epoch` is an opaque arena-identity/generation token, not application time.
Wrong-arena and stale tokens are rejected, including after discard. Status is
pending (saved/total pages) or ready with copied `CaptureMetrics`; errors are
reported separately. The stopped
path returns ready. The WP path returns only after arming acknowledgement.
Waiting finalizes worker ownership before reporting readiness.

`CaptureMetrics` contains saved-page counts by fault/scan origin, copied bytes,
the offset from checkpoint-call entry to T, and WP worker elapsed/CPU time.
Worker elapsed time runs from T through context closure, before owner join;
worker CPU time includes its setup, copying, and cleanup. WP-only fields are
absent for stopped capture. Stopped copied bytes/page counts use the same copy
routine. These are small per-capture counters/timestamps, not a telemetry system.

The implemented ready status is `Ready { metrics: CaptureMetrics }`. Its public
fields are `fault_pages`, `scan_pages`, `copied_bytes`, `boundary_offset`,
`ready_offset`, `worker_elapsed`, and `worker_cpu`. Durations use monotonic clocks.
`boundary_offset` ends at T, while `ready_offset` runs from checkpoint-call entry
through owner observation/join and is fixed on first finalization. Thus WP time
from T to owner-ready is `ready_offset - boundary_offset`; `worker_elapsed`
separately ends after context close before join. `worker_cpu` includes worker
setup through cleanup. WP worker durations are `Option<Duration>` and absent
for stopped copying, whose copied pages are counted in `scan_pages` and whose
`fault_pages` is zero. Failed/cancelled epochs publish no ready metrics.

One engine-owned image may be pending or ready. Another checkpoint returns busy
until explicit discard; it does not overwrite the existing image. Discard cancels
and tears down a pending capture, or invalidates a ready image. Keep the single
buffer for reuse, resetting page state before the next epoch. Every slot must be
overwritten during that capture before readiness; old buffer contents are never
evidence that a page was saved. A ready image can
be restored repeatedly until discarded. Peak engine storage is approximately
`2 * rounded_arena_bytes` plus O(page count) metadata/runtime overhead. Test
oracles and caller-made image copies are additional storage.

## Capture protocol

Use the image and bookkeeping prepared by `Arena::new`; transfer the one buffer
to the worker and recover it on completion or successful cancellation. Reuse
requires no public preparation API. Include state reset, worker/context setup,
registration, arming, and acknowledgement in checkpoint-call pause. Report
constructor allocation/population separately; do not hide it or claim only WP
ioctl time is the application pause. Prefaulting does not prevent later reclaim.

For each WP epoch, create a fresh context using
`O_CLOEXEC | O_NONBLOCK | UFFD_USER_MODE_ONLY`. Probe on a temporary descriptor,
then enable synchronous anonymous WP and `WP_UNPOPULATED` on the actual one.
Check generic and registered-range ioctl masks; do not enable `WP_ASYNC`.
Register the mapping in WP mode and arm its entire range. No saved-page release
is permitted until arming completes. Failed arming never establishes T.

One worker owns the descriptor, image, page state, and scanner. Between page
copies it checks cancellation and pending fault events; service faults before
advancing the scan. For a protected unsaved page: atomically read every slot,
record the saved state, then remove that page's WP and wake writers. A saved
page is never recopied. Treat fault events as demand hints, not exactly-once
delivery. Validate address and flags; handle already-saved notifications
idempotently. No second background copier, page-lock framework, or lock needed
by a faulting owner is introduced.

Close every descriptor reference and join the worker before retiring its epoch.
A fresh context prevents old queued messages from becoming a new epoch's work.
Registration/arming, page copies, unprotection, and teardown are measured costs.

## Restore, failures, and lifetime

Restore rejects pending checkpoints; the owner first waits or discards. Check
the token and ready image before any live store. Preserve the existing mapping,
restore every slot, and return only after completion. Worker/control metadata
stays outside the restored payload. No `UFFDIO_COPY` missing-page restore is used.

Constructor failures produce no arena. Capture preflight failures before
protection leave an existing arena usable.
Unexpected faults, worker panic, or capture failures after protection invalidate
the pending image and poison the arena. Publish terminal failure with release
ordering before any failure-driven unprotection or final descriptor closure
releases a blocked owner store; owner failure checks use acquire ordering.
The worker's unwind/cleanup ownership must preserve this order, including panic.
Cleanup closes the final context to
remove registrations/release blocked faults, joins the worker, and retains the
mapping until no worker can access it. Owner accesses check worker failure before
and after operations so a released in-flight store cannot hide capture failure.
No new checkpoint is accepted on a poisoned arena.

Intentional cancellation may return the arena to empty/usable only after
confirmed teardown. Drop performs the same ownership cleanup; it is permitted to
block. If safe cleanup cannot be established, explicit process termination is
preferable to continuing or abandoning blocked writers. No bounded scheduling,
kernel-failure recovery, or OOM survival guarantee is made. Test potential hangs
in subprocesses with an external deadline.

Linux calls retry interrupted (`EINTR`) attempts. A nonblocking fault read that
returns `EAGAIN` means no event is currently available; scanning may continue.
WP protection/unprotection retries `EAGAIN`, as documented by
[UFFDIO_WRITEPROTECT](https://man7.org/linux/man-pages/man2/UFFDIO_WRITEPROTECT.2const.html).
Other errors follow the preflight/active-failure policy above. Final descriptor
close is never retried: Linux may already have consumed the descriptor. Retry
handling does not establish a scheduling bound or guarantee recovery from a
persistent kernel stall.

Failure, cancellation and drop must preserve these cleanup guarantees. Fault
injection and adversarial tests verify each lifecycle exit, including blocked
writes; a worker must never be abandoned.

## Evidence and validation obligations

Kernel tests must prove real register/protect/fault/save/unprotect behavior and
that final descriptor closure releases a blocked writer. A feature bit is not
that proof. Stopped tests supply full-image equality and restore/replay oracles.
Concurrent capture tests compare WP images with an independent stopped image at
the same boundary and force scanner/fault orders. Lifecycle tests exercise
cleanup with pending faults. Benchmarks measure pause, completion, write stalls,
throughput and memory separately.

Miri can supplement ordinary-memory unsafe/lifetime checks; it does not exercise
the Linux UFFD path or certify precision. A concurrency-model framework is not
a default dependency; add one only for a specific unresolved shared-state test.

## Primary references

- [Kernel userfaultfd semantics](https://docs.kernel.org/admin-guide/mm/userfaultfd.html)
- [Creation, restrictions, and descriptor release](https://man7.org/linux/man-pages/man2/userfaultfd.2.html)
- [Feature negotiation](https://man7.org/linux/man-pages/man2/UFFDIO_API.2const.html)
- [WP registration](https://man7.org/linux/man-pages/man2/UFFDIO_REGISTER.2const.html)
- [Protection and waking](https://man7.org/linux/man-pages/man2/UFFDIO_WRITEPROTECT.2const.html)
- [Linux 6.6 UAPI header](https://github.com/torvalds/linux/blob/v6.6/include/uapi/linux/userfaultfd.h)
- [Linux 6.6 descriptor release implementation](https://github.com/torvalds/linux/blob/v6.6/fs/userfaultfd.c)
- [Mapping advice](https://man7.org/linux/man-pages/man2/madvise.2.html)
- [Rust atomic rules](https://doc.rust-lang.org/std/sync/atomic/index.html)
- [AtomicU64 layout](https://doc.rust-lang.org/std/sync/atomic/type.AtomicU64.html)
- [Rust undefined behavior](https://doc.rust-lang.org/reference/behavior-considered-undefined.html)
- [UnsafeCell limitations](https://doc.rust-lang.org/std/cell/struct.UnsafeCell.html)

// Exercise private modules without exporting a test-only engine API.
use epochsnap::{Capabilities, Error, Result};
#[path = "../src/linux.rs"]
mod linux;
#[path = "../src/mapping.rs"]
mod mapping;

use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn isolated(name: &str, case: fn()) {
    isolated_exit(name, case, None);
}

fn isolated_exit(name: &str, case: fn(), expected_signal: Option<i32>) {
    use std::os::unix::process::ExitStatusExt;
    if std::env::var("EPOCHSNAP_KERNEL_CHILD").as_deref() == Ok(name) {
        if expected_signal.is_some() {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid initialized resource limit; affects only this child
            // and prevents deliberate fatal-exit tests from creating core files.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) }, 0);
        }
        case();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("EPOCHSNAP_KERNEL_CHILD", name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out protocol child");
            let output = child.wait_with_output().unwrap();
            panic!(
                "{name}: external 10s deadline; child killed/reaped\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Watchdog polling only: none of the protocol assertions depends on sleep.
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        if let Some(signal) = expected_signal {
            output.status.signal() == Some(signal)
        } else {
            output.status.success()
        },
        "{name}: child failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepared() -> (Arc<mapping::Mapping>, linux::Registered, Vec<u64>) {
    let map = Arc::new(mapping::Mapping::new(1).unwrap());
    let mut image = vec![0; map.len_words()];
    for slot in 0..map.len_words() {
        map.store(slot, 0xaabb_0000 + slot as u64).unwrap();
    }
    map.copy_page(0, &mut image).unwrap();
    let mut registered = linux::Registered::new(Arc::clone(&map)).unwrap();
    registered.protect().unwrap();
    (map, registered, image)
}

fn writer(
    map: Arc<mapping::Mapping>,
    failure: Arc<AtomicBool>,
) -> (thread::JoinHandle<Result<()>>, mpsc::Receiver<()>) {
    let (send, done) = mpsc::channel();
    let join = thread::spawn(move || {
        if failure.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        map.store(0, 0xdead_beef)?;
        let result = if failure.load(Ordering::Acquire) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        };
        send.send(()).unwrap();
        result
    });
    (join, done)
}

fn blocked(context: &linux::Registered, map: &mapping::Mapping, done: &mpsc::Receiver<()>) {
    let fault = context.wait_fault(Duration::from_secs(2)).unwrap();
    assert!(fault.address >= map.address() as u64);
    assert!(fault.address < (map.address() + map.len_words() * 8) as u64);
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(map.load(0).unwrap(), 0xaabb_0000);
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn one_page_preserves_prewrite() {
    isolated("one_page_preserves_prewrite", || {
        let (map, mut context, expected) = prepared();
        assert_eq!(context.capabilities.page_size, map.len_bytes());
        let mut image = vec![0; map.len_words()];
        let failure = context.failure();
        let (owner, done) = writer(Arc::clone(&map), Arc::clone(&failure));
        blocked(&context, &map, &done);
        map.copy_page(0, &mut image).unwrap();
        assert_eq!(image, expected);
        context.unprotect().unwrap();
        // Keep the final descriptor open: closing it must not mask a broken
        // unprotect ioctl. The parent deadline bounds a blocked join.
        assert_eq!(owner.join().unwrap(), Ok(()));
        drop(context);
        assert!(!failure.load(Ordering::Acquire));
        assert_eq!(map.load(0), Ok(0xdead_beef));
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn close_releases_faulted_writer() {
    isolated("close_releases_faulted_writer", || {
        let (map, context, _) = prepared();
        let failure = context.failure();
        let (owner, done) = writer(Arc::clone(&map), Arc::clone(&failure));
        blocked(&context, &map, &done);
        drop(context);
        assert_eq!(owner.join().unwrap(), Err(Error::Poisoned));
        assert!(failure.load(Ordering::Acquire));
        assert_eq!(map.load(0), Ok(0xdead_beef));
        map.store(map.len_words() - 1, 77).unwrap();
        assert_eq!(map.load(map.len_words() - 1), Ok(77));
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn panic_publishes_failure_before_release() {
    isolated("panic_publishes_failure_before_release", || {
        let (map, context, _) = prepared();
        let failure = context.failure();
        let (owner, done) = writer(Arc::clone(&map), failure);
        blocked(&context, &map, &done);
        let handler = thread::spawn(move || {
            let _owned_context = context;
            panic!("injected handler panic while owner is faulted");
        });
        assert!(handler.join().is_err());
        assert_eq!(owner.join().unwrap(), Err(Error::Poisoned));
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn error_publishes_failure_before_release() {
    isolated("error_publishes_failure_before_release", || {
        let (map, context, _) = prepared();
        let (owner, done) = writer(Arc::clone(&map), context.failure());
        blocked(&context, &map, &done);
        // The only event was consumed. A real poll timeout is a checked service error.
        assert_eq!(
            context.wait_fault(Duration::from_millis(20)),
            Err(Error::Timeout)
        );
        drop(context);
        assert_eq!(owner.join().unwrap(), Err(Error::Poisoned));
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn wp_image_matches_stopped_oracle() {
    isolated("wp_image_matches_stopped_oracle", || {
        use epochsnap::{Arena, CaptureMode};
        let page = mapping::page_size().unwrap() / 8;
        let mut arena = Arena::new(4 * page).unwrap();
        for slot in 0..3 * page {
            arena
                .store_word(slot, (slot as u64).wrapping_mul(0x9e37_79b9) ^ 37)
                .unwrap();
        }
        let stopped = arena.checkpoint(CaptureMode::Stopped).unwrap();
        // This independent oracle is caller-owned extra storage, outside the
        // engine's one reusable image and its approximately 2x payload budget.
        let oracle = arena.checkpoint_words(stopped).unwrap().to_vec();
        arena.discard_checkpoint(stopped).unwrap();
        let wp = arena.checkpoint(CaptureMode::WriteProtected).unwrap();
        for pass in 1..=3 {
            for slot in (0..arena.len_words()).rev() {
                arena.store_word(slot, pass + slot as u64).unwrap();
            }
        }
        arena.wait_checkpoint(wp).unwrap();
        assert_eq!(arena.checkpoint_words(wp).unwrap(), oracle);
        let epochsnap::CheckpointStatus::Ready { metrics } = arena.checkpoint_status(wp).unwrap()
        else {
            panic!("public WP image must be ready after join");
        };
        assert_eq!(metrics.fault_pages + metrics.scan_pages, 4);
        assert_eq!(metrics.copied_bytes, 4 * page * 8);
        assert!(metrics.boundary_offset + metrics.worker_elapsed.unwrap() <= metrics.ready_offset);
        assert!(metrics.worker_cpu.is_some());
        arena.restore(wp).unwrap();
        for (slot, expected) in oracle.into_iter().enumerate() {
            assert_eq!(arena.load_word(slot), Ok(expected));
        }
    });
}

// Compile the same private owner/worker code with internal schedule hooks; the
// public oracle above additionally exercises the shipped library. No hook or
// live access API is exported by the library.
#[path = "../src/arena.rs"]
mod arena;
#[path = "../src/capture.rs"]
mod capture;

fn scheduled_arena(hooks: capture::Hooks) -> (arena::Arena, Vec<u64>) {
    let page = mapping::page_size().unwrap() / 8;
    let mut arena = arena::Arena::new(3 * page).unwrap();
    for slot in 0..2 * page {
        arena.store_word(slot, 41 + slot as u64).unwrap();
    }
    let stopped = arena.checkpoint(arena::CaptureMode::Stopped).unwrap();
    let oracle = arena.checkpoint_words(stopped).unwrap().to_vec();
    arena.discard_checkpoint(stopped).unwrap();
    arena.capture_hooks = Some(hooks);
    (arena, oracle)
}

fn ready_metrics(arena: &mut arena::Arena, epoch: arena::Epoch) -> arena::CaptureMetrics {
    let arena::CheckpointStatus::Ready { metrics } = arena.checkpoint_status(epoch).unwrap() else {
        panic!("joined capture must be ready");
    };
    assert_eq!(
        metrics.fault_pages + metrics.scan_pages,
        arena.len_words() * 8 / mapping::page_size().unwrap()
    );
    assert_eq!(metrics.copied_bytes, arena.len_words() * 8);
    assert!(metrics.boundary_offset + metrics.worker_elapsed.unwrap() <= metrics.ready_offset);
    assert!(metrics.worker_cpu.is_some());
    metrics
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn first_write_before_scan_has_fault_priority() {
    isolated("first_write_before_scan_has_fault_priority", || {
        let (saved, received) = mpsc::channel();
        let mut first = true;
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::BeforeDrain && first {
                first = false;
                context.unwrap().wait_readable(Duration::from_secs(2))?;
            }
            if let capture::Event::Copied(page) = event {
                saved.send(page).unwrap();
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        let page = mapping::page_size().unwrap() / 8;
        // Last page is zero-filled at T, and its first write is received before
        // the scanner is allowed to start. Fault priority must save it first.
        for value in [91, 92, 93] {
            arena.store_word(2 * page + 7, value).unwrap();
        }
        assert_eq!(received.recv().unwrap(), 2);
        for slot in [0, page - 1, page, 2 * page - 1] {
            arena.store_word(slot, u64::MAX).unwrap();
        }
        arena.wait_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
        assert!(ready_metrics(&mut arena, epoch).fault_pages >= 1);
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn scan_before_write_allows_progress_and_rejects_pending_operations() {
    isolated(
        "scan_before_write_allows_progress_and_rejects_pending_operations",
        || {
            let (paused, reached) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let hooks = capture::Hooks::new(move |event, _| {
                if event == capture::Event::BeforeScan(1) {
                    paused.send(()).unwrap();
                    gate.recv().unwrap();
                }
                Ok(None)
            });
            let (mut arena, oracle) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            reached.recv().unwrap();
            assert_eq!(
                arena.checkpoint_status(epoch),
                Ok(arena::CheckpointStatus::Pending {
                    saved_pages: 1,
                    total_pages: 3
                })
            );
            assert_eq!(arena.checkpoint_words(epoch), Err(Error::Pending));
            assert_eq!(arena.restore(epoch), Err(Error::Pending));
            for mode in [
                arena::CaptureMode::Stopped,
                arena::CaptureMode::WriteProtected,
            ] {
                assert_eq!(arena.checkpoint(mode), Err(Error::Busy));
            }
            for value in [71, 72, 73] {
                arena.store_word(0, value).unwrap();
            }
            assert_eq!(arena.load_word(0), Ok(73));
            assert!(matches!(
                arena.checkpoint_status(epoch),
                Ok(arena::CheckpointStatus::Pending { saved_pages: 1, .. })
            ));
            release.send(()).unwrap();
            arena.wait_checkpoint(epoch).unwrap();
            assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
            assert_eq!(ready_metrics(&mut arena, epoch).scan_pages, 3);
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn delayed_real_fault_never_recopies_a_scanned_page() {
    isolated("delayed_real_fault_never_recopies_a_scanned_page", || {
        let (unprotected, released) = mpsc::channel();
        let (resume, gate) = mpsc::channel();
        let (copied, copies) = mpsc::channel();
        let (scanning, selected) = mpsc::channel();
        let mut first_release = true;
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::BeforeScan(0) {
                // Do not let the owner queue its fault before the scanner has
                // selected page 0; otherwise fault priority is a valid rival
                // schedule and this case never exercises a delayed notification.
                scanning.send(()).unwrap();
                // Read a real queued fault, then defer its demand hint until the
                // already-selected scanner has saved/unprotected this page.
                return Ok(Some(context.unwrap().wait_fault(Duration::from_secs(2))?));
            }
            if let capture::Event::Copied(page) = event {
                copied.send(page).unwrap();
            }
            if event == capture::Event::Unprotected(0) && first_release {
                first_release = false;
                unprotected.send(()).unwrap();
                gate.recv().unwrap();
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        selected.recv().unwrap();
        arena.store_word(0, 1001).unwrap();
        released.recv().unwrap();
        arena.store_word(0, 1002).unwrap();
        resume.send(()).unwrap();
        arena.wait_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
        assert_eq!(copies.try_iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        let metrics = ready_metrics(&mut arena, epoch);
        assert_eq!((metrics.fault_pages, metrics.scan_pages), (0, 3));
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn readiness_waits_for_context_close_and_worker_join() {
    isolated("readiness_waits_for_context_close_and_worker_join", || {
        let (closed, reached) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::Closed {
                assert!(context.is_none());
                closed.send(()).unwrap();
                gate.recv().unwrap();
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        reached.recv().unwrap();
        assert_eq!(
            arena.checkpoint_status(epoch),
            Ok(arena::CheckpointStatus::Pending {
                saved_pages: 3,
                total_pages: 3
            })
        );
        assert_eq!(arena.checkpoint_words(epoch), Err(Error::Pending));
        assert_eq!(arena.restore(epoch), Err(Error::Pending));
        arena.store_word(0, 123).unwrap();
        release.send(()).unwrap();
        arena.wait_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
        ready_metrics(&mut arena, epoch);
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn partial_arm_failure_rolls_back_without_publishing_an_epoch() {
    isolated(
        "partial_arm_failure_rolls_back_without_publishing_an_epoch",
        || {
            let mut hooks = capture::Hooks::new(|_, _| Ok(None));
            hooks.partial_arm_failure = true;
            let (mut arena, _) = scheduled_arena(hooks);
            assert_eq!(
                arena.checkpoint(arena::CaptureMode::WriteProtected),
                Err(Error::Protocol("injected partial arming failure"))
            );
            assert_eq!(arena.load_word(0), Err(Error::Poisoned));
            assert_eq!(arena.store_word(0, 1), Err(Error::Poisoned));
            assert_eq!(
                arena.checkpoint(arena::CaptureMode::Stopped),
                Err(Error::Poisoned)
            );
            // Actual protection must be removed even though no epoch was returned.
            arena.mapping.store(0, 81).unwrap();
            assert_eq!(arena.mapping.load(0), Ok(81));
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn preflight_failure_retains_storage_and_usability() {
    isolated("preflight_failure_retains_storage_and_usability", || {
        let hooks = capture::Hooks::new(|event, _| {
            if event == capture::Event::BeforeArm {
                return Err(Error::Protocol("injected preflight failure"));
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        assert_eq!(
            arena.checkpoint(arena::CaptureMode::WriteProtected),
            Err(Error::Protocol("injected preflight failure"))
        );
        arena.store_word(0, oracle[0]).unwrap();
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        arena.wait_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn arena_worker_panic_poisons_the_faulted_owner_before_release() {
    isolated(
        "arena_worker_panic_poisons_the_faulted_owner_before_release",
        || {
            let mut first = true;
            let hooks = capture::Hooks::new(move |event, context| {
                if event == capture::Event::BeforeDrain && first {
                    first = false;
                    context.unwrap().wait_readable(Duration::from_secs(2))?;
                }
                if matches!(event, capture::Event::Fault(_)) {
                    panic!("injected arena worker panic with a faulted owner");
                }
                Ok(None)
            });
            let (mut arena, _) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            assert_eq!(arena.store_word(0, 777), Err(Error::Poisoned));
            assert_eq!(arena.load_word(0), Err(Error::Poisoned));
            assert_eq!(arena.checkpoint_words(epoch), Err(Error::Poisoned));
            assert_eq!(arena.restore(epoch), Err(Error::Poisoned));
            assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
            assert_eq!(arena.checkpoint_status(epoch), Err(Error::Poisoned));
            assert_eq!(
                arena.checkpoint(arena::CaptureMode::Stopped),
                Err(Error::Poisoned)
            );
            assert_eq!(arena.mapping.load(0), Ok(777));
        },
    );
}

fn cancellation_hooks() -> (
    capture::Hooks,
    mpsc::Receiver<()>,
    mpsc::Sender<Arc<AtomicBool>>,
) {
    let (paused, reached) = mpsc::channel();
    let (flag, receiver) = mpsc::channel::<Arc<AtomicBool>>();
    let mut first = true;
    let hooks = capture::Hooks::new(move |event, _| {
        if event == capture::Event::BeforeDrain && first {
            first = false;
            paused.send(()).unwrap();
            let cancel = receiver.recv().unwrap();
            while !cancel.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        Ok(None)
    });
    (hooks, reached, flag)
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn pending_discard_cancels_and_reuses_the_prepared_buffer() {
    isolated(
        "pending_discard_cancels_and_reuses_the_prepared_buffer",
        || {
            let (hooks, reached, flag) = cancellation_hooks();
            let (mut arena, oracle) = scheduled_arena(hooks);
            let image_address = arena.image.as_ptr();
            let pages_address = arena.page_saved.as_ptr();
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            reached.recv().unwrap();
            flag.send(arena.capture.as_ref().unwrap().cancel_flag())
                .unwrap();
            arena.discard_checkpoint(epoch).unwrap();
            assert_eq!(arena.image.as_ptr(), image_address);
            assert_eq!(arena.page_saved.as_ptr(), pages_address);
            assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
            arena.store_word(0, oracle[0]).unwrap();
            let next = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            arena.wait_checkpoint(next).unwrap();
            assert_eq!(arena.checkpoint_words(next).unwrap(), oracle);
            arena.restore(next).unwrap();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn pending_arena_drop_joins_worker_before_mapping_retirement() {
    isolated(
        "pending_arena_drop_joins_worker_before_mapping_retirement",
        || {
            let (hooks, reached, flag) = cancellation_hooks();
            let (mut arena, _) = scheduled_arena(hooks);
            let mapping = Arc::clone(&arena.mapping);
            arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            reached.recv().unwrap();
            flag.send(arena.capture.as_ref().unwrap().cancel_flag())
                .unwrap();
            drop(arena);
            // A private retained Arc is test observation only: after drop returns,
            // the worker has joined and every protected slot must be writable.
            for slot in 0..mapping.len_words() {
                mapping.store(slot, 55).unwrap();
            }
            assert_eq!(mapping.load(mapping.len_words() - 1), Ok(55));
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn arming_acknowledgement_precedes_owner_resume() {
    isolated("arming_acknowledgement_precedes_owner_resume", || {
        let (arming, reached) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (returned, acknowledgement) = mpsc::channel();
        let mut first = true;
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::BeforeArm {
                arming.send(()).unwrap();
                gate.recv().unwrap();
            }
            if event == capture::Event::BeforeDrain && first {
                first = false;
                context.unwrap().wait_readable(Duration::from_secs(2))?;
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        let owner = thread::spawn(move || {
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            returned.send(()).unwrap();
            // Demand from the final page must reach the fully armed worker.
            arena.store_word(arena.len_words() - 1, 987).unwrap();
            arena.wait_checkpoint(epoch).unwrap();
            (arena, epoch)
        });
        reached.recv().unwrap();
        assert!(matches!(
            acknowledgement.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        acknowledgement.recv().unwrap();
        let (mut arena, epoch) = owner.join().unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
        assert_eq!(ready_metrics(&mut arena, epoch).fault_pages, 1);
    });
}

fn queued_fault_cleanup(drop_pending: bool) {
    let (paused, reached) = mpsc::channel();
    let (flag, receiver) = mpsc::channel::<Arc<AtomicBool>>();
    let mut first = true;
    let hooks = capture::Hooks::new(move |event, context| {
        if event == capture::Event::BeforeDrain && first {
            first = false;
            context.unwrap().wait_readable(Duration::from_secs(2))?;
            paused.send(()).unwrap();
            let cancel = receiver.recv().unwrap();
            while !cancel.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        Ok(None)
    });
    let (mut arena, _) = scheduled_arena(hooks);
    let epoch = arena
        .checkpoint(arena::CaptureMode::WriteProtected)
        .unwrap();
    let live = Arc::clone(&arena.mapping);
    let (done, completion) = mpsc::channel();
    let writer = thread::spawn(move || {
        live.store(0, 333).unwrap();
        done.send(()).unwrap();
    });
    reached.recv().unwrap();
    // The kernel reports a queued WP fault, and its store is still blocked.
    assert!(matches!(
        completion.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    flag.send(arena.capture.as_ref().unwrap().cancel_flag())
        .unwrap();
    if drop_pending {
        drop(arena);
    } else {
        arena.discard_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
        writer.join().unwrap();
        completion.recv().unwrap();
        assert_eq!(arena.load_word(0), Ok(333));
        let next = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        arena.wait_checkpoint(next).unwrap();
        assert_eq!(arena.checkpoint_words(next).unwrap()[0], 333);
        return;
    }
    writer.join().unwrap();
    completion.recv().unwrap();
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn pending_discard_releases_a_queued_fault() {
    isolated("pending_discard_releases_a_queued_fault", || {
        queued_fault_cleanup(false)
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn pending_drop_releases_a_queued_fault() {
    isolated("pending_drop_releases_a_queued_fault", || {
        queued_fault_cleanup(true)
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn arena_worker_error_poisons_the_faulted_owner_before_release() {
    isolated(
        "arena_worker_error_poisons_the_faulted_owner_before_release",
        || {
            let mut first = true;
            let hooks = capture::Hooks::new(move |event, context| {
                if event == capture::Event::BeforeDrain && first {
                    first = false;
                    context.unwrap().wait_readable(Duration::from_secs(2))?;
                }
                if matches!(event, capture::Event::Fault(_)) {
                    return Err(Error::Protocol("injected active service error"));
                }
                Ok(None)
            });
            let (mut arena, _) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            assert_eq!(arena.store_word(0, 888), Err(Error::Poisoned));
            assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
            assert_eq!(arena.checkpoint_words(epoch), Err(Error::Poisoned));
            assert_eq!(arena.mapping.load(0), Ok(888));
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn completed_wp_reuses_storage_and_restores_the_same_mapping() {
    isolated(
        "completed_wp_reuses_storage_and_restores_the_same_mapping",
        || {
            let (mut arena, _) = scheduled_arena(capture::Hooks::new(|_, _| Ok(None)));
            let image_address = arena.image.as_ptr();
            let pages_address = arena.page_saved.as_ptr();
            let mapping_address = arena.mapping.address();
            let page = mapping::page_size().unwrap() / 8;
            for pass in [0, 1] {
                for slot in 0..arena.len_words() {
                    let value = if pass == 0 {
                        slot as u64 + 1
                    } else if slot >= page {
                        0
                    } else {
                        17
                    };
                    arena.store_word(slot, value).unwrap();
                }
                let stopped = arena.checkpoint(arena::CaptureMode::Stopped).unwrap();
                let oracle = arena.checkpoint_words(stopped).unwrap().to_vec();
                arena.discard_checkpoint(stopped).unwrap();
                let epoch = arena
                    .checkpoint(arena::CaptureMode::WriteProtected)
                    .unwrap();
                arena.store_word(0, u64::MAX).unwrap();
                arena.wait_checkpoint(epoch).unwrap();
                let first_status = arena.checkpoint_status(epoch).unwrap();
                assert_eq!(arena.checkpoint_status(epoch).unwrap(), first_status);
                assert_eq!(arena.image.as_ptr(), image_address);
                assert_eq!(arena.page_saved.as_ptr(), pages_address);
                assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
                for _ in 0..2 {
                    arena.restore(epoch).unwrap();
                    assert_eq!(arena.mapping.address(), mapping_address);
                    for (slot, &expected) in oracle.iter().enumerate() {
                        assert_eq!(arena.load_word(slot), Ok(expected));
                    }
                    arena.store_word(0, u64::MAX).unwrap();
                }
                ready_metrics(&mut arena, epoch);
                arena.discard_checkpoint(epoch).unwrap();
            }
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn wp_capture_overwrites_a_stale_prepared_image() {
    isolated("wp_capture_overwrites_a_stale_prepared_image", || {
        use epochsnap::{Arena, CaptureMode};
        let page = mapping::page_size().unwrap() / 8;
        let mut arena = Arena::new(3 * page).unwrap();
        for slot in 0..arena.len_words() {
            arena.store_word(slot, 29).unwrap();
        }
        let old = arena.checkpoint(CaptureMode::Stopped).unwrap();
        arena.discard_checkpoint(old).unwrap();
        // Unlike the same-boundary oracle case, the reusable image now contains
        // a DIFFERENT old state. Skipping a copy cannot succeed by reusing it.
        // Expected values are caller-owned and assigned independently of capture.
        let expected: Vec<_> = (0..arena.len_words())
            .map(|slot| {
                if (page..2 * page).contains(&slot) {
                    100 + slot as u64
                } else {
                    0
                }
            })
            .collect();
        for (slot, &value) in expected.iter().enumerate() {
            arena.store_word(slot, value).unwrap();
        }
        let epoch = arena.checkpoint(CaptureMode::WriteProtected).unwrap();
        for slot in (0..arena.len_words()).rev() {
            arena.store_word(slot, u64::MAX).unwrap();
        }
        arena.wait_checkpoint(epoch).unwrap();
        for (slot, &value) in expected.iter().enumerate() {
            assert_eq!(
                arena.checkpoint_words(epoch).unwrap()[slot],
                value,
                "slot {slot}"
            );
        }
    });
}

#[path = "support/lifecycle.rs"]
mod lifecycle;

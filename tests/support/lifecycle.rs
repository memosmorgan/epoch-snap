//! Lifecycle cases run in real-kernel children, with no hooks in the public library.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn run(name: &str, case: fn()) {
    isolated(&format!("lifecycle::{name}"), case);
}

#[derive(Debug, PartialEq, Eq)]
struct Resources {
    descriptors: BTreeMap<String, std::path::PathBuf>,
    threads: BTreeSet<String>,
}
impl Resources {
    fn current() -> Self {
        let observer = format!("/proc/{}/fd", std::process::id());
        let descriptors = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let target = std::fs::read_link(entry.path()).ok()?;
                // read_dir's own descriptor is temporary observation storage.
                if target == std::path::Path::new(&observer) {
                    return None;
                }
                Some((entry.file_name().to_string_lossy().into_owned(), target))
            })
            .collect();
        let threads = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        Self {
            descriptors,
            threads,
        }
    }
    fn assert_released(&self) {
        // pthread join can precede the final disappearance of a task's /proc
        // entry. Observe that exit with yields; the parent bounds the whole case.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let after = Self::current();
            if after == *self {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "resources abandoned: before {self:?}, after {after:?}"
            );
            thread::yield_now();
        }
    }
}

fn assert_poisoned(arena: &mut arena::Arena, epoch: arena::Epoch) {
    for slot in [0, arena.len_words() - 1, usize::MAX] {
        let before = arena.mapping.load(0).unwrap();
        assert_eq!(arena.load_word(slot), Err(Error::Poisoned));
        assert_eq!(arena.store_word(slot, 999), Err(Error::Poisoned));
        assert_eq!(
            arena.mapping.load(0),
            Ok(before),
            "pre-check must reject mutation"
        );
    }
    assert_eq!(arena.checkpoint_words(epoch), Err(Error::Poisoned));
    assert_eq!(arena.restore(epoch), Err(Error::Poisoned));
    assert_eq!(arena.checkpoint_status(epoch), Err(Error::Poisoned));
    assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
    for mode in [
        arena::CaptureMode::Stopped,
        arena::CaptureMode::WriteProtected,
    ] {
        assert_eq!(arena.checkpoint(mode), Err(Error::Poisoned));
    }
    assert_eq!(arena.discard_checkpoint(epoch), Err(Error::Poisoned));
    assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
    // Every page is writable after the failed worker has been joined, even
    // though safe owner access remains permanently poisoned.
    for slot in 0..arena.len_words() {
        arena.mapping.store(slot, 77).unwrap();
    }
    assert!(arena.capture.is_none());
    assert_eq!(Arc::strong_count(&arena.mapping), 1);
}

#[derive(Clone, Copy)]
enum FailureStage {
    Copy,
    PageUnprotect,
    FinalUnprotect,
    Publication,
}

fn active_failure(stage: FailureStage) {
    let resources = Resources::current();
    let mut first = true;
    let hooks = capture::Hooks::new(move |event, context| {
        if event == capture::Event::BeforeDrain && first {
            first = false;
            context.unwrap().wait_readable(Duration::from_secs(2))?;
        }
        match (stage, event) {
            (FailureStage::PageUnprotect, capture::Event::Copied(0)) => {
                context
                    .unwrap()
                    .inject_errors(linux::Call::Unprotect, &[libc::EIO]);
            }
            (FailureStage::FinalUnprotect, capture::Event::Unprotected(2)) => {
                context
                    .unwrap()
                    .inject_errors(linux::Call::Unprotect, &[libc::EIO]);
            }
            (FailureStage::Publication, capture::Event::BeforePublish) => {
                return Err(Error::Protocol("injected publication failure"));
            }
            _ => {}
        }
        Ok(None)
    });
    let (mut arena, _) = scheduled_arena(hooks);
    if matches!(stage, FailureStage::Copy) {
        // Fail after half of a real atomic page copy: old buffer data cannot be
        // mistaken for a complete image, and the blocked owner must be released.
        arena
            .mapping
            .fail_copy_at(mapping::page_size().unwrap() / 16);
    }
    let epoch = arena
        .checkpoint(arena::CaptureMode::WriteProtected)
        .unwrap();
    let stored = arena.store_word(0, 456);
    if matches!(stage, FailureStage::Copy | FailureStage::PageUnprotect) {
        assert_eq!(
            stored,
            Err(Error::Poisoned),
            "released blocked store must see failure"
        );
    } else {
        assert!(matches!(stored, Ok(()) | Err(Error::Poisoned)));
    }
    assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
    assert_poisoned(&mut arena, epoch);
    resources.assert_released();
    drop(arena);
    resources.assert_released();
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn partial_page_copy_failure_never_publishes_ready() {
    run("partial_page_copy_failure_never_publishes_ready", || {
        active_failure(FailureStage::Copy)
    });
}
#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn page_unprotect_error_poisons_before_releasing_owner() {
    run(
        "page_unprotect_error_poisons_before_releasing_owner",
        || active_failure(FailureStage::PageUnprotect),
    );
}
#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn final_unprotect_error_never_publishes_ready() {
    run("final_unprotect_error_never_publishes_ready", || {
        active_failure(FailureStage::FinalUnprotect)
    });
}
#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn publication_error_after_close_never_publishes_ready() {
    run(
        "publication_error_after_close_never_publishes_ready",
        || active_failure(FailureStage::Publication),
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn transient_read_and_wp_errors_retry_real_operations() {
    run("transient_read_and_wp_errors_retry_real_operations", || {
        let resources = Resources::current();
        let mut first = true;
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::BeforeArm {
                context
                    .unwrap()
                    .inject_errors(linux::Call::Protect, &[libc::EINTR, libc::EAGAIN]);
            }
            if event == capture::Event::BeforeDrain && first {
                first = false;
                context.unwrap().wait_readable(Duration::from_secs(2))?;
                context
                    .unwrap()
                    .inject_errors(linux::Call::Read, &[libc::EINTR, libc::EINTR]);
            }
            if let capture::Event::Copied(_) = event {
                context
                    .unwrap()
                    .inject_errors(linux::Call::Unprotect, &[libc::EINTR, libc::EAGAIN]);
            }
            if event == capture::Event::Unprotected(2) {
                context
                    .unwrap()
                    .inject_errors(linux::Call::Unprotect, &[libc::EAGAIN]);
                context
                    .unwrap()
                    .inject_errors(linux::Call::Read, &[libc::EAGAIN]);
                assert_eq!(context.unwrap().read_fault(), Ok(None));
            }
            if event == capture::Event::BeforeClose {
                assert_eq!(context.unwrap().pending_errors(), 0);
            }
            Ok(None)
        });
        let (mut arena, oracle) = scheduled_arena(hooks);
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        arena.store_word(0, 456).unwrap();
        arena.wait_checkpoint(epoch).unwrap();
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), oracle);
        assert_eq!(ready_metrics(&mut arena, epoch).fault_pages, 1);
        arena.restore(epoch).unwrap();
        arena.discard_checkpoint(epoch).unwrap();
        assert_eq!(Arc::strong_count(&arena.mapping), 1);
        resources.assert_released();
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn preflight_panic_retains_usable_arena_and_releases_resources() {
    run(
        "preflight_panic_retains_usable_arena_and_releases_resources",
        || {
            let resources = Resources::current();
            let (mut arena, oracle) = scheduled_arena(capture::Hooks::new(|event, _| {
                if event == capture::Event::BeforeArm {
                    panic!("preflight panic");
                }
                Ok(None)
            }));
            let image = arena.image.as_ptr();
            assert_eq!(
                arena.checkpoint(arena::CaptureMode::WriteProtected),
                Err(Error::Protocol("capture worker panicked"))
            );
            assert_eq!(arena.image.as_ptr(), image);
            assert_eq!(Arc::strong_count(&arena.mapping), 1);
            resources.assert_released();
            arena.store_word(0, oracle[0]).unwrap();
            let next = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            arena.wait_checkpoint(next).unwrap();
            assert_eq!(arena.checkpoint_words(next).unwrap(), oracle);
            arena.discard_checkpoint(next).unwrap();
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn partial_arm_failure_releases_all_resources_and_never_mutates_after_poison() {
    run(
        "partial_arm_failure_releases_all_resources_and_never_mutates_after_poison",
        || {
            let resources = Resources::current();
            let mut hooks = capture::Hooks::new(|_, _| Ok(None));
            hooks.partial_arm_failure = true;
            let (mut arena, _) = scheduled_arena(hooks);
            let image = arena.image.as_ptr();
            assert!(
                arena
                    .checkpoint(arena::CaptureMode::WriteProtected)
                    .is_err()
            );
            assert_eq!(arena.image.as_ptr(), image);
            assert_eq!(arena.store_word(0, 999), Err(Error::Poisoned));
            assert_eq!(arena.mapping.load(0), Ok(41));
            assert_eq!(arena.load_word(0), Err(Error::Poisoned));
            for mode in [
                arena::CaptureMode::Stopped,
                arena::CaptureMode::WriteProtected,
            ] {
                assert_eq!(arena.checkpoint(mode), Err(Error::Poisoned));
            }
            for slot in 0..arena.len_words() {
                arena.mapping.store(slot, 0).unwrap();
            }
            assert_eq!(Arc::strong_count(&arena.mapping), 1);
            resources.assert_released();
            drop(arena);
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn queued_cancellation_and_drop_abandon_no_resources() {
    run("queued_cancellation_and_drop_abandon_no_resources", || {
        let resources = Resources::current();
        for drop_pending in [false, true] {
            queued_fault_cleanup(drop_pending);
            resources.assert_released();
        }
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn cancellation_cleanup_error_poisons_and_releases_queued_writer() {
    run(
        "cancellation_cleanup_error_poisons_and_releases_queued_writer",
        || {
            let resources = Resources::current();
            let (paused, reached) = mpsc::channel();
            let (send_flag, receive_flag) = mpsc::channel::<Arc<AtomicBool>>();
            let hooks = capture::Hooks::new(move |event, context| {
                if event == capture::Event::BeforeDrain {
                    let context = context.unwrap();
                    context.wait_readable(Duration::from_secs(2))?;
                    context.inject_errors(linux::Call::Unprotect, &[libc::EIO]);
                    paused.send(()).unwrap();
                    let cancel = receive_flag.recv().unwrap();
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
            let mapping = Arc::clone(&arena.mapping);
            let writer = thread::spawn(move || mapping.store(0, 123));
            reached.recv().unwrap(); // real pending WP fault, not a scheduling guess
            send_flag
                .send(arena.capture.as_ref().unwrap().cancel_flag())
                .unwrap();
            assert_eq!(arena.discard_checkpoint(epoch), Err(Error::Poisoned));
            assert_eq!(writer.join().unwrap(), Ok(()));
            assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
            assert_eq!(arena.load_word(0), Err(Error::Poisoned));
            assert_eq!(arena.store_word(0, 999), Err(Error::Poisoned));
            assert_eq!(arena.mapping.load(0), Ok(123));
            for mode in [
                arena::CaptureMode::Stopped,
                arena::CaptureMode::WriteProtected,
            ] {
                assert_eq!(arena.checkpoint(mode), Err(Error::Poisoned));
            }
            assert_eq!(Arc::strong_count(&arena.mapping), 1);
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn cancellation_returns_only_after_close_and_join() {
    run("cancellation_returns_only_after_close_and_join", || {
        let resources = Resources::current();
        let (mut arena, _) = scheduled_arena(capture::Hooks::new(|_, _| Ok(None)));
        let (before_close, closing) = mpsc::channel();
        let (release_close, gate) = mpsc::channel();
        let paused = before_close;
        let (send_flag, receive_flag) = mpsc::channel::<Arc<AtomicBool>>();
        let (start, started) = mpsc::channel();
        arena.capture_hooks = Some(capture::Hooks::new(move |event, _| {
            if event == capture::Event::BeforeDrain {
                start.send(()).unwrap();
                let cancel = receive_flag.recv().unwrap();
                while !cancel.load(Ordering::Acquire) {
                    thread::yield_now();
                }
            }
            if event == capture::Event::BeforeClose {
                paused.send(()).unwrap();
                gate.recv().unwrap();
            }
            Ok(None)
        }));
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        started.recv().unwrap();
        send_flag
            .send(arena.capture.as_ref().unwrap().cancel_flag())
            .unwrap();
        let (returned, done) = mpsc::channel();
        let owner = thread::spawn(move || {
            arena.discard_checkpoint(epoch).unwrap();
            returned.send(()).unwrap();
            arena.store_word(0, 61).unwrap();
            arena
        });
        closing.recv().unwrap();
        assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release_close.send(()).unwrap();
        let arena = owner.join().unwrap();
        done.recv().unwrap();
        assert_eq!(arena.load_word(0), Ok(61));
        assert_eq!(Arc::strong_count(&arena.mapping), 1);
        resources.assert_released();
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn owner_unwind_drops_pending_capture_before_unmapping() {
    run(
        "owner_unwind_drops_pending_capture_before_unmapping",
        || {
            let resources = Resources::current();
            let (hooks, reached, flag) = cancellation_hooks();
            let (mut arena, _) = scheduled_arena(hooks);
            let weak = Arc::downgrade(&arena.mapping);
            let owner = thread::spawn(move || {
                arena
                    .checkpoint(arena::CaptureMode::WriteProtected)
                    .unwrap();
                reached.recv().unwrap();
                flag.send(arena.capture.as_ref().unwrap().cancel_flag())
                    .unwrap();
                panic!("owner unwind with pending capture");
            });
            assert!(owner.join().is_err());
            assert!(
                weak.upgrade().is_none(),
                "worker/context must not retain mapping after owner drop"
            );
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn failed_load_checks_health_after_payload_access() {
    run("failed_load_checks_health_after_payload_access", || {
        let resources = Resources::current();
        let (ready, started) = mpsc::channel();
        let (accessed, after_access) = mpsc::channel();
        let hooks = capture::Hooks::new(move |event, context| {
            if event == capture::Event::BeforeDrain {
                ready.send(context.unwrap().failure()).unwrap();
                after_access.recv().unwrap();
                return Err(Error::Protocol("failure during owner load"));
            }
            Ok(None)
        });
        let (mut arena, _) = scheduled_arena(hooks);
        let epoch = arena
            .checkpoint(arena::CaptureMode::WriteProtected)
            .unwrap();
        let failure = started.recv().unwrap();
        arena.access_hook = Some(Box::new(move |event| {
            if event == arena::Access::Loaded {
                accessed.send(()).unwrap();
                while !failure.load(Ordering::Acquire) {
                    thread::yield_now();
                }
            }
        }));
        assert_eq!(arena.load_word(0), Err(Error::Poisoned));
        assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
        assert_poisoned(&mut arena, epoch);
        resources.assert_released();
    });
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn final_descriptor_close_failure_aborts() {
    isolated_exit(
        "lifecycle::final_descriptor_close_failure_aborts",
        || {
            let map = Arc::new(mapping::Mapping::new(1).unwrap());
            let mut context = linux::Registered::new(map).unwrap();
            context.protect().unwrap();
            context.inject_errors(linux::Call::Close, &[libc::EIO]);
            drop(context);
        },
        Some(libc::SIGABRT),
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn escaped_worker_panic_aborts_instead_of_detaching() {
    isolated_exit(
        "lifecycle::escaped_worker_panic_aborts_instead_of_detaching",
        || {
            let (paused, reached) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let mut hooks = capture::Hooks::new(move |event, _| {
                if event == capture::Event::BeforePublish {
                    paused.send(()).unwrap();
                    gate.recv().unwrap();
                }
                Ok(None)
            });
            hooks.escape_panic = true;
            let (mut arena, _) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            reached.recv().unwrap();
            release.send(()).unwrap();
            let _ = arena.wait_checkpoint(epoch);
        },
        Some(libc::SIGABRT),
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn malformed_and_out_of_range_events_poison_with_a_real_queued_fault() {
    run(
        "malformed_and_out_of_range_events_poison_with_a_real_queued_fault",
        || {
            let resources = Resources::current();
            // Event/flags/address/length validation is exercised through read_fault,
            // while a real kernel WP fault remains queued for error cleanup.
            for bad in 0..9 {
                let (mut arena, _) = scheduled_arena(capture::Hooks::new(|_, _| Ok(None)));
                let start = arena.mapping.address() as u64;
                let end = start + arena.mapping.len_bytes() as u64;
                let mut message = [0u8; 32];
                message[0] = if bad == 0 { 0x16 } else { 0x12 };
                let flags: u64 = match bad {
                    1 => 1,
                    2 => 2,
                    3 => 7,
                    _ => 3,
                };
                message[8..16].copy_from_slice(&flags.to_ne_bytes());
                let address = match bad {
                    4 => start - 1,
                    5 => end,
                    6 => u64::MAX,
                    _ => start,
                };
                message[16..24].copy_from_slice(&address.to_ne_bytes());
                let count = match bad {
                    7 => 0,
                    8 => 31,
                    _ => 32,
                };
                let mut first = true;
                arena.capture_hooks = Some(capture::Hooks::new(move |event, context| {
                    if event == capture::Event::BeforeDrain && first {
                        first = false;
                        let context = context.unwrap();
                        context.wait_readable(Duration::from_secs(2))?;
                        context.inject_message(message, count);
                    }
                    Ok(None)
                }));
                let epoch = arena
                    .checkpoint(arena::CaptureMode::WriteProtected)
                    .unwrap();
                assert_eq!(
                    arena.store_word(0, 123),
                    Err(Error::Poisoned),
                    "bad event {bad}"
                );
                assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
                assert_poisoned(&mut arena, epoch);
                resources.assert_released();
            }
        },
    );
}

fn reject_old(arena: &mut arena::Arena, epoch: arena::Epoch) {
    let before = arena.load_word(0).unwrap();
    assert_eq!(arena.checkpoint_status(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.wait_checkpoint(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.restore(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.discard_checkpoint(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.load_word(0), Ok(before));
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn thousand_wp_epochs_isolate_cancelled_faults_tokens_and_images() {
    run(
        "thousand_wp_epochs_isolate_cancelled_faults_tokens_and_images",
        || {
            let resources = Resources::current();
            let page = mapping::page_size().unwrap() / 8;
            let mut arena = arena::Arena::new(2 * page).unwrap();
            let mut other = arena::Arena::new(1).unwrap();
            let wrong = other.checkpoint(arena::CaptureMode::Stopped).unwrap();
            let image = arena.image.as_ptr();
            let pages = arena.page_saved.as_ptr();
            let address = arena.mapping.address();
            let mut old = Vec::new();
            for cycle in 0..1000u64 {
                let expected: Vec<_> = (0..arena.len_words())
                    .map(|slot| {
                        if cycle % 4 == 0 && slot >= page {
                            0
                        } else {
                            (cycle + 1) * 100_003 + slot as u64
                        }
                    })
                    .collect();
                for (slot, &word) in expected.iter().enumerate() {
                    arena.store_word(slot, word).unwrap();
                }
                let mut first = true;
                let cancel_pending = cycle % 2 == 1;
                let (queued, received) = mpsc::channel();
                let (flag, receiver) = mpsc::channel::<Arc<AtomicBool>>();
                arena.capture_hooks = Some(capture::Hooks::new(move |event, context| {
                    if event == capture::Event::BeforeDrain && first {
                        first = false;
                        context.unwrap().wait_readable(Duration::from_secs(2))?;
                        if cancel_pending {
                            queued.send(()).unwrap();
                            let cancel = receiver.recv().unwrap();
                            while !cancel.load(Ordering::Acquire) {
                                thread::yield_now();
                            }
                        }
                    }
                    Ok(None)
                }));
                let epoch = arena
                    .checkpoint(arena::CaptureMode::WriteProtected)
                    .unwrap();
                reject_old(&mut arena, wrong);
                if let Some(&previous) = old.last() {
                    reject_old(&mut arena, previous);
                }
                if let Some(&first_epoch) = old.first() {
                    reject_old(&mut arena, first_epoch);
                }
                if cancel_pending {
                    let live = Arc::clone(&arena.mapping);
                    // Only this fixture thread mutates while the owner cancels; the
                    // public owner API still has a single application mutation owner.
                    let writer = thread::spawn(move || live.store(2 * page - 1, cycle));
                    received.recv().unwrap();
                    flag.send(arena.capture.as_ref().unwrap().cancel_flag())
                        .unwrap();
                    arena.discard_checkpoint(epoch).unwrap();
                    assert_eq!(writer.join().unwrap(), Ok(()));
                    assert_eq!(arena.load_word(2 * page - 1), Ok(cycle));
                } else {
                    arena.store_word(0, u64::MAX).unwrap();
                    arena.wait_checkpoint(epoch).unwrap();
                    assert_eq!(
                        arena.checkpoint_words(epoch).unwrap(),
                        expected,
                        "cycle {cycle}"
                    );
                    assert_eq!(ready_metrics(&mut arena, epoch).fault_pages, 1);
                    for _ in 0..2 {
                        arena.restore(epoch).unwrap();
                        for (slot, &word) in expected.iter().enumerate() {
                            assert_eq!(arena.load_word(slot), Ok(word));
                        }
                        arena.store_word(0, u64::MAX).unwrap();
                    }
                    arena.discard_checkpoint(epoch).unwrap();
                }
                old.push(epoch);
                assert_eq!(arena.image.as_ptr(), image);
                assert_eq!(arena.page_saved.as_ptr(), pages);
                assert_eq!(arena.mapping.address(), address);
                assert_eq!(Arc::strong_count(&arena.mapping), 1);
                resources.assert_released();
            }
            for epoch in old {
                reject_old(&mut arena, epoch);
            }
            // The final cancelled context had an actual queued fault. The next image
            // must include its completed write and use a fresh context.
            let expected: Vec<_> = (0..arena.len_words())
                .map(|slot| arena.load_word(slot).unwrap())
                .collect();
            let last = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            arena.wait_checkpoint(last).unwrap();
            assert_eq!(arena.checkpoint_words(last).unwrap(), expected);
            arena.discard_checkpoint(last).unwrap();
            drop(arena);
            drop(other);
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn worker_panic_with_blocked_owner_abandons_no_resources() {
    run(
        "worker_panic_with_blocked_owner_abandons_no_resources",
        || {
            let resources = Resources::current();
            let mut first = true;
            let hooks = capture::Hooks::new(move |event, context| {
                if event == capture::Event::BeforeDrain && first {
                    first = false;
                    context.unwrap().wait_readable(Duration::from_secs(2))?;
                }
                if event == capture::Event::Fault(0) {
                    panic!("panic with blocked owner");
                }
                Ok(None)
            });
            let (mut arena, _) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            assert_eq!(arena.store_word(0, 456), Err(Error::Poisoned));
            assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
            assert_poisoned(&mut arena, epoch);
            drop(arena);
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn permanent_read_error_poisons_and_releases_queued_owner() {
    run(
        "permanent_read_error_poisons_and_releases_queued_owner",
        || {
            let resources = Resources::current();
            let mut first = true;
            let hooks = capture::Hooks::new(move |event, context| {
                if event == capture::Event::BeforeDrain && first {
                    first = false;
                    context.unwrap().wait_readable(Duration::from_secs(2))?;
                    context
                        .unwrap()
                        .inject_errors(linux::Call::Read, &[libc::EIO]);
                }
                Ok(None)
            });
            let (mut arena, _) = scheduled_arena(hooks);
            let epoch = arena
                .checkpoint(arena::CaptureMode::WriteProtected)
                .unwrap();
            assert_eq!(arena.store_word(0, 456), Err(Error::Poisoned));
            assert_eq!(arena.wait_checkpoint(epoch), Err(Error::Poisoned));
            assert_poisoned(&mut arena, epoch);
            resources.assert_released();
        },
    );
}

#[test]
#[ignore = "requires real userfaultfd WP; run explicitly"]
fn out_of_range_read_reports_protocol_error_before_page_indexing() {
    run(
        "out_of_range_read_reports_protocol_error_before_page_indexing",
        || {
            let resources = Resources::current();
            let (map, context, _) = prepared();
            let (owner, done) = writer(Arc::clone(&map), context.failure());
            context.wait_readable(Duration::from_secs(2)).unwrap();
            assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
            let start = map.address() as u64;
            for address in [start - 1, start + map.len_bytes() as u64, u64::MAX] {
                let mut bytes = [0u8; 32];
                bytes[0] = 0x12;
                bytes[8] = 3;
                bytes[16..24].copy_from_slice(&address.to_ne_bytes());
                context.inject_message(bytes, 32);
                assert_eq!(
                    context.read_fault(),
                    Err(Error::Protocol("out-of-range fault address"))
                );
            }
            drop(context);
            assert_eq!(owner.join().unwrap(), Err(Error::Poisoned));
            assert_eq!(Arc::strong_count(&map), 1);
            resources.assert_released();
        },
    );
}

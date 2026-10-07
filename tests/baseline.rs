use epochsnap::{Arena, CaptureMode, CheckpointStatus, Epoch, Error};

fn page_words() -> usize {
    // SAFETY: sysconf takes no pointer and only queries the host's base page size.
    let bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(bytes > 0 && bytes % 8 == 0);
    bytes as usize / 8
}

#[test]
fn rejects_zero_overflow_and_sizes_above_pointer_limit() {
    for words in [
        0,
        usize::MAX,
        usize::MAX / 8 + 1,
        isize::MAX as usize / 8 + 1,
        isize::MAX as usize / 8,
    ] {
        assert!(matches!(Arena::new(words), Err(Error::InvalidSize)));
    }
}

#[test]
fn page_rounded_capacity_is_fully_initialized() {
    let page = page_words();
    for (requested, capacity) in [(1, page), (page, page), (page + 1, 2 * page)] {
        let arena = Arena::new(requested).unwrap();
        assert_eq!(arena.len_words(), capacity);
        for slot in 0..capacity {
            assert_eq!(arena.load_word(slot), Ok(0), "slot {slot}");
        }
    }
}

#[test]
fn last_slot_is_writable_and_invalid_offsets_do_not_mutate() {
    let mut arena = Arena::new(page_words() + 1).unwrap();
    let last = arena.len_words() - 1;
    arena.store_word(0, 0x0123_4567_89ab_cdef).unwrap();
    arena.store_word(last, u64::MAX).unwrap();
    for slot in [arena.len_words(), usize::MAX] {
        assert_eq!(arena.load_word(slot), Err(Error::Bounds));
        assert_eq!(arena.store_word(slot, 99), Err(Error::Bounds));
    }
    assert_eq!(arena.load_word(0), Ok(0x0123_4567_89ab_cdef));
    assert_eq!(arena.load_word(last), Ok(u64::MAX));
}

fn nonuniform_words(len: usize) -> Vec<u64> {
    (0..len)
        .map(|slot| {
            (slot as u64)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .rotate_left(17)
                ^ 0xa5a5_1234_ffff_0000
        })
        .collect()
}

fn store_all(arena: &mut Arena, words: &[u64]) {
    assert_eq!(arena.len_words(), words.len());
    for (slot, &word) in words.iter().enumerate() {
        arena.store_word(slot, word).unwrap();
    }
}

fn live_words(arena: &Arena) -> Vec<u64> {
    (0..arena.len_words())
        .map(|slot| arena.load_word(slot).unwrap())
        .collect()
}

#[test]
fn stopped_image_is_exact() {
    let mut arena = Arena::new(3 * page_words() + 7).unwrap();
    let expected = nonuniform_words(arena.len_words());
    store_all(&mut arena, &expected);
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    assert!(matches!(
        arena.checkpoint_status(epoch),
        Ok(CheckpointStatus::Ready { .. })
    ));
    assert_eq!(arena.wait_checkpoint(epoch), Ok(()));
    assert_eq!(arena.checkpoint_words(epoch).unwrap(), expected);
    assert_eq!(live_words(&arena), expected);
    store_all(&mut arena, &vec![u64::MAX; expected.len()]);
    assert_eq!(arena.checkpoint_words(epoch).unwrap(), expected);
}

#[test]
fn restore_is_exact() {
    let mut arena = Arena::new(2 * page_words() + 1).unwrap();
    let expected = nonuniform_words(arena.len_words());
    store_all(&mut arena, &expected);
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    for mutation in [0, u64::MAX, 0x1234_5678_9abc_def0] {
        store_all(&mut arena, &vec![mutation; expected.len()]);
        arena.restore(epoch).unwrap();
        assert_eq!(live_words(&arena), expected);
        assert_eq!(arena.checkpoint_words(epoch).unwrap(), expected);
        assert!(matches!(
            arena.checkpoint_status(epoch),
            Ok(CheckpointStatus::Ready { .. })
        ));
    }
}

#[test]
fn second_capture_is_busy_and_preserves_the_retained_image() {
    let mut arena = Arena::new(1).unwrap();
    arena.store_word(0, 11).unwrap();
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    arena.store_word(0, 22).unwrap();
    for mode in [CaptureMode::Stopped, CaptureMode::WriteProtected] {
        assert_eq!(arena.checkpoint(mode), Err(Error::Busy));
    }
    assert_eq!(arena.checkpoint_words(epoch).unwrap()[0], 11);
    assert_eq!(arena.load_word(0), Ok(22));
    arena.restore(epoch).unwrap();
    assert_eq!(arena.load_word(0), Ok(11));
}

fn assert_invalid_epoch_has_no_effect(arena: &mut Arena, epoch: Epoch) {
    let before = live_words(arena);
    assert_eq!(arena.checkpoint_status(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.wait_checkpoint(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.checkpoint_words(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.restore(epoch), Err(Error::InvalidEpoch));
    assert_eq!(arena.discard_checkpoint(epoch), Err(Error::InvalidEpoch));
    assert_eq!(live_words(arena), before);
}

#[test]
fn wrong_arena_tokens_cannot_read_restore_or_discard() {
    let mut first = Arena::new(1).unwrap();
    let mut second = Arena::new(1).unwrap();
    first.store_word(0, 111).unwrap();
    second.store_word(0, 222).unwrap();
    let a = first.checkpoint(CaptureMode::Stopped).unwrap();
    let b = second.checkpoint(CaptureMode::Stopped).unwrap();
    assert_ne!(a, b);
    first.store_word(0, 333).unwrap();
    second.store_word(0, 444).unwrap();
    assert_invalid_epoch_has_no_effect(&mut first, b);
    assert_invalid_epoch_has_no_effect(&mut second, a);
    assert_eq!(first.checkpoint_words(a).unwrap()[0], 111);
    assert_eq!(second.checkpoint_words(b).unwrap()[0], 222);
    first.restore(a).unwrap();
    second.restore(b).unwrap();
    assert_eq!(first.load_word(0), Ok(111));
    assert_eq!(second.load_word(0), Ok(222));
}

#[test]
fn discard_invalidates_tokens_and_reuse_overwrites_every_slot() {
    let mut arena = Arena::new(3 * page_words() + 1).unwrap();
    let first_image = nonuniform_words(arena.len_words());
    store_all(&mut arena, &first_image);
    let old = arena.checkpoint(CaptureMode::Stopped).unwrap();
    arena.discard_checkpoint(old).unwrap();
    assert_eq!(live_words(&arena), first_image);
    assert_invalid_epoch_has_no_effect(&mut arena, old);

    // Zero pages and rounded padding must replace the old, nonzero buffer too.
    let mut expected = vec![0; arena.len_words()];
    expected[page_words() - 1] = 7;
    expected[page_words()] = 8;
    *expected.last_mut().unwrap() = 9;
    store_all(&mut arena, &expected);
    let new = arena.checkpoint(CaptureMode::Stopped).unwrap();
    assert_ne!(old, new);
    assert_invalid_epoch_has_no_effect(&mut arena, old);
    assert_eq!(arena.checkpoint_words(new).unwrap(), expected);
    store_all(&mut arena, &first_image);
    arena.restore(new).unwrap();
    assert_eq!(live_words(&arena), expected);
}

#[test]
fn dropped_arena_token_is_invalid_for_a_new_arena() {
    let token = {
        let mut arena = Arena::new(1).unwrap();
        arena.checkpoint(CaptureMode::Stopped).unwrap()
    };
    let mut arena = Arena::new(1).unwrap();
    assert_invalid_epoch_has_no_effect(&mut arena, token);
    let current = arena.checkpoint(CaptureMode::Stopped).unwrap();
    assert_ne!(current, token);
    assert_invalid_epoch_has_no_effect(&mut arena, token);
    assert!(matches!(
        arena.checkpoint_status(current),
        Ok(CheckpointStatus::Ready { .. })
    ));
}

#[test]
fn stopped_metrics_cover_the_rounded_image_and_owner_boundary() {
    let mut arena = Arena::new(page_words() + 1).unwrap();
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    let CheckpointStatus::Ready { metrics } = arena.checkpoint_status(epoch).unwrap() else {
        panic!("stopped capture must be ready");
    };
    assert_eq!(metrics.fault_pages, 0);
    assert_eq!(metrics.scan_pages, 2);
    assert_eq!(metrics.copied_bytes, arena.len_words() * 8);
    assert!(metrics.boundary_offset <= metrics.ready_offset);
    assert_eq!(metrics.worker_elapsed, None);
    assert_eq!(metrics.worker_cpu, None);
}

#[test]
fn ownership_and_retained_epoch_can_move_between_threads() {
    let mut arena = Arena::new(1).unwrap();
    arena.store_word(0, 123).unwrap();
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    let arena = std::thread::spawn(move || {
        arena.store_word(0, 456).unwrap();
        arena.restore(epoch).unwrap();
        arena
    })
    .join()
    .unwrap();
    assert_eq!(arena.load_word(0), Ok(123));
    assert_eq!(arena.checkpoint_words(epoch).unwrap()[0], 123);
}

// A complete logical application operation. Step, RNG, accumulator, and records
// all live in slots; no outside mutable state participates in replay.
fn application_step(arena: &mut Arena, input: u64) {
    let step = arena.load_word(0).unwrap();
    let mut rng = arena.load_word(1).unwrap();
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    let record = 3 + rng as usize % (arena.len_words() - 3);
    let old = arena.load_word(record).unwrap();
    let updated = old.wrapping_add(input ^ rng).rotate_left(11);
    let accumulator = arena.load_word(2).unwrap().wrapping_add(updated);
    arena.store_word(record, updated).unwrap();
    arena.store_word(2, accumulator).unwrap();
    arena.store_word(1, rng).unwrap();
    arena.store_word(0, step + 1).unwrap();
}

#[test]
fn restore_replays_identical_inputs_to_identical_full_state() {
    let mut arena = Arena::new(3 * page_words() + 1).unwrap();
    let initial = nonuniform_words(arena.len_words());
    store_all(&mut arena, &initial);
    arena.store_word(0, 0).unwrap();
    arena.store_word(1, 0x1234_5678_9abc_def0).unwrap();
    for input in [1, 5, 9, 13, 17] {
        application_step(&mut arena, input);
    }
    let at_boundary = live_words(&arena);
    let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    let inputs = [0, 1, u64::MAX, 42, 0xdead_beef, 123];
    for input in inputs.into_iter().cycle().take(128) {
        application_step(&mut arena, input);
    }
    let first_run = live_words(&arena);
    assert_eq!(first_run[0], 133);
    assert_ne!(first_run[1], at_boundary[1]);
    assert_ne!(first_run, at_boundary);

    arena.restore(epoch).unwrap();
    assert_eq!(live_words(&arena), at_boundary);
    for input in inputs.into_iter().cycle().take(128) {
        application_step(&mut arena, input);
    }
    assert_eq!(live_words(&arena), first_run);
    assert_eq!(arena.checkpoint_words(epoch).unwrap(), at_boundary);
}

#[test]
fn thousand_stopped_epochs_reject_old_tokens_and_replace_every_slot() {
    let mut arena = Arena::new(1).unwrap();
    let mut old = Vec::new();
    for cycle in 0..1000u64 {
        let expected: Vec<_> = (0..arena.len_words())
            .map(|slot| {
                if cycle % 2 == 0 {
                    0
                } else {
                    cycle * 100_003 + slot as u64
                }
            })
            .collect();
        store_all(&mut arena, &expected);
        let current = arena.checkpoint(CaptureMode::Stopped).unwrap();
        if let Some(&previous) = old.last() {
            assert_invalid_epoch_has_no_effect(&mut arena, previous);
        }
        if let Some(&first) = old.first() {
            assert_invalid_epoch_has_no_effect(&mut arena, first);
        }
        assert_eq!(arena.checkpoint_words(current).unwrap(), expected);
        arena.store_word(0, u64::MAX).unwrap();
        arena.restore(current).unwrap();
        assert_eq!(live_words(&arena), expected);
        arena.discard_checkpoint(current).unwrap();
        old.push(current);
    }
    for epoch in old {
        assert_invalid_epoch_has_no_effect(&mut arena, epoch);
    }
}

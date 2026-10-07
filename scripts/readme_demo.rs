//! Deterministic public-API demo used by render_demo.py; no UFFD required.
use epochsnap::{Arena, CaptureMode};

fn visible_words(arena: &Arena) -> epochsnap::Result<Vec<u64>> {
    (0..4).map(|slot| arena.load_word(slot)).collect()
}

fn main() -> epochsnap::Result<()> {
    let mut arena = Arena::new(4)?;
    for (slot, value) in [10, 20, 30, 40].into_iter().enumerate() {
        arena.store_word(slot, value)?;
    }
    println!("mode: Stopped | same-process arena rewind");
    println!("arena: {} slots; showing slots 0..4", arena.len_words());
    println!("initial     {:?}", visible_words(&arena)?);

    let epoch = arena.checkpoint(CaptureMode::Stopped)?;
    let saved = arena.checkpoint_words(epoch)?.to_vec();
    assert_eq!(&saved[..4], &[10, 20, 30, 40]);
    println!("checkpoint  saved {:?}", &saved[..4]);

    for (slot, value) in [90, 80, 70, 60].into_iter().enumerate() {
        arena.store_word(slot, value)?;
    }
    assert_eq!(visible_words(&arena)?, [90, 80, 70, 60]);
    assert_eq!(arena.checkpoint_words(epoch)?, saved);
    println!("mutated     {:?}", visible_words(&arena)?);

    arena.restore(epoch)?;
    let restored: Vec<u64> = (0..arena.len_words())
        .map(|slot| arena.load_word(slot))
        .collect::<epochsnap::Result<_>>()?;
    assert_eq!(restored, saved);
    println!("restored    {:?}", visible_words(&arena)?);
    arena.discard_checkpoint(epoch)?;
    println!(
        "verified    all {} slots match the checkpoint",
        restored.len()
    );
    Ok(())
}

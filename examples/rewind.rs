//! Fixed-record replay with all mutable simulation state inside the arena.
mod support;

use epochsnap::{Arena, CaptureMode, CheckpointStatus};
use support::{Result, arena_words, hash, mix, page_size};

const HEADER_WORDS: usize = 3; // step, RNG state, accumulator
const RECORD_WORDS: usize = 4; // value, visit count, last input, last step

fn initialize(arena: &mut Arena) -> Result<()> {
    for slot in 0..arena.len_words() {
        arena.store_word(slot, mix(slot as u64))?;
    }
    arena.store_word(0, 0)?;
    arena.store_word(1, 0x1234_5678_9abc_def0)?;
    arena.store_word(2, 0)?;
    Ok(())
}

fn step(arena: &mut Arena, input: u64) -> Result<()> {
    let step = arena.load_word(0)?;
    let mut rng = arena.load_word(1)?;
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    let records = (arena.len_words() - HEADER_WORDS) / RECORD_WORDS;
    let record = HEADER_WORDS + (rng as usize % records) * RECORD_WORDS;
    let value = arena
        .load_word(record)?
        .wrapping_add(input ^ rng)
        .rotate_left(11);
    let visits = arena.load_word(record + 1)?.wrapping_add(1);
    let accumulator = arena.load_word(2)?.wrapping_add(value);
    arena.store_word(record, value)?;
    arena.store_word(record + 1, visits)?;
    arena.store_word(record + 2, input)?;
    arena.store_word(record + 3, step)?;
    arena.store_word(2, accumulator)?;
    arena.store_word(1, rng)?;
    arena.store_word(0, step + 1)?;
    Ok(())
}

fn input(step: u64) -> u64 {
    mix(step ^ 0xa5a5_5a5a_1122_3344)
}

fn compare(arena: &Arena, expected: &[u64]) -> Result<()> {
    if arena.len_words() != expected.len() {
        return Err("state length mismatch".into());
    }
    for (slot, &value) in expected.iter().enumerate() {
        if arena.load_word(slot)? != value {
            return Err(format!("full-state mismatch at slot {slot}").into());
        }
    }
    Ok(())
}

fn run() -> Result<()> {
    let mut mode = CaptureMode::WriteProtected;
    let mut mib = 16;
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        if key == "--help" {
            println!(
                "rewind [--mode wp|stopped] [--arena-mib N]; default WP, 16 MiB. No fallback."
            );
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {key}"))?;
        match key.as_str() {
            "--mode" => {
                mode = match value.as_str() {
                    "wp" => CaptureMode::WriteProtected,
                    "stopped" => CaptureMode::Stopped,
                    _ => return Err(format!("unknown mode {value}").into()),
                }
            }
            "--arena-mib" => mib = value.parse()?,
            _ => return Err(format!("unknown option {key}").into()),
        }
    }
    let mut arena = Arena::new(arena_words(mib)?)?;
    initialize(&mut arena)?;
    println!(
        "same-process, in-memory rewind; mode={mode:?}, arena={} bytes, base pages={} bytes",
        arena.len_words() * 8,
        page_size()?
    );
    println!(
        "logical step, RNG, accumulator and fixed records live in slots; rendering and external effects do not rewind"
    );
    for logical in 0..128 {
        step(&mut arena, input(logical))?;
    }
    let epoch = arena.checkpoint(mode)?;
    println!("checkpoint step=128; owner resumed after the capture boundary");
    for logical in 128..1152 {
        step(&mut arena, input(logical))?;
        if (logical + 1) % 128 == 0 {
            match arena.checkpoint_status(epoch)? {
                CheckpointStatus::Pending {
                    saved_pages,
                    total_pages,
                } => println!(
                    "capture pending: {saved_pages}/{total_pages} pages; live step={}",
                    arena.load_word(0)?
                ),
                CheckpointStatus::Ready { .. } => {
                    println!("capture ready; live step={}", arena.load_word(0)?)
                }
            }
        }
    }
    arena.wait_checkpoint(epoch)?;
    println!(
        "capture ready after final close and worker join; checkpoint step={}",
        arena.checkpoint_words(epoch)?[0]
    );
    for logical in 1152..1408 {
        step(&mut arena, input(logical))?;
    }
    let expected: Vec<_> = (0..arena.len_words())
        .map(|slot| arena.load_word(slot))
        .collect::<epochsnap::Result<_>>()?;
    let first_hash = hash(expected.iter().copied());
    println!("advanced to step=1408, state hash={first_hash:016x}");
    arena.restore(epoch)?;
    compare(&arena, arena.checkpoint_words(epoch)?)?;
    if arena.load_word(0)? != 128 {
        return Err("restore lost logical step".into());
    }
    println!("restored step=128; replaying identical inputs");
    for logical in 128..1408 {
        step(&mut arena, input(logical))?;
    }
    compare(&arena, &expected)?;
    let replay_hash =
        hash((0..arena.len_words()).map(|slot| arena.load_word(slot).expect("checked live state")));
    println!(
        "full-state equality: true ({} slots); replay hash={replay_hash:016x}",
        expected.len()
    );
    arena.discard_checkpoint(epoch)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rewind: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_rejects_a_single_changed_slot_even_outside_records() {
        let mut arena = Arena::new(1).unwrap();
        initialize(&mut arena).unwrap();
        let expected: Vec<_> = (0..arena.len_words())
            .map(|slot| arena.load_word(slot).unwrap())
            .collect();
        arena.store_word(arena.len_words() - 1, 123).unwrap();
        assert!(compare(&arena, &expected).is_err());
    }
}

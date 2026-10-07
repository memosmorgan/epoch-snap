use crate::{
    Error, Result,
    capture::{Storage, Worker},
    mapping::Mapping,
};
use std::{
    cell::Cell,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Loaded,
}

/// Select stopped copying or synchronous userfaultfd write-protected capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Stopped,
    WriteProtected,
}

/// An opaque identity/generation token for one arena's retained checkpoint.
///
/// Tokens may be copied, but cannot be constructed by callers. Discard makes
/// every copy stale; dropping the arena cannot make it valid for another arena.
///
/// ```compile_fail
/// use epochsnap::Epoch;
/// let forged = Epoch { arena_id: 1, generation: 1 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch {
    arena_id: u64,
    generation: u64,
}

/// Per-capture origin counts and monotonic offsets from checkpoint-call entry.
///
/// `boundary_offset` ends at T after full arming (or at stopped copying start).
/// `ready_offset` includes final context close, owner observation and worker join.
/// WP worker elapsed runs from T through context close, before owner join;
/// worker CPU includes setup, copying and cleanup. WP-only fields are `None`
/// for stopped capture, whose pages are counted in `scan_pages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureMetrics {
    pub fault_pages: usize,
    pub scan_pages: usize,
    pub copied_bytes: usize,
    pub boundary_offset: Duration,
    pub ready_offset: Duration,
    pub worker_elapsed: Option<Duration>,
    pub worker_cpu: Option<Duration>,
}

/// Progress never implies readiness: ready requires descriptor closure and join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointStatus {
    Pending {
        saved_pages: usize,
        total_pages: usize,
    },
    Ready {
        metrics: CaptureMetrics,
    },
}

/// A fixed, page-rounded arena of atomic `u64` slots with one application owner.
///
/// Only copied values leave live storage. Mutation requires exclusive ownership.
///
/// ```
/// use epochsnap::{Arena, CaptureMode, CheckpointStatus};
/// # fn main() -> epochsnap::Result<()> {
/// let mut arena = Arena::new(1)?;
/// arena.store_word(0, 42)?;
/// let epoch = arena.checkpoint(CaptureMode::Stopped)?;
/// assert!(matches!(arena.checkpoint_status(epoch)?, CheckpointStatus::Ready { .. }));
/// arena.store_word(0, 99)?;
/// assert_eq!(arena.checkpoint_words(epoch)?[0], 42);
/// arena.restore(epoch)?;
/// assert_eq!(arena.load_word(0)?, 42);
/// arena.discard_checkpoint(epoch)?;
/// # Ok(())
/// # }
/// ```
///
/// Shared ownership cannot cross threads through the safe API:
///
/// ```compile_fail
/// use epochsnap::Arena;
/// fn require_sync<T: Sync>() {}
/// require_sync::<Arena>();
/// ```
///
/// The arena cannot be cloned:
///
/// ```compile_fail
/// use epochsnap::Arena;
/// let arena = Arena::new(1).unwrap();
/// let duplicate = arena.clone();
/// ```
pub struct Arena {
    pub(crate) mapping: Arc<Mapping>,
    pub(crate) image: Vec<u64>,
    pub(crate) page_saved: Vec<bool>,
    pub(crate) capture: Option<Worker>,
    metrics: Option<CaptureMetrics>,
    poisoned: bool,
    #[cfg(test)]
    pub(crate) capture_hooks: Option<crate::capture::Hooks>,
    #[cfg(test)]
    pub(crate) access_hook: Option<Box<dyn Fn(Access) + Send>>,
    arena_id: u64,
    generation: u64,
    checkpoint: Option<Epoch>,
    // Arena can move to another thread, but cannot be shared concurrently.
    owner: PhantomData<Cell<()>>,
}

impl Arena {
    /// Allocate/populate the live mapping, one reusable image, and page state.
    /// All rounded payload slots and the image are initialized to zero.
    ///
    /// Rejects zero, overflowing sizes, and rounded byte sizes above `isize::MAX`.
    pub fn new(requested_words: usize) -> Result<Self> {
        let mapping = Arc::new(Mapping::new(requested_words)?);
        let image = zeroed_image(mapping.len_words())?;
        let total_pages = mapping.len_bytes() / mapping.page_size();
        let mut page_saved = Vec::new();
        page_saved
            .try_reserve_exact(total_pages)
            .map_err(|_| Error::Allocation("page bookkeeping"))?;
        page_saved.resize(total_pages, false);
        Ok(Self {
            mapping,
            image,
            page_saved,
            capture: None,
            metrics: None,
            poisoned: false,
            #[cfg(test)]
            capture_hooks: None,
            #[cfg(test)]
            access_hook: None,
            arena_id: claim_arena_id(&NEXT_ARENA_ID)?,
            generation: 0,
            checkpoint: None,
            owner: PhantomData,
        })
    }

    /// Return the page-rounded capacity, including initialized padding slots.
    pub fn len_words(&self) -> usize {
        self.mapping.len_words()
    }

    /// Load one checked slot as a copied value.
    pub fn load_word(&self, slot: usize) -> Result<u64> {
        self.check_health()?;
        let result = self.mapping.load(slot);
        #[cfg(test)]
        if let Some(hook) = &self.access_hook {
            hook(Access::Loaded);
        }
        self.check_health()?;
        result
    }

    /// Store one checked slot under exclusive owner access.
    pub fn store_word(&mut self, slot: usize, value: u64) -> Result<()> {
        self.check_health()?;
        let result = self.mapping.store(slot, value);
        self.check_health()?;
        result
    }

    /// Capture between complete logical operations under exclusive owner access.
    ///
    /// Stopped copying returns ready. WP returns after full arming establishes T;
    /// writes can then stall until their whole page is saved. Discard the retained
    /// checkpoint before another capture. Capability errors never fall back.
    pub fn checkpoint(&mut self, mode: CaptureMode) -> Result<Epoch> {
        let started = Instant::now();
        self.check_health()?;
        if self.checkpoint.is_some() {
            return Err(Error::Busy);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::EpochExhausted)?;
        self.page_saved.fill(false);
        match mode {
            CaptureMode::Stopped => {
                let boundary_offset = started.elapsed();
                let words_per_page = self.mapping.page_size() / 8;
                // T is here: the owner has exclusive arena access until copying ends.
                // Replace the entire old image, including page padding.
                for (page, target) in self.image.chunks_exact_mut(words_per_page).enumerate() {
                    self.mapping.copy_page(page, target)?;
                    self.page_saved[page] = true;
                }
                self.metrics = Some(CaptureMetrics {
                    fault_pages: 0,
                    scan_pages: self.page_saved.len(),
                    copied_bytes: self.mapping.len_bytes(),
                    boundary_offset,
                    ready_offset: started.elapsed(),
                    worker_elapsed: None,
                    worker_cpu: None,
                });
            }
            CaptureMode::WriteProtected => {
                let (worker, send, armed) = Worker::spawn(
                    Arc::clone(&self.mapping),
                    started,
                    #[cfg(test)]
                    self.capture_hooks.take(),
                )?;
                let storage = Storage {
                    image: std::mem::take(&mut self.image),
                    pages: std::mem::take(&mut self.page_saved),
                };
                if let Err(error) = send.send(storage) {
                    self.image = error.0.image;
                    self.page_saved = error.0.pages;
                    drop(send);
                    drop(worker); // confirms any startup worker has joined
                    return Err(Error::Protocol("capture storage handoff failed"));
                }
                drop(send);
                self.capture = Some(worker);
                if let Err(error) = armed
                    .recv()
                    .unwrap_or(Err(Error::Protocol("arming acknowledgement abandoned")))
                {
                    let _ = self.finalize_capture();
                    return Err(error);
                }
                if self.check_health().is_err() {
                    let _ = self.finalize_capture();
                    return Err(Error::Poisoned);
                }
            }
        }
        let epoch = Epoch {
            arena_id: self.arena_id,
            generation,
        };
        self.generation = generation;
        self.checkpoint = Some(epoch);
        Ok(epoch)
    }

    /// Poll progress. Finalize completed/failing workers before reporting ready.
    pub fn checkpoint_status(&mut self, epoch: Epoch) -> Result<CheckpointStatus> {
        self.validate_epoch(epoch)?;
        if self
            .capture
            .as_ref()
            .is_some_and(|worker| worker.is_finished() || worker.failed())
        {
            self.finalize_capture()?;
        }
        self.check_health()?;
        if let Some(worker) = &self.capture {
            return Ok(CheckpointStatus::Pending {
                saved_pages: worker.saved_pages(),
                total_pages: self.mapping.len_bytes() / self.mapping.page_size(),
            });
        }
        Ok(CheckpointStatus::Ready {
            metrics: self.metrics.ok_or(Error::Pending)?,
        })
    }

    /// Join the current worker and recover its image after final descriptor close.
    /// This may block; it does not promise a kernel/scheduler timeout.
    pub fn wait_checkpoint(&mut self, epoch: Epoch) -> Result<()> {
        self.validate_epoch(epoch)?;
        self.finalize_capture()?;
        self.check_health()
    }

    /// Borrow the immutable completed image in separate storage, including padding.
    /// Its borrow excludes mutation, restore, capture, and discard of this owner.
    ///
    /// ```compile_fail
    /// use epochsnap::{Arena, CaptureMode};
    /// let mut arena = Arena::new(1).unwrap();
    /// let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    /// let image = arena.checkpoint_words(epoch).unwrap();
    /// arena.restore(epoch).unwrap();
    /// assert_eq!(image[0], 0);
    /// ```
    ///
    /// ```compile_fail
    /// use epochsnap::{Arena, CaptureMode};
    /// let mut arena = Arena::new(1).unwrap();
    /// let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
    /// let image = arena.checkpoint_words(epoch).unwrap();
    /// arena.discard_checkpoint(epoch).unwrap();
    /// assert_eq!(image[0], 0);
    /// ```
    pub fn checkpoint_words(&self, epoch: Epoch) -> Result<&[u64]> {
        self.validate_epoch(epoch)?;
        self.check_health()?;
        if self.capture.is_some() || self.metrics.is_none() {
            return Err(Error::Pending);
        }
        Ok(&self.image)
    }

    /// Rewind every slot in the existing mapping with atomic stores.
    ///
    /// Reject an invalid epoch before any payload store. This does not discard the
    /// image; the ready checkpoint may be restored repeatedly. Stacks, external
    /// effects, and state outside this arena are not restored.
    pub fn restore(&mut self, epoch: Epoch) -> Result<()> {
        self.validate_epoch(epoch)?;
        self.check_health()?;
        if self.capture.is_some() || self.metrics.is_none() {
            return Err(Error::Pending);
        }
        for (slot, &value) in self.image.iter().enumerate() {
            self.mapping.store(slot, value)?;
        }
        Ok(())
    }

    /// Discard a ready image or cancel a pending epoch. Return to usable only
    /// after successful teardown and join; unexpected active failures poison.
    pub fn discard_checkpoint(&mut self, epoch: Epoch) -> Result<()> {
        self.validate_epoch(epoch)?;
        if let Some(worker) = &self.capture {
            worker.cancel();
        }
        let result = self.finalize_capture();
        self.checkpoint = None;
        self.metrics = None;
        result?;
        self.check_health()
    }

    fn check_health(&self) -> Result<()> {
        if self.poisoned || self.capture.as_ref().is_some_and(Worker::failed) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn finalize_capture(&mut self) -> Result<()> {
        if let Some(worker) = self.capture.take() {
            let finished = worker.finish();
            self.image = finished.storage.image;
            self.page_saved = finished.storage.pages;
            self.poisoned |= finished.poisoned;
            match finished.outcome {
                Ok(metrics) => self.metrics = metrics,
                Err(error) => {
                    self.metrics = None;
                    return if self.poisoned {
                        Err(Error::Poisoned)
                    } else {
                        Err(error)
                    };
                }
            }
        }
        self.check_health()
    }

    fn validate_epoch(&self, epoch: Epoch) -> Result<()> {
        if epoch.arena_id != self.arena_id || self.checkpoint != Some(epoch) {
            return Err(Error::InvalidEpoch);
        }
        Ok(())
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        if let Some(worker) = &self.capture {
            worker.cancel();
        }
        let _ = self.finalize_capture();
        // Worker has joined and closed its only descriptor before field drops
        // can release the mapping, including when the owner itself unwinds.
    }
}

fn zeroed_image(words: usize) -> Result<Vec<u64>> {
    let mut image = Vec::new();
    image
        .try_reserve_exact(words)
        .map_err(|_| Error::Allocation("checkpoint image"))?;
    image.resize(words, 0);
    Ok(image)
}

fn claim_arena_id(counter: &AtomicU64) -> Result<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| Error::EpochExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_capacity_failure_is_reported_without_panicking() {
        assert_eq!(
            zeroed_image(usize::MAX),
            Err(crate::Error::Allocation("checkpoint image"))
        );
    }

    #[test]
    fn arena_identity_never_wraps_or_reuses_an_id() {
        let counter = std::sync::atomic::AtomicU64::new(u64::MAX - 1);
        assert_eq!(claim_arena_id(&counter), Ok(u64::MAX - 1));
        for _ in 0..2 {
            assert_eq!(claim_arena_id(&counter), Err(crate::Error::EpochExhausted));
        }
    }

    #[test]
    fn generation_exhaustion_does_not_publish_an_image_or_wrap() {
        let mut arena = Arena::new(1).unwrap();
        arena.generation = u64::MAX - 1;
        let last = arena.checkpoint(CaptureMode::Stopped).unwrap();
        arena.discard_checkpoint(last).unwrap();
        arena.store_word(0, 37).unwrap();
        assert_eq!(
            arena.checkpoint(CaptureMode::Stopped),
            Err(crate::Error::EpochExhausted)
        );
        assert_eq!(arena.load_word(0), Ok(37));
        assert_eq!(arena.restore(last), Err(crate::Error::InvalidEpoch));
    }

    #[test]
    fn restore_preserves_mapping_and_reuses_prepared_storage() {
        let mut arena = Arena::new(1025).unwrap();
        let address = arena.mapping.address();
        let image = arena.image.as_ptr();
        let pages = arena.page_saved.as_ptr();
        assert_eq!(arena.image.len(), arena.len_words());
        assert!(arena.image.iter().all(|&word| word == 0));
        assert!(arena.page_saved.iter().all(|&saved| !saved));
        for value in [17, 29] {
            arena.store_word(arena.len_words() - 1, value).unwrap();
            let epoch = arena.checkpoint(CaptureMode::Stopped).unwrap();
            assert!(arena.page_saved.iter().all(|&saved| saved));
            arena.store_word(arena.len_words() - 1, 99).unwrap();
            arena.restore(epoch).unwrap();
            assert_eq!(arena.mapping.address(), address);
            assert_eq!(arena.load_word(arena.len_words() - 1), Ok(value));
            arena.discard_checkpoint(epoch).unwrap();
            assert_eq!(arena.image.as_ptr(), image);
            assert_eq!(arena.page_saved.as_ptr(), pages);
        }
    }
}

use crate::{Error, Result};
use std::{
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) struct Mapping {
    base: NonNull<u8>,
    bytes: usize,
    page: usize,
    #[cfg(test)]
    copy_error_at: std::sync::atomic::AtomicUsize,
}

// SAFETY: the mapping owns its allocation and may move between threads; its
// private methods tie atomic views to &self and expose only copied values.
// Arena and registered contexts/threaded accessors retain an Arc.
// Both ordinary borrows and Arc ownership prevent unmap during access. Callers
// perform no external mapping/pinning/alias operations.
unsafe impl Send for Mapping {}
// SAFETY: all concurrent payload operations are aligned AtomicU64 loads/stores;
// immutable address/size fields never change. Borrows of the owner or a retained
// Arc keep the allocation alive; this private Sync does not make Arena Sync.
unsafe impl Sync for Mapping {}

pub(crate) fn page_size() -> Result<usize> {
    // SAFETY: sysconf has no pointer argument; this asks for the host page size.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value <= 0 || value % 8 != 0 {
        return Err(Error::Protocol("unsupported system page size"));
    }
    Ok(value as usize)
}
impl Mapping {
    pub(crate) fn new(words: usize) -> Result<Self> {
        if words == 0 {
            return Err(Error::InvalidSize);
        }
        let page = page_size()?;
        let bytes = words
            .checked_mul(8)
            .and_then(|n| n.checked_add(page - 1))
            .map(|n| n / page * page)
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or(Error::InvalidSize)?;
        // SAFETY: nonzero checked length, private anonymous mapping, no fixed
        // address or backing descriptor. mmap initializes every byte to zero.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(os_error("mmap"));
        }
        let base = match NonNull::new(raw.cast::<u8>()) {
            Some(base) => base,
            None => {
                // SAFETY: even a null-address success must be unmapped before rejection.
                if unsafe { libc::munmap(raw, bytes) } != 0 {
                    std::process::abort();
                }
                return Err(Error::Protocol("null mapping address"));
            }
        };
        let map = Self {
            base,
            bytes,
            page,
            #[cfg(test)]
            copy_error_at: std::sync::atomic::AtomicUsize::new(usize::MAX),
        };
        // SAFETY: whole owned mapping, valid Linux advice, before page population.
        if unsafe { libc::madvise(raw, bytes, libc::MADV_NOHUGEPAGE) } != 0 {
            return Err(os_error("madvise(MADV_NOHUGEPAGE)"));
        }
        for slot in 0..map.len_words() {
            map.store(slot, 0)?;
        }
        Ok(map)
    }
    pub(crate) fn len_words(&self) -> usize {
        self.bytes / 8
    }
    pub(crate) fn address(&self) -> usize {
        self.base.as_ptr() as usize
    }
    pub(crate) fn len_bytes(&self) -> usize {
        self.bytes
    }
    pub(crate) fn page_size(&self) -> usize {
        self.page
    }
    fn atomic(&self, slot: usize) -> Result<&AtomicU64> {
        if slot >= self.len_words() {
            return Err(Error::Bounds);
        }
        // SAFETY: bounds checked within one initialized, page-aligned allocation;
        // offsets are multiples of 8 and <= isize::MAX. All overlapping live
        // accesses use AtomicU64. The returned private view is bounded by &self.
        Ok(unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(slot * 8).cast::<u64>()) })
    }
    pub(crate) fn load(&self, slot: usize) -> Result<u64> {
        Ok(self.atomic(slot)?.load(Ordering::Relaxed))
    }
    pub(crate) fn store(&self, slot: usize, value: u64) -> Result<()> {
        self.atomic(slot)?.store(value, Ordering::Relaxed);
        Ok(())
    }
    // Keep the concurrent copy routine identifiable in release IR/disassembly.
    #[inline(never)]
    pub(crate) fn copy_page(&self, page: usize, image: &mut [u64]) -> Result<()> {
        if page >= self.bytes / self.page || image.len() != self.page / 8 {
            return Err(Error::Bounds);
        }
        let start = page * (self.page / 8);
        for (offset, target) in image.iter_mut().enumerate() {
            *target = self.load(start + offset)?;
            #[cfg(test)]
            if self
                .copy_error_at
                .compare_exchange(
                    start + offset,
                    usize::MAX,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Err(Error::Protocol("injected partial page copy failure"));
            }
        }
        Ok(())
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the direct-module integration harness.
    pub(crate) fn fail_copy_at(&self, slot: usize) {
        self.copy_error_at.store(slot, Ordering::Relaxed);
    }
}
fn os_error(operation: &'static str) -> Error {
    Error::System {
        operation,
        errno: std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: owns this exact allocation; owner borrows/Arc lifetime rules
        // ensure no accessor remains. A registered context holds its own Arc
        // until its fd is closed.
        if unsafe { libc::munmap(self.base.as_ptr().cast(), self.bytes) } != 0 {
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialized_rounded_mapping_and_atomic_page_copy() {
        let page = page_size().unwrap();
        let map = Mapping::new(1).unwrap();
        assert_eq!(map.len_words(), page / 8);
        assert_eq!(map.page_size(), page);
        assert_eq!(map.address() % page, 0);
        assert_eq!(map.load(map.len_words() - 1), Ok(0));
        map.store(0, 0x1234).unwrap();
        map.store(map.len_words() - 1, 0x5678).unwrap();
        let mut image = vec![0; page / 8];
        map.copy_page(0, &mut image).unwrap();
        assert_eq!(image[0], 0x1234);
        assert_eq!(*image.last().unwrap(), 0x5678);
        assert_eq!(map.load(map.len_words()), Err(Error::Bounds));
        assert_eq!(map.store(map.len_words(), 1), Err(Error::Bounds));
        assert_eq!(map.copy_page(1, &mut image), Err(Error::Bounds));
        assert_eq!(map.copy_page(0, &mut image[..1]), Err(Error::Bounds));
    }
    #[test]
    fn rejects_invalid_allocation_sizes() {
        assert!(matches!(Mapping::new(0), Err(Error::InvalidSize)));
        assert!(matches!(Mapping::new(usize::MAX), Err(Error::InvalidSize)));
    }
}

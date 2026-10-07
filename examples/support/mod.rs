use std::error::Error;

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub fn hash(words: impl IntoIterator<Item = u64>) -> u64 {
    words.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, word| {
        (hash ^ word).wrapping_mul(0x100_0000_01b3)
    })
}

pub fn page_size() -> Result<usize> {
    // SAFETY: sysconf takes a constant selector and accesses no caller memory.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(page as usize)
}

pub fn arena_words(mib: usize) -> Result<usize> {
    mib.checked_mul(1024 * 1024)
        .filter(|&bytes| bytes > 0 && bytes <= isize::MAX as usize)
        .map(|bytes| bytes / 8)
        .ok_or_else(|| "arena MiB must be positive and fit isize::MAX bytes".into())
}

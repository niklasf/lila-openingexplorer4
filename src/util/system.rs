/// Total physical memory in bytes.
pub fn total_memory() -> Option<u64> {
    // SAFETY: sysconf has no preconditions.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)
}

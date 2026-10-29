use std::{ffi::CString, fs, io, mem, path::Path};

/// Total physical memory in bytes.
pub fn total_memory() -> Option<u64> {
    // SAFETY: sysconf has no preconditions.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)
}

/// Bytes available to unprivileged users on the file system containing
/// `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs only writes to the zeroed struct.
    let mut stat: libc::statvfs = unsafe { mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Creates a directory, unless it already exists.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => Err(io::Error::new(
            err.kind(),
            format!("{}: {err}", dir.display()),
        )),
        _ => Ok(()),
    }
}

/// Creates a directory like `create_dir`, and fails if it is not empty.
pub fn create_empty_dir(dir: &Path) -> io::Result<()> {
    create_dir(dir)?;
    if fs::read_dir(dir)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", dir.display())))?
        .next()
        .is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{}: not empty", dir.display()),
        ));
    }
    Ok(())
}

//! Platform page size for allocation-budget scenarios. Budgets must include
//! rounded mappings on macOS/Windows as well as Linux.
pub fn page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no pointer arguments.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info = std::mem::MaybeUninit::<SYSTEM_INFO>::zeroed();
        // SAFETY: initialized output buffer has the required size.
        unsafe {
            GetSystemInfo(info.as_mut_ptr());
            info.assume_init().dwPageSize as usize
        }
    }
}

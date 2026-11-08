//! Small Windows-only process helpers. No-ops elsewhere.

/// Restrict later `LoadLibrary` calls to the application directory and System32.
///
/// This covers runtime loads only. Load-time imports such as `DirectML.dll`
/// resolve before `main` runs: application directory first, then System32.
pub fn harden_dll_search() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{
            SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
}

/// Peak working set of this process in whole MiB (0 when unknown).
pub fn peak_working_set_mb() -> u32 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            return (c.PeakWorkingSetSize / (1024 * 1024)) as u32;
        }
    }
    0
}

/// Replace the default panic hook, which prints the panic message. Messages can carry
/// document text (a slice index, an unwrap on a decoded string), so only the source
/// location is logged.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        match info.location() {
            Some(l) => eprintln!("infield-ocr: panic at {}:{}", l.file(), l.line()),
            None => eprintln!("infield-ocr: panic"),
        }
    }));
}

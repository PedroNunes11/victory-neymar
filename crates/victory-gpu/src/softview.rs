//! Views of physical pages kept in the books alone (`VICTORY_SOFT_VIEWS`,
//! off by default; item 2's port of victory-scarlett's freeze fix).
//!
//! The title's page-mapping batch copies (`CopyPageMappingsBatchX`) map one
//! 64 KiB view per page into its tiled texture heap, and unmap it later: a
//! `MapViewOfFile3` and an `UnmapViewOfFile2` each, 100-290 us under memory
//! pressure. In Pedro's open-world run the frame log's remapping took 97.6 s
//! of the 313 s spent in long frames. With this switch those views can stay
//! in the books, and whoever reads them reads the same bytes through one
//! lasting view of the whole section, a window, instead.
//!
//! Idea from Op-lite/victory-scarlett (branch `scarlett-fox`,
//! `crates/victory-hle/src/modules/xmem.rs`, `mod soft`, commit `bc264a4174`,
//! author "scarlett-local", GPL-3.0), written
//! anew here.
//!
//! A window is a view of a pagefile-backed section, so it is shared memory:
//! it adds nothing to the process's private bytes (what the memory guard
//! counts) or to the commit, only page tables for the pages read through it.
//! It is never opened as a D3D12 heap.


use std::collections::HashMap;
use std::sync::Mutex;

/// What `VICTORY_SOFT_VIEWS` asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Every view is mapped, as before.
    Off,
    /// Every view is mapped as before, and the reads a soft view would take
    /// through the window are compared with the view's bytes.
    Verify,
    /// The batch copies' views stay in the books until touched.
    Copies,
}

thread_local! {
    /// For tests (here and the runtime's), on their own thread only, so that
    /// tests running meanwhile keep the switch: 1 off, 2 verify, 3 copies; 0
    /// reads the switch.
    static TEST_MODE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Tests set the mode with this; `None` goes back to the switch.
#[doc(hidden)]
pub fn set_mode_for_tests(mode: Option<Mode>) {
    let n = match mode {
        None => 0,
        Some(Mode::Off) => 1,
        Some(Mode::Verify) => 2,
        Some(Mode::Copies) => 3,
    };
    TEST_MODE.with(|m| m.set(n));
}

/// Read once.
pub fn mode() -> Mode {
    match TEST_MODE.with(std::cell::Cell::get) {
        1 => return Mode::Off,
        2 => return Mode::Verify,
        3 => return Mode::Copies,
        _ => {}
    }
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_SOFT_VIEWS").as_deref().map(str::trim) {
        Ok("verify") => Mode::Verify,
        Ok("copies" | "1") => Mode::Copies,
        _ => Mode::Off,
    })
}

/// Windows by section handle: the address of the view of the whole section
/// and its bytes.
static WINDOWS: Mutex<Option<HashMap<usize, (usize, usize)>>> = Mutex::new(None);

/// The window of `section`, mapped on first use; `None` if the section is
/// not one made for physical pages or the host refuses the view.
fn window(section: usize) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile};
    let mut all = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let all = all.get_or_insert_with(HashMap::new);
    if let Some(&w) = all.get(&section) {
        return Some(w);
    }
    let bytes = usize::try_from(crate::inplace::section_bytes(section)?).ok()?;
    // SAFETY: a section handle the runtime keeps open until
    // `title_memory_section_gone`; a read-only view of all of it.
    let at = unsafe { MapViewOfFile(section as _, FILE_MAP_READ, 0, 0, bytes) };
    if at.Value.is_null() {
        tracing::warn!(bytes, error = %std::io::Error::last_os_error(), "no window over a section of physical pages");
        return None;
    }
    let w = (at.Value as usize, bytes);
    all.insert(section, w);
    Some(w)
}

/// The runtime closes `section`: its window goes, so that it does not keep
/// the section's memory alive.
pub fn title_memory_section_gone(section: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
    let gone = WINDOWS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|all| all.remove(&section));
    if let Some((at, _)) = gone {
        // SAFETY: our view, out of the table, so no reader can still get it.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as _ }) };
    }
}

/// Copies the bytes `[offset, offset + out.len())` of `section` into `out`
/// through its window; false if there is no window or a page there was
/// never committed.
///
/// Read through the kernel (`ReadProcessMemory`), which reports a page never
/// committed instead of faulting. Asking `VirtualQuery` first, as the first
/// version did, walks the window's page tables to the end of its region: in
/// a 4 GiB window that froze the texture pass for 14-33 s a frame
/// (`item2-check-softviews`, 26/11).
pub(crate) fn read_section(section: usize, offset: u64, out: &mut [u8]) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let Some((base, bytes)) = window(section) else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    if offset.checked_add(out.len()).is_none_or(|end| end > bytes) {
        return false;
    }
    let mut read = 0usize;
    // SAFETY: our own process; `out` has room for `out.len()` bytes. A page
    // the title writes meanwhile through another view reads either way, as
    // with any read in place.
    let ok = unsafe {
        ReadProcessMemory(GetCurrentProcess(), (base + offset) as _, out.as_mut_ptr().cast(), out.len(), &mut read)
    };
    ok != 0 && read == out.len()
}

/// Copies the bytes the title's views show at `[at, at + out.len())` into
/// `out` through the sections' windows; false if any byte is not in a
/// known view of a section with a window.
pub(crate) fn read_title(at: usize, out: &mut [u8]) -> bool {
    let end = at + out.len();
    let mut here = at;
    while here < end {
        let Some(run) = crate::inplace::run_around(here, here, end) else {
            return false;
        };
        let (from, to) = (here - at, run.end - at);
        if run.end <= here || !read_section(run.section, run.offset, &mut out[from..to]) {
            return false;
        }
        here = run.end;
    }
    true
}

/// The 64 KiB pages the title's batch copies mapped, by address: the views
/// [`Mode::Copies`] keeps in the books, and that [`Mode::Verify`] checks.
/// Forgotten when the range is mapped anew or unmapped ([`forget`]).
static NOTED: Mutex<std::collections::BTreeSet<usize>> = Mutex::new(std::collections::BTreeSet::new());

const K64: usize = 64 << 10;

/// Whether any noted page touches `[start, start + len)`: with
/// [`Mode::Copies`], a page kept in the books, which reads as unmapped
/// through the kernel and faults in place.
pub(crate) fn any_noted(start: usize, len: usize) -> bool {
    mode() != Mode::Off
        && NOTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .range(start.saturating_sub(K64 - 1)..start.saturating_add(len))
            .next()
            .is_some()
}

/// With [`Mode::Copies`]: `out` holds the bytes at `[start, start +
/// out.len())` as read through the kernel (zeros where a page is not
/// mapped); the parts on pages kept in the books are filled from the
/// windows. False if one of them could not be read.
pub(crate) fn overlay(start: usize, out: &mut [u8]) -> bool {
    if mode() != Mode::Copies {
        return true;
    }
    let end = start + out.len();
    let pages: Vec<usize> = NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .range(start.saturating_sub(K64 - 1)..end)
        .copied()
        .collect();
    let mut all = true;
    for page in pages {
        let (from, to) = (page.max(start), (page + K64).min(end));
        all &= read_title(from, &mut out[from - start..to - start]);
    }
    all
}

/// Makes the page kept in the books at an address real (the runtime's
/// `xmem::make_real`): true if it was one and is mapped now.
pub type Maker = fn(usize) -> bool;

static MAKER: std::sync::OnceLock<Maker> = std::sync::OnceLock::new();

/// The runtime hands over how a page kept in the books is made real, and
/// the handler that does it on a touch is installed (with
/// [`Mode::Copies`] only).
pub fn use_maker(maker: Maker) {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    if mode() != Mode::Copies || MAKER.set(maker).is_err() {
        return;
    }
    unsafe extern "system" fn on_touch(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // A read or a write alike: the page is not mapped at all.
        let at = r.ExceptionInformation[1];
        let page = at & !(K64 - 1);
        let kept = NOTED.lock().unwrap_or_else(|e| e.into_inner()).contains(&page);
        if !kept {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        TOUCHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match MAKER.get() {
            Some(make) if make(page) => EXCEPTION_CONTINUE_EXECUTION,
            _ => EXCEPTION_CONTINUE_SEARCH,
        }
    }
    // SAFETY: the handler only looks at our books and maps the page.
    unsafe { AddVectoredExceptionHandler(1, Some(on_touch)) };
}

/// With [`Mode::Copies`]: whether the page holding `at` is kept in the books.
pub(crate) fn is_kept(at: usize) -> bool {
    mode() == Mode::Copies && NOTED.lock().unwrap_or_else(|e| e.into_inner()).contains(&(at & !(K64 - 1)))
}

/// With [`Mode::Copies`]: before a write through the kernel into `[at, at +
/// len)` (which does not fault), the pages kept in the books there are
/// made real.
pub(crate) fn make_real_range(at: usize, len: usize) {
    if mode() != Mode::Copies {
        return;
    }
    let Some(make) = MAKER.get() else {
        return;
    };
    let pages: Vec<usize> = NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .range(at.saturating_sub(K64 - 1)..at.saturating_add(len))
        .copied()
        .collect();
    for page in pages {
        make(page);
    }
}

/// Pages kept in the books that a touch made real since the last [`say`].
static TOUCHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The runtime mapped the page at `at` for a batch copy (`xmem::map_alias`),
/// after the mapping, when the switch is on.
pub fn note_batch_page(at: usize) {
    if mode() != Mode::Off {
        NOTED.lock().unwrap_or_else(|e| e.into_inner()).insert(at);
    }
}

/// What `[at, at + len)` showed is gone (`crate::inplace::views_gone`).
pub(crate) fn forget(at: usize, len: usize) {
    if mode() == Mode::Off {
        return;
    }
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let gone: Vec<usize> = noted.range(at.saturating_sub(K64 - 1)..at.saturating_add(len)).copied().collect();
    for page in gone {
        noted.remove(&page);
    }
}

/// [`verify`]'s counts since the last [`say`]: pages compared, bytes
/// compared, pages that differed, pieces the windows could not read, and
/// the bytes of this second's budget.
static VERIFIED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
static BUDGET: Mutex<(Option<std::time::Instant>, u64)> = Mutex::new((None, 0));

/// Bytes compared a second at most.
const VERIFY_BYTES_A_SECOND: u64 = 16 << 20;

/// With [`Mode::Verify`]: `bytes` were just read in place at `start` (the
/// texture pass); the parts of them on noted pages are read again through
/// the windows and compared. A soft view would have answered with the
/// window's bytes, so a difference is a texture `Copies` would get wrong.
pub(crate) fn verify(start: usize, bytes: &[u8]) {
    use std::sync::atomic::Ordering::Relaxed;
    if mode() != Mode::Verify || bytes.is_empty() {
        return;
    }
    let end = start + bytes.len();
    let pages: Vec<usize> = NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .range(start.saturating_sub(K64 - 1)..end)
        .copied()
        .collect();
    if pages.is_empty() {
        return;
    }
    let mut piece = vec![0u8; K64];
    for page in pages {
        let (from, to) = (page.max(start), (page + K64).min(end));
        let n = (to - from) as u64;
        {
            let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if budget.0.is_none_or(|at| now.duration_since(at).as_secs() >= 1) {
                *budget = (Some(now), 0);
            }
            if budget.1 + n > VERIFY_BYTES_A_SECOND {
                return;
            }
            budget.1 += n;
        }
        let out = &mut piece[..to - from];
        if !read_title(from, out) {
            VERIFIED[3].fetch_add(1, Relaxed);
            continue;
        }
        VERIFIED[0].fetch_add(1, Relaxed);
        VERIFIED[1].fetch_add(n, Relaxed);
        if out[..] != bytes[from - start..to - start] && !same_again(from, out) {
            VERIFIED[2].fetch_add(1, Relaxed);
        }
    }
}

/// After a difference: whether the title's view and the window agree when
/// both are read again now. The title or the kernel may have written the
/// page between the first two reads (a texture streaming in); only a page
/// that still differs is one a soft view would get wrong. `scratch` is
/// overwritten.
fn same_again(at: usize, scratch: &mut [u8]) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut view = vec![0u8; scratch.len()];
    let mut read = 0usize;
    // SAFETY: our own process; `view` has room for its length. An unmapped
    // page fails the read instead of faulting.
    let ok = unsafe {
        ReadProcessMemory(GetCurrentProcess(), at as _, view.as_mut_ptr().cast(), view.len(), &mut read)
    };
    ok != 0 && read == view.len() && read_title(at, scratch) && scratch[..] == view[..]
}

/// Prints the books and [`verify`]'s counts, and starts the counts again
/// (with the copy census, every 30 s).
#[cfg(feature = "diagnostics")]
pub(crate) fn say() {
    use std::sync::atomic::Ordering::Relaxed;
    if mode() == Mode::Off {
        return;
    }
    let [pages, bytes, differing, unread] = VERIFIED.each_ref().map(|c| c.swap(0, Relaxed));
    let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).len();
    tracing::info!(
        "soft views (VICTORY_SOFT_VIEWS={:?}): {noted} batch-copy pages noted now; checked {pages} pages ({:.1} MB) \
         against the windows, {differing} DIFFERING, {unread} not readable through a window; {touched} kept pages made \
         real by a touch",
        mode(),
        bytes as f64 / 1e6,
        touched = TOUCHED.swap(0, Relaxed)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test at a time: they share the views and windows tables, and a
    /// section handle one closes can come back as another's.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
        SEC_COMMIT, UnmapViewOfFile,
    };

    #[test]
    fn a_title_view_reads_the_same_through_the_window() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: a fresh committed section of four pages, closed at the end.
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_COMMIT, 0, (4 * K64) as u32, std::ptr::null())
        };
        assert!(!section.is_null());
        let handle = section as usize;
        crate::inplace::title_memory_section(handle, (4 * K64) as u64, true);
        // A title view of pages 1 and 2, written through it.
        // SAFETY: a view of the section above, unmapped at the end.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, K64 as u32, 2 * K64) };
        assert!(!view.Value.is_null());
        let at = view.Value as usize;
        // SAFETY: the view's 128 KiB, ours.
        let bytes = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, 2 * K64) };
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        crate::inplace::title_memory_view(at, 2 * K64, handle, K64 as u64);

        let mut out = vec![0u8; 3000];
        assert!(read_title(at + K64 - 1000, &mut out));
        assert_eq!(&out[..], &bytes[K64 - 1000..K64 + 2000]);
        // Past the view: not known, untouched.
        let mut past = vec![0u8; 16];
        assert!(!read_title(at + 2 * K64 - 8, &mut past));
        // A write through the title's view shows in the window at once.
        bytes[5] = 0xab;
        let mut one = [0u8; 1];
        assert!(read_section(handle, K64 as u64 + 5, &mut one));
        assert_eq!(one[0], 0xab);

        crate::inplace::views_gone(at, 2 * K64);
        title_memory_section_gone(handle);
        // SAFETY: the view and section made above.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as _ });
            CloseHandle(section);
        }
    }

    #[test]
    fn verify_counts_a_page_whose_window_bytes_differ() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::atomic::Ordering::Relaxed;
        set_mode_for_tests(Some(Mode::Verify));
        // SAFETY: a fresh committed section of two pages, closed at the end.
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_COMMIT, 0, (2 * K64) as u32, std::ptr::null())
        };
        assert!(!section.is_null());
        let handle = section as usize;
        crate::inplace::title_memory_section(handle, (2 * K64) as u64, true);
        // SAFETY: a view of the section above, unmapped at the end.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, K64) };
        assert!(!view.Value.is_null());
        let at = view.Value as usize;
        // SAFETY: the view's 64 KiB, ours.
        let bytes = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, K64) };
        bytes.fill(0x5a);
        crate::inplace::title_memory_view(at, K64, handle, 0);
        note_batch_page(at);
        for c in &VERIFIED {
            c.store(0, Relaxed);
        }
        // What was read in place is what the window shows: no difference.
        let same = bytes.to_vec();
        verify(at + 100, &same[100..4100]);
        assert_eq!(VERIFIED[2].load(Relaxed), 0);
        assert!(VERIFIED[0].load(Relaxed) >= 1);
        // A read that saw other bytes than the window, but the page agrees
        // with it when both are read again: a write between the reads, not
        // a difference.
        let mut other = same.clone();
        other[200] ^= 1;
        verify(at, &other);
        assert_eq!(VERIFIED[2].load(Relaxed), 0);
        // The books say the view shows another page of the section than it
        // does: the window reads other bytes for good, one differing page.
        // SAFETY: the section's second page, through a view of our own.
        let second = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, K64 as u32, K64) };
        assert!(!second.Value.is_null());
        // SAFETY: that view's 64 KiB, ours.
        unsafe { std::slice::from_raw_parts_mut(second.Value as *mut u8, K64) }.fill(0x33);
        crate::inplace::views_gone(at, K64);
        crate::inplace::title_memory_view(at, K64, handle, K64 as u64);
        note_batch_page(at);
        verify(at, &same);
        assert_eq!(VERIFIED[2].load(Relaxed), 1);
        // SAFETY: the view made above.
        unsafe { UnmapViewOfFile(second) };
        // Unmapped (forgotten): no longer checked.
        crate::inplace::views_gone(at, K64);
        let before = VERIFIED[0].load(Relaxed);
        verify(at, &other);
        assert_eq!(VERIFIED[0].load(Relaxed), before);
        set_mode_for_tests(None);
        title_memory_section_gone(handle);
        // SAFETY: the view and section made above.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as _ });
            CloseHandle(section);
        }
    }

    #[test]
    fn a_page_never_committed_is_not_read() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        use windows_sys::Win32::System::Memory::SEC_RESERVE;
        // SAFETY: a fresh reserved section of two pages, closed at the end.
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_RESERVE, 0, (2 * K64) as u32, std::ptr::null())
        };
        assert!(!section.is_null());
        let handle = section as usize;
        crate::inplace::title_memory_section(handle, (2 * K64) as u64, false);
        let mut out = [0u8; 8];
        assert!(!read_section(handle, 0, &mut out));
        title_memory_section_gone(handle);
        // SAFETY: the section made above.
        unsafe { CloseHandle(section) };
    }
}

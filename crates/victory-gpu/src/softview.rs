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
//! `crates/victory-hle/src/modules/xmem.rs`, `mod soft`, GPL-3.0), written
//! anew here.
//!
//! A window is a view of a pagefile-backed section, so it is shared memory:
//! it adds nothing to the process's private bytes (what the memory guard
//! counts) or to the commit, only page tables for the pages read through it.
//! It is never opened as a D3D12 heap.

// The books and the readers that use the windows come in the next commits.
#![cfg_attr(not(test), allow(dead_code))]

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

/// Read once.
pub fn mode() -> Mode {
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

/// Whether every page of `[at, at + len)` is committed (a section made
/// reserved has its pages committed when first mapped).
fn committed(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let end = at + len;
    let mut here = at;
    while here < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(here as _, &mut info, size_of_val(&info)) } == 0 || info.State != MEM_COMMIT {
            return false;
        }
        here = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// Copies the bytes `[offset, offset + out.len())` of `section` into `out`
/// through its window; false (and `out` untouched) if there is no window or
/// a page there was never committed.
pub(crate) fn read_section(section: usize, offset: u64, out: &mut [u8]) -> bool {
    let Some((base, bytes)) = window(section) else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    if offset.checked_add(out.len()).is_none_or(|end| end > bytes) || !committed(base + offset, out.len()) {
        return false;
    }
    // SAFETY: committed bytes of our window, read-only; the title may write
    // them meanwhile through another view, as with any read in place.
    unsafe { std::ptr::copy_nonoverlapping((base + offset) as *const u8, out.as_mut_ptr(), out.len()) };
    true
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
        SEC_COMMIT, UnmapViewOfFile,
    };

    const K64: usize = 64 << 10;

    #[test]
    fn a_title_view_reads_the_same_through_the_window() {
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
    fn a_page_never_committed_is_not_read() {
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

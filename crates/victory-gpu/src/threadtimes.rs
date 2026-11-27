//! Processor time by thread of the whole process, every census (30 s,
//! diagnostics only; item 2).
//!
//! In Pedro's open-world run (407b) long frames grow with the process's page
//! faults and kernel time (frames of 134 ms with ~1,200 faults, 637 ms with
//! ~115,000), while the submitting thread grows far less: the rest is the
//! title's threads stopped in the kernel. Windows counts page faults per
//! process only, so this says which threads spend the kernel time, beside
//! the process's faults and working set over the same span, to tell a
//! working set trimmed under memory pressure (faults back in) from the
//! first touch of streamed data.

use std::collections::HashMap;
use std::sync::Mutex;

/// By thread id: (user, kernel) in 100 ns units at the last census.
static LAST: Mutex<Option<(HashMap<u32, (u64, u64)>, u32, std::time::Instant)>> = Mutex::new(None);

fn filetime(t: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)
}

/// Every thread of this process now: id, (user, kernel), and its name.
fn threads() -> Vec<(u32, (u64, u64), String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    let me = unsafe { GetCurrentProcessId() };
    let mut out = Vec::new();
    // SAFETY: a snapshot of the system's threads, closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return out;
    }
    let mut e = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: a valid snapshot and entry of the size given.
    let mut more = unsafe { Thread32First(snap, &mut e) } != 0;
    while more {
        if e.th32OwnerProcessID == me {
            // SAFETY: a thread of our own process, closed below.
            let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, e.th32ThreadID) };
            if !h.is_null() {
                let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
                let (mut c, mut x, mut k, mut u) = (z, z, z, z);
                // SAFETY: valid handle and out pointers.
                if unsafe { GetThreadTimes(h, &mut c, &mut x, &mut k, &mut u) } != 0 {
                    let mut name = String::new();
                    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
                    // SAFETY: the system allocates the description; freed below.
                    if unsafe { GetThreadDescription(h, &mut p) } >= 0 && !p.is_null() {
                        // SAFETY: a nul-terminated wide string from the system.
                        let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
                        name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
                        // SAFETY: allocated by GetThreadDescription with LocalAlloc.
                        unsafe { windows_sys::Win32::Foundation::LocalFree(p.cast()) };
                    }
                    out.push((e.th32ThreadID, (filetime(u), filetime(k)), name));
                }
                // SAFETY: the handle opened above.
                unsafe { CloseHandle(h) };
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap, &mut e) } != 0;
    }
    // SAFETY: the snapshot made above.
    unsafe { CloseHandle(snap) };
    out
}

/// The process's page faults so far, working set and private bytes.
fn process_memory() -> (u32, usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: our process; a structure of the size given.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    (c.PageFaultCount, c.WorkingSetSize, c.PrivateUsage)
}

/// The system's available physical memory, in bytes.
fn available() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: a structure of the size given.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    m.ullAvailPhys
}

/// Prints, over the `frames` frames since the last call, the threads that
/// spent the most kernel time (and their user time), marking the recording
/// threads, with the process's page faults, working set, private bytes and
/// the system's available memory.
pub(crate) fn say(frames: f64) {
    let now = threads();
    let (faults, ws, private) = process_memory();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let Some((before, faults_before, at)) = last.take() else {
        *last = Some((now.iter().map(|t| (t.0, t.1)).collect(), faults, std::time::Instant::now()));
        return;
    };
    *last = Some((now.iter().map(|t| (t.0, t.1)).collect(), faults, std::time::Instant::now()));
    drop(last);
    if frames <= 0.0 {
        return;
    }
    let recorders = crate::copystat::recorder_ids();
    // Per frame, in ms: (kernel, user, id, name).
    let mut rows: Vec<(f64, f64, u32, &str)> = now
        .iter()
        .map(|(id, (u, k), name)| {
            let (u0, k0) = before.get(id).copied().unwrap_or((0, 0));
            (k.saturating_sub(k0) as f64 / 1e4 / frames, u.saturating_sub(u0) as f64 / 1e4 / frames, *id, name.as_str())
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (kernel, user) = rows.iter().fold((0.0, 0.0), |s, r| (s.0 + r.0, s.1 + r.1));
    let rec_kernel: f64 = rows.iter().filter(|r| recorders.contains(&r.2)).map(|r| r.0).sum();
    let line: Vec<String> = rows
        .iter()
        .take(10)
        .map(|r| {
            format!(
                "thread {}{}{} kernel {:.1} user {:.1}",
                r.2,
                if r.3.is_empty() { String::new() } else { format!(" \"{}\"", r.3) },
                if recorders.contains(&r.2) { " (records)" } else { "" },
                r.0,
                r.1
            )
        })
        .collect();
    tracing::info!(
        "processor time by thread over {frames:.0} frames ({:.0} s), ms per frame: all threads kernel {kernel:.1} \
         user {user:.1}, of the kernel {rec_kernel:.1} on recording threads; page faults {:.0} a frame; working set \
         {:.2} GB, private {:.2} GB, available {:.2} GB; {}",
        at.elapsed().as_secs_f64(),
        faults.wrapping_sub(faults_before) as f64 / frames,
        ws as f64 / 1e9,
        private as f64 / 1e9,
        available() as f64 / 1e9,
        line.join("; ")
    );
}

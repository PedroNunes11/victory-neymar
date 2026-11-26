//! Windows of title memory the host GPU reads and writes where they are,
//! with no host copy (`VICTORY_UNIFIED=1`, off by default; after the
//! unified-memory design of the layer's first in-place experiment and
//! Marvisak/VIctory's pull request 2921 for letting held memory go).
//!
//! Ported from ka1qux/BRVIctory-ka1qux (GPL-3.0), Kaique's commit
//! 23a83b739 "GPU, HLE: windows of title memory made in place, with no host
//! copy (VICTORY_UNIFIED=1)", fitted to this tree's copies
//! (`crate::shadow`), its in-place file (`crate::inplace`) and its
//! runtime's hooks (`victory_hle`'s xmem).
//!
//! A window is what a copy of title memory outside the title's buffer
//! objects is: a 4 MiB cell of the title's addresses (`crate::shadow`'s
//! `loose`). With the switch on, an ask that finds no window is given, when
//! it can be, a window *in place*: a buffer placed over a heap that the host
//! opens on the title's own pages (`OpenExistingHeapFromAddress1`), so that
//! nothing is copied up, refreshed or copied back, and the window holds no
//! host memory of its own. Two kinds of memory are opened, the two the
//! host can open:
//!
//! * **view memory**, the title's physical pages, mapped at the title's
//!   addresses one view of 64 KiB or more at a time: the run of pages that
//!   are consecutive in one section around the ask (cut to the 4 MiB cells
//!   the ask lies in) is mapped once more through a view of ours, and the
//!   heap is opened on that view ([`inplace::place_run`]);
//! * **private memory**, where the ask lies in a committed read-write run
//!   of an allocation that is at least [`SMALLEST_RUN`] long (the title's
//!   arenas, which it keeps; not the allocations it makes and frees as it
//!   goes): the part of the run in the ask's cells is opened where it is
//!   ([`inplace::place_part`]).
//!
//! What stays a copy: an ask whose pages are not consecutive in one
//! section (scattered pages, holes), a root view with no length whose pages
//! stop being consecutive before its window's end (its shader may read on),
//! small private allocations, memory that is neither private nor a view,
//! asks that find the host's shared GPU memory nearly used, and anything
//! shorter than [`LEAST`]. [`Reason`] names each, the log counts them
//! ([`tick`]), and a diagnostics build gives a census of the copies alive
//! with what the switch would do with each ([`census`]).
//!
//! # The rules of the host, and how they are kept
//!
//! * A heap opened on memory locks its pages: while it lives `VirtualFree`
//!   (decommit or release) and `VirtualProtect` on them, and unmapping the
//!   view it was opened on, fail with error 87 (measured here, and by
//!   Marvisak/VIctory pull request 2921 on NVIDIA, AMD and WARP). Heaps
//!   over physical pages are on views of ours, so the title's mapping and
//!   unmapping of its own views is never refused; the title's `VirtualFree`
//!   and `VirtualProtect` of private memory go through
//!   [`title_memory_change`], which lets the windows over the range go first
//!   and tries the call again for a while if the host unlocks late (the idea
//!   and the retry are that pull request's).
//! * An opened heap counts whole against the host's shared GPU memory (half
//!   of the PC's memory): nothing is opened that would leave less than
//!   [`RESERVE`] of it, and no more than `VICTORY_UNIFIED_MB` (8192) at once.
//! * **Never** `OpenExistingHeapFromFileMapping`: opened from a section's
//!   handle the heap keeps the whole section committed until the PC is
//!   restarted (measured with ka1qux/BRVIctory-ka1qux's
//!   `unified_probe::unified_leak_probe`, not ported). Heaps
//!   here are opened on an address only, and `inplace::open` refuses the
//!   handle unless the probe asked for it.
//!
//! What the copies caught and this does not: a shader that writes a root
//! view outside the range its bind declares writes the title's live memory
//! (its copy held such writes back); a root view with no length reads to
//! its window's end, and memory the title maps or commits right after a
//! window that ended where its memory ended is not in the window (such a
//! window is let go when it happens, for views; for private memory not).
//! The submissions' rules know shader writes by the ranges declared, and
//! windows in place declare theirs like any copy's.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError, SetLastError};
use windows_sys::Win32::System::Memory as mem;

use crate::inplace::{self, Placed};
use crate::shadow::WINDOW as CELL;

/// The longest window in place (a window is the cells its ask lies in).
pub(crate) const EXTENT_MOST: u64 = 32 << 20;
/// A window in place shorter than this stays a copy: each is a host heap,
/// a buffer and (over views) a view of ours.
const LEAST: u64 = 256 << 10;
/// The shortest committed run of private memory given windows in place:
/// the title's arenas, not the allocations it makes and frees as it goes.
const SMALLEST_RUN: u64 = 64 << 20;
/// Heaps are opened from 64 KiB to 64 KiB.
const GRAIN: u64 = 64 << 10;
/// What is left of the host's shared GPU memory for copies, uploads and
/// staging: no heap is opened into it.
const RESERVE: u64 = 2 << 30;
/// How long a refusal is remembered for the cell it was made for.
const FORGET: Duration = Duration::from_secs(10);
/// How long a change of title memory refused as locked is tried again after
/// the windows over it were let go: lists not yet reset and descriptor
/// slots hold a window a little longer.
const SETTLE: Duration = Duration::from_millis(1500);

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_UNIFIED").is_ok_and(|v| v.trim() == "1"))
}

/// Whether windows of title memory are placed in place (`VICTORY_UNIFIED=1`):
/// the runtime then sends the title's frees and protections of memory
/// through [`title_memory_change`].
pub fn in_place_on() -> bool {
    on()
}

/// The most window bytes in place at once (`VICTORY_UNIFIED_MB`, 8192).
fn most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_UNIFIED_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(8192) << 20
    })
}

/// Why an ask is left to a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Reason {
    /// The title's address is no memory the host can describe.
    Undescribed,
    /// Memory that is neither private nor a view of physical pages (a
    /// mapped file, an image).
    Neither,
    /// View memory whose pages are not consecutive in one section as far
    /// as the ask goes.
    NotConsecutive,
    /// A root view with no length, in view memory whose pages stop being
    /// consecutive before the end of its window (and the title's memory
    /// goes on).
    RootRunEnds,
    /// Shorter than [`LEAST`].
    Short,
    /// Private memory not committed read-write (or not all of the ask).
    NotWhole,
    /// Private memory in a committed run shorter than [`SMALLEST_RUN`].
    SmallAllocation,
    /// More cells than [`EXTENT_MOST`].
    TooLong,
    /// The host's shared GPU memory would be left with less than
    /// [`RESERVE`].
    NoRoom,
    /// `VICTORY_UNIFIED_MB` windows are in place.
    OverCap,
    /// The runtime was changing mappings in the range.
    Mapping,
    /// Our view of the pages (or the private memory) was not committed
    /// whole when the heap was opened.
    NotCommitted,
    /// The host refused the heap or the buffer.
    Refused,
}

const REASONS: usize = 13;

impl Reason {
    fn text(self) -> &'static str {
        match self {
            Reason::Undescribed => "memory the host cannot describe",
            Reason::Neither => "memory that is neither private nor physical pages",
            Reason::NotConsecutive => "physical pages not consecutive in one section as far as the ask goes",
            Reason::RootRunEnds => "root view: physical pages stop being consecutive before its window ends",
            Reason::Short => "a run shorter than the least window in place",
            Reason::NotWhole => "private memory not committed read-write around the ask",
            Reason::SmallAllocation => "private memory, a committed run shorter than the title's arenas",
            Reason::TooLong => "an ask over more cells than the longest window in place",
            Reason::NoRoom => "the host's shared GPU memory is nearly used",
            Reason::OverCap => "VICTORY_UNIFIED_MB is reached",
            Reason::Mapping => "the runtime was changing mappings there",
            Reason::NotCommitted => "pages not committed whole when the heap was opened",
            Reason::Refused => "the host refused the heap",
        }
    }

    /// Whether the refusal may be gone a moment later (not remembered).
    fn passing(self) -> bool {
        matches!(self, Reason::NoRoom | Reason::OverCap | Reason::Mapping)
    }

    fn all() -> [Reason; REASONS] {
        use Reason::*;
        [
            Undescribed,
            Neither,
            NotConsecutive,
            RootRunEnds,
            Short,
            NotWhole,
            SmallAllocation,
            TooLong,
            NoRoom,
            OverCap,
            Mapping,
            NotCommitted,
            Refused,
        ]
    }
}

/// What the memory of a window in place is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Physical,
    Private,
}

/// A window in place worked out, not yet made: where it starts, how long it
/// is, and what it lies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) kind: Kind,
    /// Physical pages: the section's handle and the offset of `start`.
    section: usize,
    offset: u64,
    /// Private memory: the allocation's base.
    allocation: usize,
    /// It reaches where the title's memory ends (or its cell does), so a
    /// root view with no length may read to the end of it.
    pub(crate) reaches: bool,
}

/// A window in place made, for `crate::shadow` to wrap.
pub(crate) struct Made {
    pub(crate) placed: Placed,
    pub(crate) start: u64,
    pub(crate) len: u64,
    /// `Shadow::unified`'s bits.
    pub(crate) bits: u8,
}

/// The window in place for an ask of `[address, need_end)` (a root view
/// with no length: `reach`, from `address` to its cell's end or as far as
/// the title's memory goes), worked out from the views the runtime told us
/// of and the host's description of private memory. Makes and opens
/// nothing.
pub(crate) fn plan(address: u64, need_end: u64, reach: bool) -> Result<Plan, Reason> {
    if address < 0x1_0000 {
        return Err(Reason::Undescribed);
    }
    let end = need_end.max(address.saturating_add(1));
    let cell_lo = address & !(CELL - 1);
    let cell_hi = end.checked_next_multiple_of(CELL).ok_or(Reason::TooLong)?;
    if cell_hi - cell_lo > EXTENT_MOST {
        return Err(Reason::TooLong);
    }
    let cell_end = (address | (CELL - 1)) + 1;
    if let Some(run) = inplace::run_around(address as usize, cell_lo as usize, cell_hi as usize) {
        let mapped = inplace::mapped_end(address as usize, cell_hi as usize) as u64;
        let (lo, hi) = (run.start as u64, run.end as u64);
        let need = if reach { mapped.min(cell_end) } else { end };
        if hi < need {
            return Err(if reach { Reason::RootRunEnds } else { Reason::NotConsecutive });
        }
        if hi - lo < LEAST {
            return Err(Reason::Short);
        }
        return Ok(Plan {
            start: lo,
            len: hi - lo,
            kind: Kind::Physical,
            section: run.section,
            offset: run.offset,
            allocation: 0,
            reaches: hi >= mapped,
        });
    }
    let (run_lo, run_hi, allocation) = committed_run(address)?;
    let (lowest, highest) = (run_lo.next_multiple_of(GRAIN), run_hi & !(GRAIN - 1));
    if highest.saturating_sub(lowest) < SMALLEST_RUN {
        return Err(Reason::SmallAllocation);
    }
    let need = if reach { highest.min(cell_end) } else { end };
    if lowest > address || highest < need {
        return Err(Reason::NotWhole);
    }
    let (lo, hi) = (cell_lo.max(lowest), cell_hi.min(highest));
    if hi - lo < LEAST {
        return Err(Reason::Short);
    }
    Ok(Plan { start: lo, len: hi - lo, kind: Kind::Private, section: 0, offset: 0, allocation, reaches: true })
}

/// The committed, read-write run of one private allocation around
/// `address`: where it starts and ends, and the allocation's base. (The
/// host's description of memory counts from the page it is asked about,
/// not from where a region begins, so the run's start is found by walking
/// the allocation's regions from its base.) An allocation in more than
/// [`REGIONS`] regions is left alone.
fn committed_run(address: u64) -> Result<(u64, u64, usize), Reason> {
    /// Regions followed at most.
    const REGIONS: usize = 128;
    let query = |at: u64| -> Option<mem::MEMORY_BASIC_INFORMATION> {
        // SAFETY: plain data filled in by a query that dereferences nothing.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { mem::VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        (n != 0).then_some(info)
    };
    let here = query(address).ok_or(Reason::Undescribed)?;
    if here.State == mem::MEM_FREE {
        return Err(Reason::Undescribed);
    }
    if here.Type != mem::MEM_PRIVATE {
        return Err(Reason::Neither);
    }
    let base = here.AllocationBase as u64;
    let part = |i: &mem::MEMORY_BASIC_INFORMATION| {
        i.State == mem::MEM_COMMIT
            && i.Type == mem::MEM_PRIVATE
            && i.Protect == mem::PAGE_READWRITE
            && i.AllocationBase as u64 == base
    };
    // From the allocation's base on: the committed read-write regions that
    // follow one another up to the one that holds `address`.
    let (mut at, mut start) = (base, None);
    for _ in 0..REGIONS {
        let Some(info) = query(at) else {
            return Err(Reason::Undescribed);
        };
        let end = info.BaseAddress as u64 + info.RegionSize as u64;
        if part(&info) {
            start.get_or_insert(at);
        } else {
            start = None;
        }
        if end > address {
            let Some(start) = start else {
                return Err(Reason::NotWhole);
            };
            let mut stop = end;
            for _ in 0..REGIONS {
                match query(stop) {
                    Some(i) if part(&i) => stop = i.BaseAddress as u64 + i.RegionSize as u64,
                    _ => break,
                }
            }
            return Ok((start, stop, base as usize));
        }
        at = end;
    }
    Err(Reason::NotWhole)
}

/// Windows in place over private memory, by their start: their length and
/// the allocation they are in (to let them go before the title frees it).
static PRIVATE: Mutex<BTreeMap<u64, (u64, usize)>> = Mutex::new(BTreeMap::new());

/// Cells found to be no place for a window, and why, by (cell, whether the
/// ask was a root view with no length): asked about again at every ask that
/// finds no window, which for a cell left to a copy is the next ask after
/// the copy is let go.
static NOT_HERE: Mutex<Option<HashMap<(u64, bool), (Reason, Instant)>>> = Mutex::new(None);

/// Since the start: windows made over physical pages and over private
/// memory (count, bytes each); let go because their memory changed, because
/// the title freed or protected it, and the title's calls that stayed
/// refused after that.
static MADE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static LET_GO: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static STAYED: AtomicU64 = AtomicU64::new(0);
/// Asks left to a copy, by [`Reason`]: how often and the bytes of the cells
/// they were for.
static LEFT: [[AtomicU64; 2]; REASONS] = [const { [AtomicU64::new(0), AtomicU64::new(0)] }; REASONS];

fn refuse(cell: u64, reach: bool, reason: Reason) -> Option<Made> {
    LEFT[reason as usize][0].fetch_add(1, Ordering::Relaxed);
    LEFT[reason as usize][1].fetch_add(CELL, Ordering::Relaxed);
    if !reason.passing() {
        let mut not_here = NOT_HERE.lock().unwrap_or_else(|e| e.into_inner());
        let map = not_here.get_or_insert_with(HashMap::new);
        if map.len() > 8192 {
            map.retain(|_, (_, at)| at.elapsed() < FORGET);
        }
        map.insert((cell, reach), (reason, Instant::now()));
    }
    None
}

/// Why the cell is known to be no place for a window, if it is.
fn known_not_here(cell: u64, reach: bool) -> Option<Reason> {
    let not_here = NOT_HERE.lock().unwrap_or_else(|e| e.into_inner());
    not_here.as_ref()?.get(&(cell, reach)).filter(|(_, at)| at.elapsed() < FORGET).map(|(reason, _)| *reason)
}

/// Whether the host's shared GPU memory has room for a heap of `bytes`
/// more beside what the copies and uploads need.
fn room(bytes: u64) -> bool {
    match crate::host::host().and_then(|h| h.video_memory_groups()) {
        // A budget of nothing is no information (the software adapter).
        Some([_, shared]) => shared[0] == 0 || shared[1] + bytes + RESERVE <= shared[0],
        None => true,
    }
}

/// A window in place for an ask of `[address, need_end)` (see [`plan`]), made
/// now: `None` leaves the ask to a copy, as without the switch. Called with
/// no lock held, since the host calls take a while.
pub(crate) fn make(address: u64, need_end: u64, reach: bool) -> Option<Made> {
    if !on() {
        return None;
    }
    let cell = address & !(CELL - 1);
    if let Some(reason) = known_not_here(cell, reach) {
        LEFT[reason as usize][0].fetch_add(1, Ordering::Relaxed);
        LEFT[reason as usize][1].fetch_add(CELL, Ordering::Relaxed);
        return None;
    }
    let plan = match plan(address, need_end, reach) {
        Ok(plan) => plan,
        Err(reason) => return refuse(cell, reach, reason),
    };
    let alive = crate::shadow::unified_alive();
    if alive[0][1] + alive[1][1] + plan.len > most() {
        return refuse(cell, reach, Reason::OverCap);
    }
    if !room(plan.len) {
        return refuse(cell, reach, Reason::NoRoom);
    }
    let host = crate::host::host()?;
    let opened = match plan.kind {
        Kind::Physical => {
            // The runtime changes no mapping in the range while our view
            // is made and the heap opened on it: the pages are those the
            // views showed when the plan was made.
            let Some(_still) = crate::watch::title_memory_reading_now(plan.start as usize..(plan.start + plan.len) as usize) else {
                return refuse(cell, reach, Reason::Mapping);
            };
            if plan_again(address, need_end, reach) != Some(plan) {
                return refuse(cell, reach, Reason::Mapping);
            }
            inplace::place_run(&host.device, plan.start, plan.len, plan.section, plan.offset)
        }
        Kind::Private => inplace::place_part(&host.device, plan.start, plan.len),
    };
    let placed = match opened {
        Ok(placed) => placed,
        Err(None) => return refuse(cell, reach, Reason::NotCommitted),
        Err(Some(error)) => {
            static SAID: AtomicU64 = AtomicU64::new(0);
            if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::warn!(
                    start = format_args!("{:#x}", plan.start),
                    megabytes = plan.len >> 20,
                    kind = ?plan.kind,
                    %error,
                    "title memory not opened as a host heap (VICTORY_UNIFIED=1): a copy instead"
                );
            }
            return refuse(cell, reach, Reason::Refused);
        }
    };
    let physical = plan.kind == Kind::Physical;
    MADE[if physical { 0 } else { 2 }].fetch_add(1, Ordering::Relaxed);
    MADE[if physical { 1 } else { 3 }].fetch_add(plan.len, Ordering::Relaxed);
    if !physical {
        let mut private = PRIVATE.lock().unwrap_or_else(|e| e.into_inner());
        if private.len() > 4096 {
            // Windows let go by idleness leave their entry: only the ones
            // in a window still alive matter.
            private.clear();
        }
        private.insert(plan.start, (plan.len, plan.allocation));
    }
    Some(Made {
        placed,
        start: plan.start,
        len: plan.len,
        bits: if physical { 1 } else { 2 } | if plan.reaches { 4 } else { 0 },
    })
}

/// [`plan`] again, as it is now (under the hold on the runtime's changes).
fn plan_again(address: u64, need_end: u64, reach: bool) -> Option<Plan> {
    plan(address, need_end, reach).ok()
}

/// The title is about to free, decommit or protect private memory
/// `[at, at + len)` (`len` 0: the allocation that begins at `at`): the
/// windows in place over it are let go, so that the host takes the call. How
/// many there were.
pub(crate) fn release(at: usize, len: usize) -> u64 {
    if !on() {
        return 0;
    }
    let hit: Vec<(u64, u64)> = {
        let mut private = PRIVATE.lock().unwrap_or_else(|e| e.into_inner());
        if private.is_empty() {
            return 0;
        }
        let (at, end) = (at as u64, (at as u64).saturating_add(len as u64));
        let hit: Vec<(u64, u64)> = private
            .iter()
            .filter(|&(&start, &(l, allocation))| if len == 0 { allocation as u64 == at } else { start < end && at < start + l })
            .map(|(&start, &(l, _))| (start, l))
            .collect();
        for (start, _) in &hit {
            private.remove(start);
        }
        hit
    };
    let mut n = 0;
    for (start, len) in hit {
        n += crate::shadow::in_place_let_go(start, len);
    }
    LET_GO[1].fetch_add(n, Ordering::Relaxed);
    n
}

/// The title changes private memory `[at, at + len)` with `change`, a host
/// call that frees, decommits or protects it and returns nonzero when it
/// worked (`len` 0: the call frees the allocation that begins at `at`): the
/// windows in place over it are let go first. The host unlocks pages late
/// now and then, and descriptor slots and lists not yet reset hold a window
/// a little longer: a call refused as locked (error 87) after a release is
/// tried again for a while, and its answer given. The retry is that of
/// Marvisak/VIctory pull request 2921.
pub fn title_memory_change(at: usize, len: usize, mut change: impl FnMut() -> i32) -> i32 {
    let released = release(at, len);
    let mut worked = change();
    if worked != 0 || released == 0 {
        return worked;
    }
    let start = Instant::now();
    // SAFETY: plain reads of this thread's last error.
    while unsafe { GetLastError() } == ERROR_INVALID_PARAMETER && start.elapsed() < SETTLE {
        std::thread::sleep(Duration::from_millis(2));
        worked = change();
        if worked != 0 {
            tracing::debug!(
                at = format_args!("{at:#x}"),
                len,
                ms = start.elapsed().as_secs_f64() * 1e3,
                "title memory let go from its heaps was unlocked late"
            );
            return worked;
        }
    }
    if STAYED.fetch_add(1, Ordering::Relaxed) < 8 {
        // The title reads the refusal's error after the call: the log line
        // does not change it.
        // SAFETY: plain reads and writes of this thread's last error.
        let error = unsafe { GetLastError() };
        tracing::warn!(
            at = format_args!("{at:#x}"),
            len,
            error,
            "the title's change of memory was refused: a heap opened over it by VICTORY_UNIFIED=1 is still held (the call fails, as it would on the host)"
        );
        // SAFETY: as above.
        unsafe { SetLastError(error) };
    }
    worked
}

/// Every ten seconds: what is in place, what was left to copies and why.
pub(crate) fn tick() {
    if !on() {
        return;
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    static SAID_ON: std::sync::Once = std::sync::Once::new();
    SAID_ON.call_once(|| {
        tracing::info!(
            "windows of title memory outside buffers are made in place when they can be (VICTORY_UNIFIED=1): no host copy, over physical \
             pages that are consecutive in one section and over private memory in committed runs of {} MiB or more; windows shorter than {} KiB, \
             at most {} MB in place, {} MB of the host's shared GPU memory kept",
            SMALLEST_RUN >> 20,
            LEAST >> 10,
            most() >> 20,
            RESERVE >> 20
        );
    });
    let now = crate::host::now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 10_000 || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    say();
}

/// The line of what is in place and what was left to copies.
pub(crate) fn say() {
    let alive = crate::shadow::unified_alive();
    let shared = crate::host::host().and_then(|h| h.video_memory_groups()).map(|[_, shared]| (shared[1] >> 20, shared[0] >> 20));
    let left: Vec<String> = Reason::all()
        .iter()
        .filter(|r| LEFT[**r as usize][0].load(Ordering::Relaxed) > 0)
        .map(|r| {
            format!(
                "{}: {} asks ({} MB of cells)",
                r.text(),
                LEFT[*r as usize][0].load(Ordering::Relaxed),
                LEFT[*r as usize][1].load(Ordering::Relaxed) >> 20
            )
        })
        .collect();
    tracing::info!(
        "title memory in place (VICTORY_UNIFIED=1), windows alive with no host copy: {} over physical pages {} MB, {} over private memory {} MB; \
         made since the start: {} ({} MB) and {} ({} MB); let go because their memory changed {}, because the title freed or protected it {}, \
         such calls of the title still refused {}; the host's shared GPU memory in use / budget: {:?} MB; asks left to copies so far, by reason: {:?}",
        alive[0][0],
        alive[0][1] >> 20,
        alive[1][0],
        alive[1][1] >> 20,
        MADE[0].load(Ordering::Relaxed),
        MADE[1].load(Ordering::Relaxed) >> 20,
        MADE[2].load(Ordering::Relaxed),
        MADE[3].load(Ordering::Relaxed) >> 20,
        LET_GO[0].load(Ordering::Relaxed),
        LET_GO[1].load(Ordering::Relaxed),
        STAYED.load(Ordering::Relaxed),
        shared,
        left
    );
}

/// Windows in place were let go because the title's views of their pages
/// changed.
pub(crate) fn let_go_for_remap(n: u64) {
    LET_GO[0].fetch_add(n, Ordering::Relaxed);
}

/// Diagnostics: the windows alive as copies, each as the switch would
/// treat the lowest ask made in it (with or without a length as it was
/// asked): how many could be in place and over how many bytes, and for the
/// rest the reason, in lines for the log. Counts the same with the switch
/// off, which is the use of it: where the windows lie.
#[cfg(feature = "diagnostics")]
pub(crate) fn census() -> Vec<String> {
    let windows = crate::shadow::copy_windows();
    let (total, started) = (windows.len(), Instant::now());
    let mut seen = 0usize;
    let mut tally: BTreeMap<&'static str, [u64; 3]> = BTreeMap::new();
    let mut ok = [[0u64; 3]; 2];
    for (start, len, lowest, highest, unsized_ask) in windows {
        // Asking the host about private memory takes a millisecond or so
        // for a big arena: a look is at most half a second long.
        if started.elapsed() > Duration::from_millis(500) {
            break;
        }
        seen += 1;
        let address = if lowest < highest { lowest } else { start };
        let end = if unsized_ask { address + 1 } else { highest.max(address + 1) };
        match plan(address, end, unsized_ask) {
            Ok(plan) => {
                let at = usize::from(plan.kind == Kind::Private);
                ok[at][0] += 1;
                ok[at][1] += len;
                ok[at][2] += plan.len;
            }
            Err(reason) => {
                let e = tally.entry(reason.text()).or_default();
                e[0] += 1;
                e[1] += len;
            }
        }
    }
    let mb = |v: u64| v >> 20;
    let mut lines = vec![format!(
        "census of the windows alive as copies ({seen} of {total} looked at), each by its lowest ask: VICTORY_UNIFIED=1 would place {} over physical          pages ({} MB of windows, {} MB in place) and {} over private memory ({} MB of windows, {} MB in place)",
        ok[0][0],
        mb(ok[0][1]),
        mb(ok[0][2]),
        ok[1][0],
        mb(ok[1][1]),
        mb(ok[1][2])
    )];
    for (reason, [n, bytes, _]) in tally {
        lines.push(format!("census,   left to a copy: {reason}: {n} windows, {} MB", mb(bytes)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;

    const K64: usize = 64 << 10;

    /// Whether the tests that open heaps may run: the switch on and the
    /// host device on the software adapter (`VICTORY_D3D12_WARP=1` in a
    /// diagnostics build, as `crate::host` makes it), never on a card. Asked
    /// before the host device is made.
    fn on_warp() -> bool {
        let warp = cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some();
        if !on() || !warp {
            eprintln!("unified tests: skipped (VICTORY_UNIFIED=1 and the software adapter, VICTORY_D3D12_WARP=1 with diagnostics, are needed)");
            return false;
        }
        true
    }

    /// A root view with no length needs its pages consecutive to its
    /// cell's end, or to where the title's memory ends; a sized ask, only
    /// as far as it goes.
    #[test]
    fn view_memory_in_place_follows_the_runs_of_pages() {
        // Cells of 4 MiB: a run of 3.5 MiB of one section from the cell's
        // start, then two pages of another section, then a gap.
        let base = 0x7_c000_0000usize;
        let section = 4242;
        let mib = 1usize << 20;
        inplace::title_memory_view(base, 3 * mib, section, 0);
        inplace::title_memory_view(base + 3 * mib, mib / 2, section, 3 * mib as u64);
        inplace::title_memory_view(base + 3 * mib + mib / 2, 2 * K64, section + 1, 0);
        let (ask, cell_end) = (base as u64 + 0x12_3456, (base + 4 * mib) as u64);
        // A sized ask inside the run: the run, cut to the cell.
        let p = plan(ask, ask + 0x1000, false).expect("a run");
        assert_eq!((p.start, p.len, p.kind), (base as u64, (3 * mib + mib / 2) as u64, Kind::Physical));
        assert_eq!((p.section, p.offset), (section, 0));
        assert!(!p.reaches, "the title's memory goes on past the run");
        // A root view with no length: the run ends before its cell does
        // and the title's memory goes on: a copy.
        assert_eq!(plan(ask, ask + 1, true), Err(Reason::RootRunEnds));
        // A sized ask that runs past the run's end: not consecutive.
        assert_eq!(plan(ask, base as u64 + (3 * mib + mib / 2) as u64 + 8, false), Err(Reason::NotConsecutive));
        // A root view in the two pages of the other section: that
        // section's own run, too short for a window.
        let in_other = (base + 3 * mib + mib / 2) as u64 + 0x1000;
        assert_eq!(plan(in_other, in_other + 1, true), Err(Reason::Short));
        let _ = cell_end;
        // A run whose pages stop where the title's memory stops is
        // reached by a root view with no length.
        inplace::views_gone(base + 3 * mib + mib / 2, 2 * K64);
        let p = plan(ask, ask + 1, true).expect("the title's memory ends with the run");
        assert!(p.reaches);
        assert_eq!(p.len, (3 * mib + mib / 2) as u64);
        // A view of the same pages but another order is scattered.
        inplace::views_gone(base, 4 * mib);
        inplace::title_memory_view(base, mib, section, 0);
        inplace::title_memory_view(base + mib, mib, section, 5 * mib as u64);
        inplace::title_memory_view(base + 2 * mib, 2 * mib, section, 2 * mib as u64);
        assert_eq!(plan(base as u64 + mib as u64 - 0x1000, base as u64 + mib as u64 + 0x1000, false), Err(Reason::NotConsecutive));
        let p = plan(base as u64 + 2 * mib as u64, base as u64 + 2 * mib as u64 + 1, true).expect("the last run");
        assert_eq!((p.start, p.len, p.offset), (base as u64 + 2 * mib as u64, 2 * mib as u64, 2 * mib as u64));
        assert!(p.reaches);
        inplace::views_gone(base, 4 * mib);
        // Nothing mapped there now: not a view, and (if the address is
        // free) no memory at all.
        assert!(plan(ask, ask + 1, true).is_err());
    }

    /// An ask over more cells than the longest window: not placed.
    #[test]
    fn a_window_in_place_is_at_most_so_many_cells() {
        let at = 0x7_d000_0000u64;
        assert_eq!(plan(at, at + EXTENT_MOST + 1, false), Err(Reason::TooLong));
        assert_eq!(plan(0x100, 0x200, false), Err(Reason::Undescribed));
    }

    /// Small private memory is a copy, and a committed run of the title's
    /// arena size is a window of the ask's cells.
    #[test]
    fn private_memory_in_place_needs_a_big_committed_run() {
        use mem::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        // SAFETY: allocations of the test's own, released below.
        unsafe {
            let small = VirtualAlloc(std::ptr::null(), 8 << 20, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as u64;
            assert_ne!(small, 0);
            assert_eq!(plan(small + 0x1234, small + 0x2000, false), Err(Reason::SmallAllocation));
            VirtualFree(small as *mut _, 0, mem::MEM_RELEASE);
            let big = VirtualAlloc(std::ptr::null(), (SMALLEST_RUN + (16 << 20)) as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as u64;
            assert_ne!(big, 0);
            let ask = big + (5 << 20) + 0x1234;
            let p = plan(ask, ask + 0x1000, false).expect("a run in an arena");
            assert_eq!(p.kind, Kind::Private);
            let cell = ask & !(CELL - 1);
            assert_eq!(p.start, cell.max(big.next_multiple_of(GRAIN)));
            assert!(p.start <= ask && p.start + p.len >= ask + 0x1000);
            assert_eq!(p.allocation as u64, big);
            // Not committed past the committed part: a root view whose
            // cell goes on past it still reaches the committed end.
            let part = VirtualAlloc(std::ptr::null(), (SMALLEST_RUN + (16 << 20)) as usize, MEM_RESERVE, PAGE_READWRITE) as u64;
            assert_ne!(part, 0);
            // The committed part ends 1 MiB past a cell's start, wherever the
            // system put the reservation: a part that ended within `LEAST` of
            // it would leave the last cell's window too short (a refusal that
            // came with 6 % of the addresses).
            let end = (part + SMALLEST_RUN).next_multiple_of(CELL) + (1 << 20);
            let run = end - part;
            assert!(run <= SMALLEST_RUN + (16 << 20));
            VirtualAlloc(part as *const _, run as usize, MEM_COMMIT, PAGE_READWRITE);
            let ask = end - 0x3000;
            let p = plan(ask, ask + 1, true).unwrap_or_else(|why| panic!("to the end of the committed part: {why:?} (ask {ask:#x}, part {part:#x})"));
            let cell_hi = (ask + 1).next_multiple_of(CELL);
            assert_eq!(p.start + p.len, (end & !(GRAIN - 1)).min(cell_hi));
            // A sized ask that runs past it is not held.
            assert_eq!(plan(ask, ask + 0x10000, false), Err(Reason::NotWhole));
            VirtualFree(part as *mut _, 0, mem::MEM_RELEASE);
            VirtualFree(big as *mut _, 0, mem::MEM_RELEASE);
        }
    }

    /// With the switch on and the host device the software adapter
    /// (`VICTORY_D3D12_WARP=1 VICTORY_UNIFIED=1 cargo test -p victory-gpu
    /// --features diagnostics unified::tests::warp`): an ask in an arena
    /// gets a window in place, every ask in its cells takes it, and the
    /// title's free of the arena goes through once the window is let go.
    #[test]
    fn warp_a_window_in_private_memory_is_placed_and_the_title_can_free_it() {
        use crate::shadow::find;
        use mem::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        if !on_warp() {
            return;
        }
        let Some(host) = crate::host::host() else { return };
        if host.device.cast::<windows::Win32::Graphics::Direct3D12::ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own, released below.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), (SMALLEST_RUN + (16 << 20)) as usize, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
        } as u64;
        assert_ne!(base, 0);
        let ask = base + (20 << 20) + 0x1234;
        let window = find(ask, 1).expect("a window");
        let (placed, bits, _, start, len) = window.in_place_facts();
        assert!(placed, "in place, not a copy");
        assert_eq!(bits & 3, 2, "over private memory");
        assert!(start <= ask && start + len > (ask | (CELL - 1)), "to the end of its cell");
        // Another ask in the cells: the same window.
        let sized = find(ask + 0x4000, 0x2000).expect("a window");
        assert!(std::sync::Arc::ptr_eq(&window, &sized));
        let again = find(ask + 0x100, 1).expect("a window");
        assert!(std::sync::Arc::ptr_eq(&window, &again));
        assert!(crate::shadow::unified_alive()[1][0] >= 1);
        drop((window, sized, again));
        // The title frees the arena: the windows over it go first, then the
        // host takes the call.
        let freed = title_memory_change(base as usize, 0, || {
            // SAFETY: the test's allocation.
            unsafe { VirtualFree(base as *mut _, 0, mem::MEM_RELEASE) }
        });
        assert_ne!(freed, 0, "the host frees the arena once no heap is on it");
        assert!(crate::shadow::unified_alive()[1][0] == 0 || LET_GO[1].load(Ordering::Relaxed) >= 1);
    }

    /// As above over physical pages: a section shown by one view, an ask in
    /// the middle, the window cut to its cells, the same window for every
    /// ask in them, and let go when the runtime says the view changed.
    #[test]
    fn warp_a_window_over_physical_pages_is_placed_through_a_view_of_ours() {
        use crate::shadow::find;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        const SIZE: usize = 16 << 20;
        if !on_warp() {
            return;
        }
        let Some(host) = crate::host::host() else { return };
        if host.device.cast::<windows::Win32::Graphics::Direct3D12::ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: a section and a view of the test's own, released below.
        unsafe {
            let section = mem::CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                mem::PAGE_READWRITE | mem::SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = mem::MapViewOfFile(section, mem::FILE_MAP_WRITE, 0, 0, SIZE).Value as usize;
            assert_ne!(view, 0);
            mem::VirtualAlloc(view as *const _, SIZE, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            inplace::title_memory_view(view, SIZE, section as usize, 0);
            let ask = (view + SIZE / 2 + 0x1234) as u64;
            let window = find(ask, 1).expect("a window");
            let (placed, bits, own_view, start, len) = window.in_place_facts();
            assert!(placed, "in place, not a copy");
            assert_eq!(bits & 3, 1, "over physical pages");
            assert!(start <= ask && start + len > (ask | (CELL - 1)));
            assert!(own_view, "through a view of ours");
            let again = find(ask + 0x8000, 1).expect("a window");
            assert!(std::sync::Arc::ptr_eq(&window, &again));
            // The runtime changes what the view shows there.
            drop(again);
            crate::watch::title_memory_remapped(view + SIZE / 2, K64);
            let found = crate::shadow::unified_alive()[0][0];
            assert!(found == 0 || LET_GO[0].load(Ordering::Relaxed) >= 1, "let go: {found}");
            let next = find(ask, 1).expect("a window");
            assert!(!std::sync::Arc::ptr_eq(&window, &next), "a window made anew");
            drop((window, next));
            inplace::views_gone(view, SIZE);
            // Let go of the windows for good (the heaps are on our views of
            // the section, which outlive this block).
            crate::shadow::in_place_let_go(view as u64, SIZE as u64);
            std::thread::sleep(Duration::from_millis(50));
            mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ });
            CloseHandle(section);
        }
    }

    /// A change of private memory the title makes goes through, with
    /// nothing held, at once; a refusal for any other reason is the answer.
    #[test]
    fn a_change_with_nothing_held_is_the_hosts_answer() {
        let calls = AtomicU64::new(0);
        let n = title_memory_change(0x7_e000_0000, 0x1000, || {
            calls.fetch_add(1, Ordering::Relaxed);
            0
        });
        assert_eq!((n, calls.load(Ordering::Relaxed)), (0, 1));
    }
}

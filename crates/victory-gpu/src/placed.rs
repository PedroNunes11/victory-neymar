//! Host textures the title fills, placed in heaps that many of them share
//! (`VICTORY_PLACED_TEXTURES=1`, live; off by default).
//!
//! A committed texture is a heap of its own, which the system makes and
//! zeroes: ~0.4 ms each on the thread that records the frame (run 215, an
//! RTX 4070 Ti SUPER: 40-118 of them a frame while the world streams in
//! around a walk, 44 ms of a 119 ms pass over textures picked by index). A
//! placed one in a heap that is there already takes a few microseconds.
//! Small ones also start on 4 KiB instead of taking 64 KiB each.
//!
//! The range a texture leaves is given out again once what ran on the card
//! when it went has finished ([`crate::host::retire`]): the sweep lets go of
//! hundreds of textures a second, and with ranges never given out twice (as
//! at first, run 216) one texture left alive held a whole heap: 672 MB of
//! heaps held 51 MB of textures within a minute. A texture in a range given
//! out before holds another's bytes, so the texture that takes such a range
//! is made all zeros on the card first, by a list that runs ahead of the
//! next submission's ([`zero_list`]), as a new heap (and a committed texture)
//! reads before the texture's first upload. Only the textures of the path
//! that hands them to that list ([`reusing`]) take such ranges.
//!
//! What keeps a heap: every texture in it holds it through an object of ours
//! in its private data, which the host releases with the texture's last
//! reference, whoever held that (a descriptor slot, a list, the resource
//! table). A heap with no texture left goes, unless it is the last made of
//! its kind. Ranges given out again come only from heaps at least half full
//! ([`FULL_ENOUGH_PERCENT`]), and fresh ones only from the newest heap, so a
//! heap whose textures go empties and goes (run 219, which filled any heap's
//! holes: heaps held 5.6 GB for 3.0 GB of textures, +2.7 GB of the process's
//! memory). Past [`most_wasted`] bytes of heaps no live texture holds (the
//! newest heaps' fresh tops aside), textures are committed as before until
//! heaps have drained.
//!
//! After the shared heaps of the VIctory-Omega-Fork (commit b1ac3140d,
//! `placed.rs`, by Gooseman; https://github.com/NitricAcidDev/VIctory-Omega-Fork,
//! GPL-3.0), written anew for this tree, with ranges given out again.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, IUnknown, IUnknown_Vtbl, Interface};

/// The size of a heap: 16 MiB, so that a heap empties sooner as its
/// textures go (32 MiB in run 219).
const HEAP: u64 = 16 << 20;
/// How full a heap must be, in percent of it held by live textures, for a
/// range it has free to be given out again ([`make`]); tests lower it.
static FULL_ENOUGH_PERCENT: AtomicU64 = AtomicU64::new(50);
/// The biggest texture placed in one.
const LARGEST: u64 = 4 << 20;
const PAGE: u64 = 4 << 10;
const BLOCK: u64 = 64 << 10;
/// The bytes of the buffer of zeros a texture in a range given out again is
/// copied from, level by level: more than the largest texture's footprint.
const ZEROS: u64 = 2 * LARGEST;

/// A heap of ours, counted in [`HELD`] while it lives.
struct Heap {
    heap: ID3D12Heap,
}

// SAFETY: a heap is named in device calls only, which any thread may make.
unsafe impl Send for Heap {}
// SAFETY: as above.
unsafe impl Sync for Heap {}

impl Drop for Heap {
    fn drop(&mut self) {
        HELD.fetch_sub(HEAP, Ordering::Relaxed);
    }
}

/// Bytes of the heaps alive, and of the textures alive in them.
static HELD: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);

/// Since the start: textures placed, of them in a range given out before,
/// and textures committed instead for the waste.
static PLACED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// For the frame's texture line: the heaps' bytes, their textures' bytes;
/// since the start, textures placed, of them in ranges given out before, and
/// textures committed for the waste.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn counts() -> [u64; 5] {
    [&HELD, &LIVE, &PLACED[0], &PLACED[1], &PLACED[2]].map(|n| n.load(Ordering::Relaxed))
}

/// A heap and what of it is given out: ranges free again, sorted by offset
/// and joined where they touch, the end of what was ever given out, and the
/// bytes of the textures in it.
struct Slab {
    number: u64,
    heap: Arc<Heap>,
    /// 0: textures on 4 KiB, 1: on 64 KiB.
    kind: usize,
    free: Vec<(u64, u64)>,
    top: u64,
    live: u64,
}

impl Slab {
    /// The offset of the first free range that takes `bytes` on `alignment`,
    /// taken out of the free ranges.
    fn take_free(&mut self, bytes: u64, alignment: u64) -> Option<u64> {
        let (n, at) = self.free.iter().enumerate().find_map(|(n, &(start, len))| {
            let at = start.next_multiple_of(alignment);
            (at + bytes <= start + len).then_some((n, at))
        })?;
        let (start, len) = self.free.remove(n);
        // What is left on either side stays free.
        let mut rest = Vec::with_capacity(2);
        if at > start {
            rest.push((start, at - start));
        }
        if at + bytes < start + len {
            rest.push((at + bytes, start + len - at - bytes));
        }
        for (k, range) in rest.into_iter().enumerate() {
            self.free.insert(n + k, range);
        }
        Some(at)
    }

    /// `[at, at + bytes)` is free again. (The top never comes down: what lies
    /// above it was never given out, so it holds the zeros of the new heap;
    /// a range given back holds a texture's bytes and is given out again only
    /// to a texture that is made zeros.)
    fn give_back(&mut self, at: u64, bytes: u64) {
        let n = self.free.partition_point(|&(start, _)| start < at);
        self.free.insert(n, (at, bytes));
        // Joined with the next, then with the one before.
        if n + 1 < self.free.len() && self.free[n].0 + self.free[n].1 == self.free[n + 1].0 {
            self.free[n].1 += self.free[n + 1].1;
            self.free.remove(n + 1);
        }
        if n > 0 && self.free[n - 1].0 + self.free[n - 1].1 == self.free[n].0 {
            self.free[n - 1].1 += self.free[n].1;
            self.free.remove(n);
        }
    }
}

/// The heaps textures are placed in.
static SLABS: Mutex<Vec<Slab>> = Mutex::new(Vec::new());
static SLAB_NUMBERS: AtomicU64 = AtomicU64::new(1);

/// Ranges of textures that went, given back to their heaps outside that
/// texture's own release ([`let_go`]).
static GONE: Mutex<Vec<Range>> = Mutex::new(Vec::new());

/// A range of a heap a texture held, given back when dropped: once what ran
/// on the card when the texture went has finished ([`crate::host::retire`]).
struct Range {
    slab: u64,
    at: u64,
    bytes: u64,
    /// Keeps the heap until the range is back.
    heap: Arc<Heap>,
}

impl Drop for Range {
    fn drop(&mut self) {
        let mut slabs = SLABS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(n) = slabs.iter().position(|s| s.number == self.slab) else {
            return;
        };
        let slab = &mut slabs[n];
        slab.give_back(self.at, self.bytes);
        slab.live = slab.live.saturating_sub(self.bytes);
        // A heap with nothing in it goes, but the last made of its kind.
        let kind = slab.kind;
        let newest = slabs.iter().filter(|s| s.kind == kind).map(|s| s.number).max();
        if slabs[n].live == 0 && newest != Some(self.slab) {
            slabs.remove(n);
        }
        let _ = &self.heap;
    }
}

/// What a placed texture holds through its private data: an `IUnknown` of
/// ours, released when the texture is destroyed.
#[repr(C)]
struct Token {
    vtable: *const IUnknown_Vtbl,
    references: AtomicU32,
    range: Option<Range>,
}

/// Names our private data on a texture.
const TOKEN: GUID = GUID::from_u128(0x7b31_c2e4_5a90_4f6d_8e1b_2c4d_6a8f_0e93);

static TOKEN_VTABLE: IUnknown_Vtbl = IUnknown_Vtbl {
    QueryInterface: query,
    AddRef: add,
    Release: release,
};

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> windows::core::HRESULT {
    // SAFETY: the caller's pointers, as COM has them; `this` is a token.
    unsafe {
        if out.is_null() || iid.is_null() {
            return windows::Win32::Foundation::E_POINTER;
        }
        if *iid == IUnknown::IID {
            add(this);
            *out = this;
            return windows::Win32::Foundation::S_OK;
        }
        *out = std::ptr::null_mut();
    }
    windows::Win32::Foundation::E_NOINTERFACE
}

unsafe extern "system" fn add(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a token someone holds a reference of.
    unsafe { (*this.cast::<Token>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let token = this.cast::<Token>();
    // SAFETY: as above; the last reference frees what `Token::make` made.
    unsafe {
        let left = (*token).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let mut token = Box::from_raw(token);
            // The texture is being destroyed around this: its range goes
            // back afterwards (never under the heaps' lock from here).
            if let Some(range) = token.range.take() {
                LIVE.fetch_sub(range.bytes, Ordering::Relaxed);
                GONE.lock().unwrap_or_else(|e| e.into_inner()).push(range);
            }
        }
        left
    }
}

impl Token {
    fn make(range: Range) -> IUnknown {
        LIVE.fetch_add(range.bytes, Ordering::Relaxed);
        let token = Box::new(Token { vtable: &TOKEN_VTABLE, references: AtomicU32::new(1), range: Some(range) });
        // SAFETY: the object starts with its table of methods and holds the
        // one reference handed over here.
        unsafe { IUnknown::from_raw(Box::into_raw(token).cast()) }
    }
}

/// Whether textures are placed now (live: an A/B may flip it between frames;
/// textures made before keep what they are).
fn on() -> bool {
    static ON: crate::live::Switch = crate::live::Switch::new("PLACED_TEXTURES", 0);
    ON.on()
}

/// The most bytes of heaps no texture uses: a fortieth of the card's budget,
/// between 128 and 384 MiB (`VICTORY_PLACED_WASTE_MB`). The card's memory is
/// charged to the process's commit, which has a hard line on a 32 GB PC.
fn most_wasted() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        if let Some(mb) = std::env::var("VICTORY_PLACED_WASTE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            return mb << 20;
        }
        let budget = crate::host::host().and_then(|h| h.video_memory()).map_or(0, |[budget, ..]| budget);
        (budget / 40).clamp(128 << 20, 384 << 20)
    })
}

/// Gives the ranges of textures that went since the last call back to their
/// heaps, once what runs on the card now has finished. Never called under
/// the heaps' lock.
pub(crate) fn let_go() {
    let gone = std::mem::take(&mut *GONE.lock().unwrap_or_else(|e| e.into_inner()));
    if !gone.is_empty() {
        crate::host::retire(gone);
    }
}

thread_local! {
    /// While [`reusing`] runs on this thread: whether a range given out
    /// before may be taken, and the texture placed in one (to be made zeros).
    static REUSING: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Runs `make`, which may make a host texture through [`create`], letting it
/// take a range given out before; with whether it did (the caller then hands
/// the texture to [`zero_later`] before any list may read it).
pub(crate) fn reusing<T>(make: impl FnOnce() -> T) -> (T, bool) {
    let before = REUSING.replace(Some(false));
    let made = make();
    let reused = REUSING.replace(before) == Some(true);
    (made, reused)
}

/// A texture described by `desc`, in `state`, in a heap with others; `None`
/// if it is not one that can be (a target, depth or unordered access
/// texture, several samples, too big), if the heaps waste too much, or with
/// the switch off: the caller commits it then.
pub(crate) fn create(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    make(device, desc, state)
}

/// [`create`] whatever the switch says.
fn make(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES) -> Option<ID3D12Resource> {
    const WRITTEN: i32 = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0
        | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0
        | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER || desc.Flags.0 & WRITTEN != 0 || desc.SampleDesc.Count > 1 {
        return None;
    }
    let_go();
    let mut desc = *desc;
    let size = |desc: &D3D12_RESOURCE_DESC| {
        // SAFETY: a plain question.
        let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
        (info.SizeInBytes != u64::MAX && info.SizeInBytes != 0).then_some((info.SizeInBytes, info.Alignment))
    };
    // On 4 KiB if the host takes it so (asked only of a texture whose first
    // level fits in 64 KiB: the host complains of the others).
    desc.Alignment = PAGE;
    let small = small_enough(&desc).then(|| size(&desc)).flatten().filter(|&(_, alignment)| alignment == PAGE);
    let (bytes, alignment) = match small {
        Some(found) => found,
        None => {
            desc.Alignment = 0;
            size(&desc).filter(|&(_, alignment)| alignment == BLOCK)?
        }
    };
    if bytes > LARGEST {
        return None;
    }
    let kind = usize::from(alignment == BLOCK);
    // A range given out before only on the path that has it made zeros, and
    // for a texture whose levels the buffer of zeros covers.
    let may_reuse = REUSING.get().is_some() && zeroable(device, &desc);
    let mut slabs = SLABS.lock().unwrap_or_else(|e| e.into_inner());
    // The newest heap of each kind: the only one whose top is given out.
    let newest = |slabs: &[Slab], kind: usize| slabs.iter().filter(|s| s.kind == kind).map(|s| s.number).max();
    let newest_slabs = [newest(&slabs, 0), newest(&slabs, 1)];
    // What no live texture holds but for the newest heaps' tops: past the
    // line (run 219: 2.6 GB of holes, the sweep's textures let go where
    // others live on), textures are committed until heaps have drained.
    let fresh: u64 = slabs.iter().filter(|s| newest_slabs.contains(&Some(s.number))).map(|s| HEAP - s.top).sum();
    let wasted = HELD.load(Ordering::Relaxed).saturating_sub(LIVE.load(Ordering::Relaxed)).saturating_sub(fresh);
    if wasted > most_wasted() {
        PLACED[2].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut found = None;
    if may_reuse {
        // Ranges of heaps that are full enough: a heap below the line gets
        // no new texture, so that it empties as its textures go, and goes.
        let line = HEAP * FULL_ENOUGH_PERCENT.load(Ordering::Relaxed) / 100;
        found = slabs
            .iter_mut()
            .filter(|s| s.kind == kind && s.live >= line)
            .find_map(|s| s.take_free(bytes, alignment).map(|at| (s.number, s.heap.clone(), at, true)));
    }
    if found.is_none() {
        found = slabs.iter_mut().filter(|s| Some(s.number) == newest_slabs[kind]).find_map(|s| {
            let at = s.top.next_multiple_of(alignment);
            (at + bytes <= HEAP).then(|| {
                // The step to the alignment stays free.
                if at > s.top {
                    s.free.push((s.top, at - s.top));
                }
                s.top = at + bytes;
                (s.number, s.heap.clone(), at, false)
            })
        });
    }
    if found.is_none() {
        let heap = Arc::new(make_heap(device)?);
        let number = SLAB_NUMBERS.fetch_add(1, Ordering::Relaxed);
        slabs.push(Slab { number, heap: heap.clone(), kind, free: Vec::new(), top: bytes, live: 0 });
        found = Some((number, heap, 0, false));
    }
    let (number, heap, at, reused) = found?;
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the range lies in the heap and no live texture holds any of it.
    let placed = unsafe { device.CreatePlacedResource(&heap.heap, at, &desc, state, None, &mut made) };
    let slab = slabs.iter_mut().find(|s| s.number == number);
    let made = match (placed, made) {
        (Ok(()), Some(made)) => made,
        (placed, _) => {
            if let Err(e) = placed {
                warn_once(&e);
            }
            if let Some(slab) = slab {
                slab.give_back(at, bytes);
            }
            return None;
        }
    };
    if let Some(slab) = slab {
        slab.live += bytes;
    }
    drop(slabs);
    let token = Token::make(Range { slab: number, at, bytes, heap });
    // SAFETY: a live texture, which takes a reference of the token.
    if unsafe { made.SetPrivateDataInterface(&TOKEN, &token) }.is_err() {
        // Nothing would give the range back: the texture goes (its range
        // with the token below), the caller commits one.
        return None;
    }
    PLACED[0].fetch_add(1, Ordering::Relaxed);
    if reused {
        PLACED[1].fetch_add(1, Ordering::Relaxed);
        REUSING.set(Some(true));
    }
    Some(made)
}

/// Whether a texture's first level fits in 64 KiB, what one that starts on
/// 4 KiB needs.
fn small_enough(desc: &D3D12_RESOURCE_DESC) -> bool {
    let Some(format) = victory_formats::texture::format(desc.Format.0 as u32) else {
        return false;
    };
    let across = |texels: u64| texels.div_ceil(u64::from(format.block.max(1)));
    let layer = across(desc.Width) * across(u64::from(desc.Height)) * u64::from(format.bytes);
    desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && layer <= BLOCK
}

/// The texture's subresources (levels of every layer; one plane) and their
/// footprints, if every one fits in the buffer of zeros.
fn footprints(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Option<Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>> {
    let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(desc.DepthOrArraySize) };
    let count = u32::from(desc.MipLevels.max(1)) * layers;
    let mut layouts = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0u64;
    // SAFETY: the arrays hold `count` entries each.
    unsafe {
        device.GetCopyableFootprints(desc, 0, count, 0, Some(layouts.as_mut_ptr()), Some(rows.as_mut_ptr()), None, Some(&mut total));
    }
    let fits = layouts.iter().all(|l| {
        let f = l.Footprint;
        f.RowPitch > 0 && u64::from(f.RowPitch) * u64::from(f.Height) * u64::from(f.Depth) <= ZEROS
    });
    (total != u64::MAX && fits).then_some(layouts)
}

/// Whether a texture can be made zeros from the buffer of zeros (one plane;
/// every level fits in it).
fn zeroable(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> bool {
    victory_formats::texture::format(desc.Format.0 as u32).is_some_and(|f| !f.stencil) && footprints(device, desc).is_some()
}

/// Records into `list` the copies that make every level of `texture` zeros
/// from `zeros` (a buffer of [`ZEROS`] zero bytes in a state copies read),
/// the texture moved from `state` to a copy's destination and back.
pub(crate) fn record_zeros(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    zeros: &ID3D12Resource,
    texture: &ID3D12Resource,
    state: D3D12_RESOURCE_STATES,
) {
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    let Some(layouts) = footprints(device, &desc) else {
        return;
    };
    let moved = state != D3D12_RESOURCE_STATE_COPY_DEST;
    // SAFETY: an open list; the texture and the buffer live until it has run
    // (the caller keeps them).
    unsafe {
        if moved {
            list.ResourceBarrier(&[crate::host::transition(texture, state, D3D12_RESOURCE_STATE_COPY_DEST)]);
        }
        for (n, layout) in layouts.iter().enumerate() {
            let destination = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: n as u32 },
            };
            let source = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(zeros),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: 0, Footprint: layout.Footprint },
                },
            };
            list.CopyTextureRegion(&destination, 0, 0, 0, &source, None);
        }
        if moved {
            list.ResourceBarrier(&[crate::host::transition(texture, D3D12_RESOURCE_STATE_COPY_DEST, state)]);
        }
    }
}

/// A buffer of [`ZEROS`] zero bytes on `device`, in upload memory the CPU
/// writes the zeros into (a new buffer is not taken to read zeros: on the
/// software adapter one read the bytes of an upload buffer let go just
/// before), in the state upload memory stays in, which copies read from.
pub(crate) fn zero_buffer(device: &ID3D12Device) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: ZEROS,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut made,
            )
            .ok()?;
    }
    let made = made?;
    let mut at = std::ptr::null_mut();
    // SAFETY: the buffer's whole mapping, written once and never again.
    unsafe {
        made.Map(0, None, Some(&mut at)).ok()?;
        at.cast::<u8>().write_bytes(0, ZEROS as usize);
        made.Unmap(0, None);
    }
    Some(made)
}

/// Textures placed in ranges given out before, to be made zeros by the next
/// submission's first list ([`take_zeros`]).
static ZERO: Mutex<Vec<Arc<crate::resource::HostTexture>>> = Mutex::new(Vec::new());

/// `texture` took a range given out before ([`reusing`]): it is made zeros
/// before the lists of the next submission run.
pub(crate) fn zero_later(texture: Arc<crate::resource::HostTexture>) {
    ZERO.lock().unwrap_or_else(|e| e.into_inner()).push(texture);
}

/// The textures waiting to be made zeros, taken: for the submission that
/// records their copies ([`record_zeros`]) ahead of its lists. Under the
/// submission lock, where their tracked states change.
pub(crate) fn take_zeros() -> Vec<Arc<crate::resource::HostTexture>> {
    std::mem::take(&mut *ZERO.lock().unwrap_or_else(|e| e.into_inner()))
}

fn make_heap(device: &ID3D12Device) -> Option<Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: HEAP,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: BLOCK,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain creation.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        warn_once(&e);
    }
    let heap = heap?;
    HELD.fetch_add(HEAP, Ordering::Relaxed);
    Some(Heap { heap })
}

fn warn_once(error: &windows::core::Error) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::warn!(%error, "a texture could not be placed in a shared heap; committed instead"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

    /// The tests here count the heaps of the whole process: one at a time.
    static ONE: Mutex<()> = Mutex::new(());

    fn desc(side: u32, mips: u16, format: DXGI_FORMAT) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(side),
            Height: side,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    /// Ranges given back join their neighbours, and the top stays where it
    /// is (above it the heap was never given out); a range taken leaves what
    /// is around it free.
    #[test]
    fn free_ranges_join_and_split() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        let heap = Arc::new(make_heap(&rig.device).expect("heap"));
        let mut slab = Slab { number: 0, heap, kind: 0, free: Vec::new(), top: 5 * PAGE, live: 0 };
        slab.give_back(PAGE, PAGE);
        slab.give_back(3 * PAGE, PAGE);
        assert_eq!(slab.free, [(PAGE, PAGE), (3 * PAGE, PAGE)]);
        slab.give_back(2 * PAGE, PAGE);
        assert_eq!(slab.free, [(PAGE, 3 * PAGE)]);
        slab.give_back(4 * PAGE, PAGE);
        assert_eq!((slab.free.as_slice(), slab.top), (&[(PAGE, 4 * PAGE)][..], 5 * PAGE));
        slab.free.clear();
        slab.top = 10 * PAGE;
        slab.give_back(2 * PAGE, 6 * PAGE);
        assert_eq!(slab.take_free(PAGE, 4 * PAGE), Some(4 * PAGE));
        assert_eq!(slab.free, [(2 * PAGE, 2 * PAGE), (5 * PAGE, 3 * PAGE)]);
        assert_eq!(slab.take_free(4 * PAGE, PAGE), None);
    }

    /// A device on the software adapter, a queue, and a list waited for.
    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        event: HANDLE,
        submitted: u64,
    }

    impl Rig {
        fn new() -> Option<Rig> {
            // SAFETY: plain object creation on a device of our own.
            unsafe {
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&crate::host::warp_adapter().ok()?, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                let device = device?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Rig { device, queue, allocator, list, fence, event, submitted: 0 })
            }
        }

        /// Records with `record`, runs the list, waits for it and opens it again.
        fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
            record(&self.list);
            self.submitted += 1;
            // SAFETY: our list, queue and fence; the allocator is reset only
            // once the card has run what it held.
            unsafe {
                self.list.Close().unwrap();
                self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence.SetEventOnCompletion(self.submitted, self.event).unwrap();
                WaitForSingleObject(self.event, INFINITE);
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn buffer(&self, kind: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        }

        /// Fills level 0 of `texture` (in the copy destination state) with `byte`.
        fn fill(&mut self, texture: &ID3D12Resource, (side, format): (u32, DXGI_FORMAT), byte: u8) {
            let pitch = 4096u32;
            let bytes = u64::from(pitch) * u64::from(side);
            let upload = self.buffer(D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
            let mut at = std::ptr::null_mut();
            // SAFETY: the mapping holds the buffer's bytes; the list is waited for.
            unsafe {
                upload.Map(0, None, Some(&mut at)).unwrap();
                at.cast::<u8>().write_bytes(byte, bytes as usize);
            }
            self.run(|list| unsafe {
                list.CopyTextureRegion(&level0(texture), 0, 0, 0, &footprint(&upload, side, format, pitch), None)
            });
        }

        /// Level 0 of `texture` (in `state`), rows `rows` of `side * texel` bytes.
        fn read(&mut self, texture: &ID3D12Resource, (side, format): (u32, DXGI_FORMAT), texel: u32, state: D3D12_RESOURCE_STATES, rows: &[u32]) -> Vec<Vec<u8>> {
            let pitch = 4096u32;
            let back = self.buffer(D3D12_HEAP_TYPE_READBACK, u64::from(pitch) * u64::from(side), D3D12_RESOURCE_STATE_COPY_DEST);
            // SAFETY: a list of ours; both resources live.
            self.run(|list| unsafe {
                list.ResourceBarrier(&[crate::host::transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.CopyTextureRegion(&footprint(&back, side, format, pitch), 0, 0, 0, &level0(texture), None);
                list.ResourceBarrier(&[crate::host::transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state)]);
            });
            let mut read = std::ptr::null_mut();
            // SAFETY: the read-back buffer's mapping, read after the list ran.
            unsafe { back.Map(0, None, Some(&mut read)).unwrap() };
            rows.iter()
                .map(|&row| {
                    // SAFETY: inside the mapping (`row` < `side` rows of `pitch` bytes).
                    unsafe { std::slice::from_raw_parts(read.cast::<u8>().add((row * pitch) as usize), (side * texel) as usize) }.to_vec()
                })
                .collect()
        }
    }

    fn footprint(buffer: &ID3D12Resource, side: u32, format: DXGI_FORMAT, pitch: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: format, Width: side, Height: side, Depth: 1, RowPitch: pitch },
                },
            },
        }
    }

    fn level0(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        }
    }

    /// Lets go of what went until the heaps' bytes are at most `most` (once
    /// what other tests' submissions run has finished: `crate::host::retire`).
    fn settle(most: u64) {
        for _ in 0..400 {
            let_go();
            if counts()[0] <= most {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Small and middling textures share heaps, small ones on 4 KiB; each
    /// holds its own texels while the others are written; one never written
    /// reads zeros; half of them going gives no heap back while the rest
    /// live there, all of them going gives back every heap but the last of
    /// each kind.
    #[test]
    fn textures_in_shared_heaps_keep_their_own_texels_and_their_heaps_go_with_them() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        settle(2 * HEAP);
        let before = counts();
        let kinds = [
            (16u32, DXGI_FORMAT_R8G8B8A8_UNORM, 4u32),
            (64, DXGI_FORMAT_R8G8B8A8_UNORM, 4),
            (512, DXGI_FORMAT_R8G8B8A8_UNORM, 4),
            (1024, DXGI_FORMAT_R8_UNORM, 1),
        ];
        const N: usize = 120;
        let textures: Vec<ID3D12Resource> = (0..N)
            .map(|i| {
                let (side, format, _) = kinds[i % kinds.len()];
                make(&rig.device, &desc(side, 1, format), D3D12_RESOURCE_STATE_COPY_DEST).expect("placed")
            })
            .collect();
        let after = counts();
        assert!(after[2] - before[2] >= N as u64, "{after:?} {before:?}");
        assert_eq!(after[3], before[3], "nothing taken from a range given out before outside `reusing`");
        assert!(after[0] >= after[1] && after[1] > before[1], "{after:?}");
        // The 60 small ones take 4 KiB steps, not 64 KiB ones.
        let small: u64 = (0..N)
            .filter(|i| kinds[i % kinds.len()].0 <= 64)
            // SAFETY: a live resource.
            .map(|i| unsafe { rig.device.GetResourceAllocationInfo(0, &[textures[i].GetDesc()]) }.SizeInBytes)
            .sum();
        assert!(small <= 60 * (32 << 10), "{small}");
        // Each texture but every tenth filled with its own number, in an order other than theirs.
        let number = |i: usize| if i.is_multiple_of(10) { 0 } else { i as u8 + 1 };
        for i in (0..N).map(|k| k * 67 % N).filter(|&i| number(i) != 0) {
            let (side, format, _) = kinds[i % kinds.len()];
            rig.fill(&textures[i], (side, format), number(i));
        }
        for (i, texture) in textures.iter().enumerate() {
            let (side, format, texel) = kinds[i % kinds.len()];
            for row in rig.read(texture, (side, format), texel, D3D12_RESOURCE_STATE_COPY_DEST, &[0, side / 2, side - 1]) {
                assert!(row.iter().all(|&b| b == number(i)), "texture {i}: {:?}", &row[..8.min(row.len())]);
            }
        }
        // Every other one goes: their heaps live on in the others.
        let held = counts()[0];
        let textures: Vec<ID3D12Resource> = textures.into_iter().step_by(2).collect();
        settle(0);
        assert_eq!(counts()[0], held, "no heap given back while textures live in it");
        assert!(counts()[1] < after[1]);
        // All of them: every heap goes but the last of each kind.
        drop(textures);
        settle(before[0].max(2 * HEAP));
        assert!(counts()[0] <= before[0].max(2 * HEAP), "{:?} {before:?}", counts());
        assert_eq!(counts()[1], before[1]);
        // SAFETY: a plain query.
        assert!(unsafe { rig.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A range a texture left is given out again, on the path that has it
    /// made zeros only, once its heap has it back; the texture there reads
    /// the bytes the other left until its zeros are copied in, and zeros
    /// after.
    #[test]
    fn a_range_given_out_again_is_made_zeros() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        let shape = (256u32, DXGI_FORMAT_R8G8B8A8_UNORM);
        let d = desc(shape.0, 1, shape.1);
        let state = D3D12_RESOURCE_STATE_COPY_DEST;
        // The only range free will be the one the texture below leaves (the
        // holes other tests left are forgotten), in a heap however empty.
        settle(u64::MAX);
        for slab in SLABS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            slab.free.clear();
        }
        FULL_ENOUGH_PERCENT.store(0, Ordering::Relaxed);
        let old = make(&rig.device, &d, state).expect("placed");
        rig.fill(&old, shape, 0xAB);
        let before = counts();
        drop(old);
        settle(u64::MAX);
        // Outside `reusing`: not in the range given back.
        let fresh = make(&rig.device, &d, state).expect("placed");
        assert_eq!(counts()[3], before[3]);
        // Inside: in it, said so, and reading the old bytes.
        let (again, reused) = reusing(|| make(&rig.device, &d, state));
        FULL_ENOUGH_PERCENT.store(50, Ordering::Relaxed);
        let again = again.expect("placed");
        assert!(reused);
        assert_eq!(counts()[3], before[3] + 1);
        let rows = rig.read(&again, shape, 4, state, &[0, 255]);
        assert!(rows.iter().all(|r| r.iter().all(|&b| b == 0xAB)), "{:?}", &rows[0][..8]);
        // Its zeros copied in from the common state, back there after.
        let zeros = zero_buffer(&rig.device).expect("zeros");
        rig.run(|list| {
            // SAFETY: a list of ours; a live texture.
            unsafe { list.ResourceBarrier(&[crate::host::transition(&again, state, D3D12_RESOURCE_STATE_COMMON)]) };
        });
        let device = rig.device.clone();
        rig.run(|list| record_zeros(&device, list, &zeros, &again, D3D12_RESOURCE_STATE_COMMON));
        let rows = rig.read(&again, shape, 4, D3D12_RESOURCE_STATE_COMMON, &[0, 128, 255]);
        assert!(rows.iter().all(|r| r.iter().all(|&b| b == 0)), "{:?}", &rows[0][..8]);
        drop((fresh, again));
        settle(u64::MAX);
        // SAFETY: a plain query.
        assert!(unsafe { rig.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A heap that is not the newest and has fallen below the line gets no
    /// new texture in its free ranges, and goes when its last texture has.
    #[test]
    fn a_heap_below_the_line_empties_and_goes() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        settle(u64::MAX);
        let d = desc(512, 1, DXGI_FORMAT_R8G8B8A8_UNORM);
        let state = D3D12_RESOURCE_STATE_COPY_DEST;
        let newest = || {
            let slabs = SLABS.lock().unwrap_or_else(|e| e.into_inner());
            slabs.iter().filter(|s| s.kind == 1).map(|s| s.number).max()
        };
        // Textures of 1 MiB until two heaps opened: the textures made while
        // the first was the newest are all of it.
        let (mut opened, mut first, mut theirs, mut others) = (0, None, Vec::new(), Vec::new());
        while opened < 2 {
            let was = newest();
            let texture = make(&rig.device, &d, state).expect("placed");
            let now = newest();
            if now != was {
                opened += 1;
                if opened == 1 {
                    first = now;
                }
            }
            if opened == 1 && now == first {
                theirs.push(texture);
            } else {
                others.push(texture);
            }
        }
        let first = first.expect("a heap");
        let slab = |number| {
            let slabs = SLABS.lock().unwrap_or_else(|e| e.into_inner());
            slabs.iter().find(|s| s.number == number).map(|s| (s.live, s.free.clone()))
        };
        assert_eq!(slab(first).expect("first").0, HEAP, "the first heap is full");
        // Three quarters of it go: below the line.
        let kept = theirs.split_off(theirs.len() / 4 * 3);
        drop(theirs);
        settle(u64::MAX);
        let (live, free) = slab(first).expect("first");
        assert!(live < HEAP / 2 && !free.is_empty(), "{live} {free:?}");
        let reused = counts()[3];
        let (placed, _) = reusing(|| make(&rig.device, &d, state));
        assert!(placed.is_some());
        assert_eq!(slab(first).expect("first"), (live, free), "no texture placed in the heap below the line");
        assert_eq!(counts()[3], reused);
        // The rest go: so does the heap.
        let held = counts()[0];
        drop(kept);
        settle(held - HEAP);
        assert!(slab(first).is_none(), "the empty heap went");
        assert!(counts()[0] <= held - HEAP);
        drop((others, placed));
        settle(u64::MAX);
    }

    /// Targets, depth and unordered access textures and big ones are not
    /// placed; past the waste allowed no heap is made, and then one is again.
    #[test]
    fn what_is_not_placed() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
        let state = D3D12_RESOURCE_STATE_COPY_DEST;
        for flags in [
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        ] {
            let d = D3D12_RESOURCE_DESC { Flags: flags, ..desc(64, 1, DXGI_FORMAT_R8G8B8A8_UNORM) };
            assert!(make(&rig.device, &d, state).is_none(), "{flags:?}");
        }
        assert!(make(&rig.device, &desc(4096, 1, DXGI_FORMAT_R8G8B8A8_UNORM), state).is_none(), "too big");
        // Big enough that the open heaps have no room: a new one is needed.
        let big = desc(1024, 1, DXGI_FORMAT_R8G8B8A8_UNORM);
        let kept: Vec<_> = (0..9).map(|_| make(&rig.device, &big, state).expect("placed")).collect();
        // (As if so much of the heaps were wasted.)
        HELD.fetch_add(4 << 30, Ordering::Relaxed);
        let refused: Vec<_> = (0..9).map(|_| make(&rig.device, &big, state)).collect();
        HELD.fetch_sub(4 << 30, Ordering::Relaxed);
        assert!(refused.iter().any(Option::is_none), "a new heap made past the waste");
        assert!(make(&rig.device, &big, state).is_some());
        // Off by default: the switch says no.
        assert!(create(&rig.device, &big, state).is_none());
        drop((kept, refused));
        settle(u64::MAX);
    }
}

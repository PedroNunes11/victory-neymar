//! Paged buffers (item 2): a buffer of ours whose GPU address never moves
//! and whose 64 KiB pages take memory only while they are used, from a pool
//! of tiles of ours. The first use planned is one big title buffer whose
//! copy lives in the card's memory (`PLANO-ITEM2-POC3.md`: its 174 MiB of
//! the PC's memory beside the card's side go), then the windows of title
//! memory outside buffers (4 MiB each however little of them is read;
//! play3's peak: 5.7 GB of windows over 3.8 GB of pages ever copied).
//!
//! The pieces, none of them called by the game yet:
//! - [`TilePool`]: heaps of ours cut into tiles, in the card's memory or in
//!   the PC's (cached; then each heap has a buffer over all of it that the
//!   CPU maps, and a tile's bytes are written and read there);
//! - [`PagedBuffer`]: a reserved buffer whose pages get tiles of a pool
//!   (`UpdateTileMappings`, on the queue) and give them back, each page
//!   with what is known of it ([`Page`]);
//! - [`Ring`]: mapped memory in 64 KiB slots that come round once the card
//!   has passed the fence of the list that used them, for uploads and
//!   read-backs of pages.
//!
//! Tiles are mapped only into heaps of ours (`CreateHeap`), never into a
//! heap opened on the title's memory: that removes the device
//! (`inplace.rs`). Measured before (the manual test below, commit
//! `eeeb8bc46`): both pools, on the software adapter and on the RTX 4070 Ti
//! SUPER (tiled resources tier 4).

// Until the step that uses them (POC 3 step B) only the tests call them.
#![allow(dead_code)]

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// A tile, and a page of a paged buffer.
pub(crate) const PAGE: u64 = 64 << 10;

/// Where a pool's tiles live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pool {
    /// The card's memory (`D3D12_HEAP_TYPE_DEFAULT`), as levels on demand's
    /// pool (`crate::tiles`): pages are filled and read back by copies.
    Card,
    /// The PC's memory, cached (`CUSTOM`, write-back, L0), as the copies of
    /// title memory in the PC's memory are now (`host.rs` `shared_buffer`):
    /// the CPU writes and reads a tile where it is.
    Pc,
}

/// A tile of a [`TilePool`]: its heap and its place in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tile {
    pub(crate) heap: u32,
    pub(crate) index: u32,
}

struct PoolHeap {
    heap: ID3D12Heap,
    /// Its tiles no page holds.
    free: Vec<u32>,
    /// For [`Pool::Pc`]: a buffer over the whole heap and its mapping.
    mapped: Option<(ID3D12Resource, usize)>,
}

/// Heaps of ours cut into tiles of [`PAGE`] bytes.
pub(crate) struct TilePool {
    kind: Pool,
    /// Tiles a heap.
    heap_tiles: u32,
    heaps: Mutex<Vec<PoolHeap>>,
}

fn buffer_desc(bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

impl TilePool {
    /// A pool whose heaps hold `heap_tiles` tiles each, made as tiles are
    /// asked for.
    pub(crate) fn new(kind: Pool, heap_tiles: u32) -> TilePool {
        TilePool { kind, heap_tiles: heap_tiles.max(1), heaps: Mutex::new(Vec::new()) }
    }

    pub(crate) fn kind(&self) -> Pool {
        self.kind
    }

    /// A heap more, with the CPU's mapping of it for [`Pool::Pc`]. `Err` with
    /// the host's refusal.
    fn grow(&self, device: &ID3D12Device) -> windows::core::Result<PoolHeap> {
        let properties = match self.kind {
            Pool::Card => D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Pool::Pc => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            },
        };
        let bytes = u64::from(self.heap_tiles) * PAGE;
        let desc = D3D12_HEAP_DESC { SizeInBytes: bytes, Properties: properties, Alignment: 0, Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation; the out pointer is ours.
        unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut heap)? };
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        let mapped = match self.kind {
            Pool::Card => None,
            Pool::Pc => {
                let mut placed: Option<ID3D12Resource> = None;
                // SAFETY: a buffer over the whole heap, mapped whole.
                unsafe {
                    device.CreatePlacedResource(&heap, 0, &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)?;
                    let placed = placed.ok_or_else(windows::core::Error::empty)?;
                    let mut data = std::ptr::null_mut();
                    placed.Map(0, None, Some(&mut data))?;
                    Some((placed, data as usize))
                }
            }
        };
        Ok(PoolHeap { heap, free: (0..self.heap_tiles).rev().collect(), mapped })
    }

    /// `n` tiles no page holds, making heaps as needed; `None` (and nothing
    /// taken) if the host will not make another heap.
    pub(crate) fn take(&self, device: &ID3D12Device, n: usize) -> Option<Vec<Tile>> {
        let mut heaps = self.heaps.lock().unwrap_or_else(|e| e.into_inner());
        let free: usize = heaps.iter().map(|h| h.free.len()).sum();
        if free < n {
            let missing = (n - free).div_ceil(self.heap_tiles as usize);
            for _ in 0..missing {
                match self.grow(device) {
                    Ok(heap) => heaps.push(heap),
                    Err(e) => {
                        tracing::warn!(error = %e, kind = ?self.kind, "no heap for the tiles of a paged buffer");
                        return None;
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(n);
        for (i, h) in heaps.iter_mut().enumerate() {
            while out.len() < n {
                let Some(index) = h.free.pop() else { break };
                out.push(Tile { heap: i as u32, index });
            }
        }
        Some(out)
    }

    /// Tiles no page holds any more, and that the card has done with.
    pub(crate) fn give_back(&self, tiles: &[Tile]) {
        let mut heaps = self.heaps.lock().unwrap_or_else(|e| e.into_inner());
        for t in tiles {
            if let Some(h) = heaps.get_mut(t.heap as usize) {
                h.free.push(t.index);
            }
        }
    }

    /// The heap of `tile`.
    fn heap(&self, heap: u32) -> Option<ID3D12Heap> {
        self.heaps.lock().unwrap_or_else(|e| e.into_inner()).get(heap as usize).map(|h| h.heap.clone())
    }

    /// Where the CPU reads and writes `tile` ([`Pool::Pc`] only).
    pub(crate) fn cpu(&self, tile: Tile) -> Option<*mut u8> {
        let heaps = self.heaps.lock().unwrap_or_else(|e| e.into_inner());
        let (_, data) = heaps.get(tile.heap as usize)?.mapped.as_ref()?;
        Some((*data as *mut u8).wrapping_add(tile.index as usize * PAGE as usize))
    }

    /// The bytes of its heaps, and of its tiles that pages hold.
    pub(crate) fn bytes(&self) -> (u64, u64) {
        let heaps = self.heaps.lock().unwrap_or_else(|e| e.into_inner());
        let all = heaps.len() as u64 * u64::from(self.heap_tiles) * PAGE;
        let free: u64 = heaps.iter().map(|h| h.free.len() as u64).sum::<u64>() * PAGE;
        (all, all - free)
    }
}

// SAFETY: D3D12 objects are free-threaded; the mapping is a plain address,
// and a tile's bytes are written by one owner at a time (the paged buffer
// that holds it).
unsafe impl Send for TilePool {}
unsafe impl Sync for TilePool {}

/// What is known of a page of a [`PagedBuffer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Page {
    /// Its tile, if it has one.
    pub(crate) tile: Option<Tile>,
    /// The write-watch asking after which the CPU's bytes were last put in
    /// it (0: never).
    pub(crate) cpu_epoch: u64,
    /// The last submission that declared shader writes to it (0: none).
    pub(crate) gpu_epoch: u64,
    /// The fence value after which the card has done with that submission.
    pub(crate) last_fence: u64,
}

/// A buffer of ours of a fixed GPU address whose pages take tiles of a
/// [`TilePool`] only while they are used. A page without a tile reads as
/// zeros and drops writes (tiled resources tier 2 and up).
pub(crate) struct PagedBuffer {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) gpu: u64,
    pages: Mutex<Vec<Page>>,
}

impl PagedBuffer {
    /// A buffer of `bytes` (rounded up to pages) with no tile yet; `Err` with
    /// the host's refusal.
    pub(crate) fn new(device: &ID3D12Device, bytes: u64) -> windows::core::Result<PagedBuffer> {
        let pages = bytes.div_ceil(PAGE).max(1);
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device.CreateReservedResource(
                &buffer_desc(pages * PAGE, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut out,
            )?;
        }
        let buffer = out.ok_or_else(windows::core::Error::empty)?;
        // SAFETY: a plain query.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Ok(PagedBuffer { buffer, gpu, pages: Mutex::new(vec![Page::default(); pages as usize]) })
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.pages.lock().unwrap_or_else(|e| e.into_inner()).len() as u32
    }

    /// What is known of page `page`.
    pub(crate) fn page(&self, page: u32) -> Option<Page> {
        self.pages.lock().unwrap_or_else(|e| e.into_inner()).get(page as usize).copied()
    }

    /// Changes what is known of page `page`.
    pub(crate) fn note(&self, page: u32, f: impl FnOnce(&mut Page)) {
        if let Some(p) = self.pages.lock().unwrap_or_else(|e| e.into_inner()).get_mut(page as usize) {
            f(p);
        }
    }

    /// Pages with a tile, in order.
    pub(crate) fn resident(&self) -> Vec<u32> {
        let pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        (0..pages.len() as u32).filter(|&p| pages[p as usize].tile.is_some()).collect()
    }

    /// Gives the pages in `wanted` that have no tile one of `pool`'s, mapped
    /// on `queue` in one call ahead of what is submitted next. False (and
    /// nothing mapped) if the pool has no more.
    pub(crate) fn ensure(&self, device: &ID3D12Device, queue: &ID3D12CommandQueue, pool: &TilePool, wanted: &[u32]) -> bool {
        let mut pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        let mut need: Vec<u32> = wanted.iter().copied().filter(|&p| pages.get(p as usize).is_some_and(|q| q.tile.is_none())).collect();
        need.sort_unstable();
        need.dedup();
        if need.is_empty() {
            return true;
        }
        let Some(tiles) = pool.take(device, need.len()) else {
            return false;
        };
        let pairs: Vec<(u32, Option<Tile>)> = need.iter().zip(&tiles).map(|(&p, &t)| (p, Some(t))).collect();
        if !map(queue, &self.buffer, pool, &pairs) {
            pool.give_back(&tiles);
            return false;
        }
        for (&p, &t) in need.iter().zip(&tiles) {
            pages[p as usize].tile = Some(t);
            // A tile holds what its last page left: zeros, as a page no
            // write reached holds (the CPU's pool; the card's is written
            // whole by its first upload). Under the pages' lock: no copy
            // into the page can come before.
            if let Some(at) = pool.cpu(t) {
                // SAFETY: a whole tile of the pool's mapping, held by this page.
                unsafe { std::ptr::write_bytes(at, 0, PAGE as usize) };
            }
        }
        true
    }

    /// Takes the tiles of the pages in `pages` from them (null mappings on
    /// `queue`) and returns them: the caller gives them back to the pool
    /// once the card has done with every list that may read them.
    pub(crate) fn release(&self, queue: &ID3D12CommandQueue, pool: &TilePool, which: &[u32]) -> Vec<Tile> {
        let mut pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        let held: Vec<(u32, Tile)> = which.iter().filter_map(|&p| pages.get(p as usize)?.tile.map(|t| (p, t))).collect();
        if held.is_empty() {
            return Vec::new();
        }
        let pairs: Vec<(u32, Option<Tile>)> = held.iter().map(|&(p, _)| (p, None)).collect();
        map(queue, &self.buffer, pool, &pairs);
        for &(p, _) in &held {
            pages[p as usize] = Page::default();
        }
        held.into_iter().map(|(_, t)| t).collect()
    }

    /// Where the CPU reads and writes page `page` (a [`Pool::Pc`] pool's
    /// tile), if it has one.
    pub(crate) fn cpu(&self, pool: &TilePool, page: u32) -> Option<*mut u8> {
        pool.cpu(self.page(page)?.tile?)
    }
}

/// Maps each page of `pairs` to its tile of `pool` (`None`: no tile), in one
/// `UpdateTileMappings` per heap. False if a heap is missing.
fn map(queue: &ID3D12CommandQueue, buffer: &ID3D12Resource, pool: &TilePool, pairs: &[(u32, Option<Tile>)]) -> bool {
    // By heap (a call maps into one heap); unmappings go with the first.
    let mut heaps: Vec<u32> = pairs.iter().filter_map(|(_, t)| t.map(|t| t.heap)).collect();
    heaps.sort_unstable();
    heaps.dedup();
    if heaps.is_empty() {
        heaps.push(0);
    }
    for (n, &h) in heaps.iter().enumerate() {
        let part: Vec<&(u32, Option<Tile>)> =
            pairs.iter().filter(|(_, t)| t.map_or(n == 0, |t| t.heap == h)).collect();
        if part.is_empty() {
            continue;
        }
        let Some(heap) = pool.heap(h) else {
            // No heap at all (a pool never grown): only unmappings, with
            // nothing to unmap from.
            if part.iter().all(|(_, t)| t.is_none()) {
                continue;
            }
            return false;
        };
        let at: Vec<D3D12_TILED_RESOURCE_COORDINATE> =
            part.iter().map(|&&(page, _)| D3D12_TILED_RESOURCE_COORDINATE { X: page, Y: 0, Z: 0, Subresource: 0 }).collect();
        let sizes = vec![D3D12_TILE_REGION_SIZE { NumTiles: 1, ..Default::default() }; part.len()];
        let flags: Vec<D3D12_TILE_RANGE_FLAGS> =
            part.iter().map(|&&(_, t)| if t.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
        let starts: Vec<u32> = part.iter().map(|&&(_, t)| t.map_or(0, |t| t.index)).collect();
        let counts = vec![1u32; part.len()];
        // SAFETY: a live reserved buffer and a heap of ours; as many regions
        // as ranges, one tile each, in arrays that live through the call.
        unsafe {
            queue.UpdateTileMappings(
                buffer,
                part.len() as u32,
                Some(at.as_ptr()),
                Some(sizes.as_ptr()),
                &heap,
                part.len() as u32,
                Some(flags.as_ptr()),
                Some(starts.as_ptr()),
                Some(counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
    }
    true
}

/// Mapped buffer memory in slots of [`PAGE`] bytes for uploads (an upload
/// heap) or read-backs (a read-back heap). A slot comes round once the card
/// has passed the fence value of the list that used it.
pub(crate) struct Ring {
    pub(crate) buffer: ID3D12Resource,
    data: usize,
    slots: Slots,
}

/// The slots of a [`Ring`]: the fence value each waits for (0: free), and
/// the next one to hand out.
struct Slots {
    fences: Vec<u64>,
    next: usize,
}

impl Slots {
    fn new(n: usize) -> Slots {
        Slots { fences: vec![0; n.max(1)], next: 0 }
    }

    /// The next slot, if the card has passed its fence (`completed`), now
    /// held until `fence`.
    fn take(&mut self, completed: u64, fence: u64) -> Option<usize> {
        let i = self.next;
        if self.fences[i] > completed {
            return None;
        }
        self.fences[i] = fence;
        self.next = (i + 1) % self.fences.len();
        Some(i)
    }
}

// SAFETY: a D3D12 object and the address of its mapping; one owner at a
// time (behind a lock where it is shared).
unsafe impl Send for Ring {}

impl Ring {
    /// A ring of `slots` slots in an upload heap (`up`) or a read-back heap;
    /// `Err` with the host's refusal.
    pub(crate) fn new(device: &ID3D12Device, up: bool, slots: u32) -> windows::core::Result<Ring> {
        let heap = D3D12_HEAP_PROPERTIES { Type: if up { D3D12_HEAP_TYPE_UPLOAD } else { D3D12_HEAP_TYPE_READBACK }, ..Default::default() };
        let state = if up { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COPY_DEST };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain buffer creation; a mapping of the whole buffer.
        unsafe {
            device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(u64::from(slots.max(1)) * PAGE, D3D12_RESOURCE_FLAG_NONE), state, None, &mut out)?;
            let buffer = out.ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            buffer.Map(0, None, Some(&mut data))?;
            Ok(Ring { buffer, data: data as usize, slots: Slots::new(slots as usize) })
        }
    }

    pub(crate) fn slots(&self) -> usize {
        self.slots.fences.len()
    }

    /// The next slot if the card has passed its fence (`completed`), taken
    /// for a list the card will have done with at `fence`: its offset in
    /// the ring's buffer and where the CPU writes or reads it. `None` while
    /// the card has not.
    pub(crate) fn take(&mut self, completed: u64, fence: u64) -> Option<(u64, *mut u8)> {
        let i = self.slots.take(completed, fence)?;
        let at = i as u64 * PAGE;
        Some((at, (self.data as *mut u8).wrapping_add(at as usize)))
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{Interface, s};

    use super::*;
    use crate::host::{compile, transition, warp_adapter};

    /// Pages of the paged buffer (4 MiB, a window), and tiles a heap: two
    /// heaps for the 40 pages mapped at once, so that a call maps into both.
    const PAGES: u32 = 64;
    const HEAP_TILES: u32 = 32;
    /// Slots of the upload and read-back rings.
    const RING: u32 = 8;
    /// Dwords a page.
    const DWORDS: usize = (PAGE / 4) as usize;

    /// Reads and writes 64 dwords of each page named in `pages` (one group a
    /// page), marks the page, and reads one page named after the pages
    /// (one without a tile) into the marks.
    const TOUCH: &str = r#"
#define RS "SRV(t0), UAV(u0), UAV(u1)"
ByteAddressBuffer pages : register(t0);
RWByteAddressBuffer data : register(u0);
RWByteAddressBuffer marks : register(u1);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void touch(uint3 group : SV_GroupID, uint thread : SV_GroupThreadID) {
    uint count = pages.Load(0);
    if (group.x == count) {
        if (thread == 0) {
            uint empty = pages.Load(4 + 4 * count);
            marks.Store(4 * 64, data.Load(empty * 65536 + 4096));
        }
        return;
    }
    uint page = pages.Load(4 + 4 * group.x);
    uint at = page * 65536 + thread * 1024;
    data.Store(at, data.Load(at) ^ 0xA5A5A5A5);
    if (thread == 0) {
        marks.Store(4 * page, 1);
    }
}
"#;

    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        event: HANDLE,
        submitted: u64,
        step: String,
        card: bool,
    }

    impl Rig {
        /// The software adapter, or the default one with
        /// `VICTORY_PAGED_TEST_CARD=1`; `None` without one or without tiled
        /// resources of tier 2 (a page without a tile reads zeros).
        fn new() -> Option<Rig> {
            // SAFETY: plain object creation on a device of our own.
            unsafe {
                let card = std::env::var_os("VICTORY_PAGED_TEST_CARD").is_some_and(|v| v == "1");
                let mut device: Option<ID3D12Device> = None;
                if card {
                    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                } else {
                    D3D12CreateDevice(&warp_adapter().ok()?, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                }
                let device = device?;
                let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
                device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32).ok()?;
                eprintln!(
                    "paged buffers, {}: tiled resources tier {}, resource heap tier {}",
                    if card { "ON THE CARD" } else { "on the software adapter" },
                    options.TiledResourcesTier.0,
                    options.ResourceHeapTier.0
                );
                if options.TiledResourcesTier.0 < 2 {
                    return None;
                }
                let queue: ID3D12CommandQueue =
                    device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).ok()?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Rig { device, queue, allocator, list, fence, event, submitted: 0, step: "making the device".into(), card })
            }
        }

        /// Stops the test, naming the step, if the device is lost.
        fn check(&self, what: &str) {
            // SAFETY: a plain query.
            let removed = unsafe { self.device.GetDeviceRemovedReason() };
            assert!(removed.is_ok(), "{}, {what}: the device is lost: {removed:?}", self.step);
        }

        /// The fence value the next submission signals.
        fn next_fence(&self) -> u64 {
            self.submitted + 1
        }

        /// What the card has finished.
        fn completed(&self) -> u64 {
            // SAFETY: a plain query.
            unsafe { self.fence.GetCompletedValue() }
        }

        /// Records with `record`, runs the list behind what is on the queue
        /// (tile mappings among it), waits for it (never long: 1.5 s on the
        /// card, under Windows' own two) and opens the list again.
        fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
            record(&self.list);
            self.submitted += 1;
            // SAFETY: our list, queue and fence; the allocator is reset only
            // after the card finished everything recorded from it.
            unsafe {
                self.list.Close().unwrap();
                let run: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(run)]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence.SetEventOnCompletion(self.submitted, self.event).unwrap();
                let limit = if self.card { 1500 } else { 20_000 };
                let waited = WaitForSingleObject(self.event, limit);
                self.check("a submission");
                assert!(waited == WAIT_OBJECT_0, "{}: a submission did not finish within {limit} ms", self.step);
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn shared(&self, bytes: u64) -> (ID3D12Resource, *mut u8) {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain buffer creation; a mapping of the whole buffer.
            unsafe {
                self.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(bytes, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS), D3D12_RESOURCE_STATE_COMMON, None, &mut out)
                    .unwrap();
                let buffer = out.unwrap();
                let mut data = std::ptr::null_mut();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                (buffer, data.cast())
            }
        }
    }

    /// What a page holds before the shader: dword j of page p.
    fn word(p: u32, j: u32, round: u32) -> u32 {
        (round << 28) ^ (p << 16) ^ j
    }

    /// The dwords of the page at `data` (mapped memory of the test's own).
    fn dwords<'a>(data: *mut u8) -> &'a mut [u32] {
        // SAFETY: a page of mapped memory of the test's own, alive to the end
        // of the test, touched by one thread.
        unsafe { std::slice::from_raw_parts_mut(data.cast::<u32>(), DWORDS) }
    }

    /// One pool: map scattered pages, fill them, let a shader read, write
    /// and mark some, read back only the marked ones, then let tiles go and
    /// give them to other pages; the buffer's GPU address never moves.
    fn one_pool(rig: &mut Rig, kind: Pool) -> Result<(), String> {
        rig.step = format!("{kind:?}: the paged buffer");
        let pool = TilePool::new(kind, HEAP_TILES);
        let paged = PagedBuffer::new(&rig.device, u64::from(PAGES) * PAGE).map_err(|e| format!("the reserved buffer was refused: {e}"))?;
        let address = paged.gpu;
        let cs = compile(TOUCH, s!("touch"), s!("cs_5_1")).map_err(|e| format!("no shader: {e}"))?;
        // SAFETY: a root signature and pipeline from the shader just made.
        let (root, pipeline) = unsafe {
            let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, &cs[..]).map_err(|e| e.to_string())?;
            let pipeline: ID3D12PipelineState = rig
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs.as_ptr().cast(), BytecodeLength: cs.len() },
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            (root, pipeline)
        };
        let mut up = Ring::new(&rig.device, true, RING).map_err(|e| e.to_string())?;
        let mut back = Ring::new(&rig.device, false, RING).map_err(|e| e.to_string())?;
        let (marks, marked) = rig.shared(4 * 72);
        let (names, named) = rig.shared(4 * 72);
        let buffer = paged.buffer.clone();

        // Puts `round`'s bytes into `pages`: by the CPU in the PC's pool, by
        // copies from the upload ring in the card's (a slot a page, the
        // ring going round as the card passes its fences).
        let fill = |rig: &mut Rig, up: &mut Ring, pages: &[u32], round: u32| -> Result<(), String> {
            if kind == Pool::Pc {
                for &p in pages {
                    let at = paged.cpu(&pool, p).ok_or(format!("page {p} has no tile the CPU can reach"))?;
                    for (j, w) in dwords(at).iter_mut().enumerate() {
                        *w = word(p, j as u32, round);
                    }
                }
                return Ok(());
            }
            for chunk in pages.chunks(up.slots()) {
                let fence = rig.next_fence();
                let mut slots = Vec::new();
                for &p in chunk {
                    let (at, data) = up.take(rig.completed(), fence).ok_or("the upload ring did not come round")?;
                    for (j, w) in dwords(data).iter_mut().enumerate() {
                        *w = word(p, j as u32, round);
                    }
                    slots.push((p, at));
                }
                let ring = up.buffer.clone();
                rig.run(|list| unsafe {
                    list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
                    for &(p, at) in &slots {
                        list.CopyBufferRegion(&buffer, u64::from(p) * PAGE, &ring, at, PAGE);
                    }
                    list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
                });
            }
            Ok(())
        };
        // Reads pages back, only the ones asked: dwords by page.
        let read = |rig: &mut Rig, back: &mut Ring, pages: &[u32]| -> Result<Vec<Vec<u32>>, String> {
            let mut out = Vec::new();
            if kind == Pool::Pc {
                for &p in pages {
                    out.push(dwords(paged.cpu(&pool, p).ok_or(format!("page {p} has no tile"))?).to_vec());
                }
                return Ok(out);
            }
            for chunk in pages.chunks(back.slots()) {
                let fence = rig.next_fence();
                let mut slots = Vec::new();
                for &p in chunk {
                    let (at, data) = back.take(rig.completed(), fence).ok_or("the read-back ring did not come round")?;
                    slots.push((p, at, data as usize));
                }
                let ring = back.buffer.clone();
                rig.run(|list| unsafe {
                    list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    for &(p, at, _) in &slots {
                        list.CopyBufferRegion(&ring, at, &buffer, u64::from(p) * PAGE, PAGE);
                    }
                    list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                });
                for &(_, _, data) in &slots {
                    out.push(dwords(data as *mut u8).to_vec());
                }
            }
            Ok(out)
        };
        // The shader over `pages`, then a read of `empty` (a page with no
        // tile); returns the pages marked and what the empty page read.
        let touch = |rig: &mut Rig, pages: &[u32], empty: u32| -> (Vec<u32>, u32) {
            // SAFETY: mapped buffers of the test's own, 72 dwords each.
            unsafe {
                let m = std::slice::from_raw_parts_mut(marked.cast::<u32>(), 72);
                m.fill(0xFFFF_FFFF);
                m[..64].fill(0);
                let n = std::slice::from_raw_parts_mut(named.cast::<u32>(), 72);
                n[0] = pages.len() as u32;
                n[1..=pages.len()].copy_from_slice(pages);
                n[1 + pages.len()] = empty;
            }
            rig.run(|list| unsafe {
                list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetComputeRootShaderResourceView(0, names.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(1, buffer.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(2, marks.GetGPUVirtualAddress());
                list.Dispatch(pages.len() as u32 + 1, 1, 1);
                list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON)]);
            });
            // SAFETY: as above, after the card finished.
            let m = unsafe { std::slice::from_raw_parts(marked.cast::<u32>(), 72) };
            ((0..64).filter(|&p| m[p as usize] == 1).collect(), m[64])
        };
        let check_pages = |what: &str, pages: &[u32], got: &[Vec<u32>], touched: &[u32], round: u32| -> Result<(), String> {
            for (&p, words) in pages.iter().zip(got) {
                for (j, &w) in words.iter().enumerate() {
                    let j = j as u32;
                    let mut want = word(p, j, round);
                    if touched.contains(&p) && j % 256 == 0 {
                        want ^= 0xA5A5_A5A5;
                    }
                    if w != want {
                        return Err(format!("{what}: page {p} dword {j} reads {w:#x}, wanted {want:#x}"));
                    }
                }
            }
            Ok(())
        };

        // 1. 40 scattered pages of 64 get tiles (two heaps of 32).
        rig.step = format!("{kind:?}: 40 scattered pages mapped");
        let first: Vec<u32> = (0..40).map(|i| (i * 7) % PAGES).collect();
        if !paged.ensure(&rig.device, &rig.queue, &pool, &first) {
            return Err("the pool gave no tiles (no heap could be made)".into());
        }
        rig.check("tiles mapped");
        if paged.resident().len() != 40 || pool.bytes() != (2 * u64::from(HEAP_TILES) * PAGE, 40 * PAGE) {
            return Err(format!("after mapping: {} pages resident, the pool's bytes {:?}", paged.resident().len(), pool.bytes()));
        }
        rig.step = format!("{kind:?}: the pages filled");
        fill(rig, &mut up, &first, 1)?;
        // 2. A shader touches 20 of them; an unmapped page reads zeros.
        rig.step = format!("{kind:?}: the shader over 20 pages");
        let touched: Vec<u32> = first[..20].to_vec();
        let empty = (0..PAGES).find(|p| !first.contains(p)).unwrap();
        let (marked_pages, empty_read) = touch(rig, &touched, empty);
        let mut want_marked = touched.clone();
        want_marked.sort_unstable();
        if marked_pages != want_marked {
            return Err(format!("the pages marked are {marked_pages:?}, wanted {want_marked:?}"));
        }
        if empty_read != 0 {
            return Err(format!("a page with no tile reads {empty_read:#x}, not zero"));
        }
        // 3. Only the marked pages come back, and the rest is as filled.
        rig.step = format!("{kind:?}: the marked pages read back");
        let got = read(rig, &mut back, &marked_pages)?;
        check_pages("marked pages", &marked_pages, &got, &touched, 1)?;
        let rest: Vec<u32> = first[20..24].to_vec();
        let got = read(rig, &mut back, &rest)?;
        check_pages("untouched pages", &rest, &got, &[], 1)?;
        // 4. Ten touched pages let their tiles go; ten new pages take them.
        rig.step = format!("{kind:?}: tiles given to other pages");
        let gone: Vec<u32> = touched[..10].to_vec();
        let freed = paged.release(&rig.queue, &pool, &gone);
        if freed.len() != 10 {
            return Err(format!("{} tiles let go, wanted 10", freed.len()));
        }
        // Back to the pool once the card has done with them (it is idle).
        pool.give_back(&freed);
        let new: Vec<u32> = (0..PAGES).filter(|p| !first.contains(p)).take(10).collect();
        if !paged.ensure(&rig.device, &rig.queue, &pool, &new) {
            return Err("the pool gave no tiles for the new pages".into());
        }
        rig.check("tiles moved");
        if pool.bytes() != (2 * u64::from(HEAP_TILES) * PAGE, 40 * PAGE) {
            return Err(format!("the tiles were not reused: the pool's bytes {:?}", pool.bytes()));
        }
        fill(rig, &mut up, &new, 2)?;
        let (marked_pages, empty_read) = touch(rig, &new, gone[0]);
        let mut want_marked = new.clone();
        want_marked.sort_unstable();
        if marked_pages != want_marked {
            return Err(format!("after the move the pages marked are {marked_pages:?}, wanted {want_marked:?}"));
        }
        if empty_read != 0 {
            return Err(format!("a page whose tile went reads {empty_read:#x}, not zero"));
        }
        let got = read(rig, &mut back, &new)?;
        check_pages("pages on reused tiles", &new, &got, &new, 2)?;
        // SAFETY: a plain query.
        let moved = unsafe { paged.buffer.GetGPUVirtualAddress() };
        if moved != address {
            return Err(format!("the buffer's GPU address moved: {address:#x} -> {moved:#x}"));
        }
        rig.check("the end of the pool");
        Ok(())
    }

    /// Run by name, alone, no game needed:
    /// `cargo test -p victory-gpu --lib paged -- --ignored --nocapture`.
    /// On the SOFTWARE adapter unless `VICTORY_PAGED_TEST_CARD=1`; on the
    /// card only the pool in the card's memory runs unless
    /// `VICTORY_PAGED_TEST_PC_POOL=1` too.
    #[test]
    #[ignore = "manual: run it by name with --ignored --nocapture"]
    fn paged_buffer_pages_take_tiles_only_while_used() {
        let Some(mut rig) = Rig::new() else {
            eprintln!("no adapter with tiled resources of tier 2; skipped");
            return;
        };
        let mut pools = vec![Pool::Card];
        if !rig.card || std::env::var_os("VICTORY_PAGED_TEST_PC_POOL").is_some_and(|v| v == "1") {
            pools.push(Pool::Pc);
        }
        let mut failed = Vec::new();
        for kind in pools {
            match one_pool(&mut rig, kind) {
                Ok(()) => eprintln!("{kind:?}: every step passed {}", if rig.card { "ON THE CARD" } else { "on the software adapter" }),
                Err(why) => {
                    eprintln!("{kind:?}: {why} ({})", rig.step);
                    failed.push(kind);
                }
            }
        }
        assert!(failed.is_empty(), "pools that failed: {failed:?}");
    }

    /// A ring hands out its slots in turn and gives none back before the
    /// card has passed the fence of the list that used it.
    #[test]
    fn a_ring_slot_comes_round_only_past_its_fence() {
        let mut slots = Slots::new(3);
        assert_eq!(slots.take(0, 1), Some(0));
        assert_eq!(slots.take(0, 1), Some(1));
        assert_eq!(slots.take(0, 2), Some(2));
        assert_eq!(slots.take(0, 3), None, "slot 0 waits for fence 1");
        assert_eq!(slots.take(1, 3), Some(0));
        assert_eq!(slots.take(1, 4), Some(1), "slot 1 waited for fence 1 too");
        assert_eq!(slots.take(1, 4), None, "slot 2 waits for fence 2");
        assert_eq!(slots.take(2, 4), Some(2));
    }
}

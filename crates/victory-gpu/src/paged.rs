//! Paged buffers (item 2, proof of concept 2): a buffer of ours whose GPU
//! address never moves and whose 64 KiB pages take memory only while they
//! are used, from a pool of tiles of ours. The goal is the windows of title
//! memory outside buffers (`crate::shadow`): 4 MiB each whatever is read of
//! them (play3's peak: 5.7 GB of windows over 3.8 GB of pages ever copied).
//!
//! Nothing here runs in the game yet: only the manual test below, which says
//! what the host gives. Tiles are mapped only into heaps of ours (`CreateHeap`),
//! never into a heap opened on the title's memory: that removes the device
//! (`inplace.rs`).

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{Interface, s};

    use crate::host::{compile, transition, warp_adapter};

    /// A tile: the pages of a paged buffer.
    const PAGE: u64 = 64 << 10;
    /// Pages of the paged buffer (4 MiB, a window) and tiles of the pool.
    const PAGES: u32 = 64;
    /// Pages the upload and read-back rings hold at once.
    const RING: u32 = 8;
    /// Dwords a page.
    const DWORDS: u32 = (PAGE / 4) as u32;

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

    /// Where the pool's tiles live.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Pool {
        /// The card's memory (`D3D12_HEAP_TYPE_DEFAULT`), as levels on
        /// demand's pool (`crate::tiles`): pages are filled and read back
        /// through rings, by copies.
        Card,
        /// The PC's memory, cached (`CUSTOM`, write-back, L0), as the
        /// windows' host copies are now (`host.rs` `shared_buffer`): a
        /// placed buffer over the whole heap is mapped, and the CPU writes
        /// and reads the tiles there.
        Pc,
    }

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
                device
                    .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32)
                    .ok()?;
                eprintln!(
                    "paged buffers, {}: tiled resources tier {}, resource heap tier {}",
                    if card { "ON THE CARD" } else { "on the software adapter" },
                    options.TiledResourcesTier.0,
                    options.ResourceHeapTier.0
                );
                if options.TiledResourcesTier.0 < 2 {
                    return None;
                }
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                    .ok()?;
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

        fn desc(bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
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

        /// A committed buffer of `bytes` in `heap`, and its mapping when the
        /// CPU may map it.
        fn buffer(&self, heap: D3D12_HEAP_PROPERTIES, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES, bytes: u64) -> (ID3D12Resource, *mut u8) {
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain buffer creation; a mapping of the whole buffer.
            unsafe {
                self.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &Self::desc(bytes, flags), state, None, &mut out).unwrap();
                let buffer = out.unwrap();
                let mut data = std::ptr::null_mut();
                if heap.Type != D3D12_HEAP_TYPE_DEFAULT {
                    buffer.Map(0, None, Some(&mut data)).unwrap();
                }
                (buffer, data.cast())
            }
        }

        /// The pool's heap of `PAGES` tiles and, in the PC's memory, a
        /// placed buffer over all of it, mapped. `Err` with the host's
        /// refusal.
        fn pool(&self, pool: Pool) -> windows::core::Result<(ID3D12Heap, Option<(ID3D12Resource, *mut u8)>)> {
            let properties = match pool {
                Pool::Card => D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                Pool::Pc => D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_CUSTOM,
                    CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                    MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                    ..Default::default()
                },
            };
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: u64::from(PAGES) * PAGE,
                Properties: properties,
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
            };
            let mut heap: Option<ID3D12Heap> = None;
            // SAFETY: plain creation.
            unsafe { self.device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut heap)? };
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            if pool == Pool::Card {
                return Ok((heap, None));
            }
            let mut placed: Option<ID3D12Resource> = None;
            // SAFETY: a buffer over the whole heap, mapped whole.
            unsafe {
                self.device.CreatePlacedResource(
                    &heap,
                    0,
                    &Self::desc(u64::from(PAGES) * PAGE, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut placed,
                )?;
                let placed = placed.ok_or_else(windows::core::Error::empty)?;
                let mut data = std::ptr::null_mut();
                placed.Map(0, None, Some(&mut data))?;
                Ok((heap, Some((placed, data.cast()))))
            }
        }

        /// A buffer of `PAGES` pages with no memory of its own.
        fn reserved(&self) -> windows::core::Result<ID3D12Resource> {
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                self.device.CreateReservedResource(
                    &Self::desc(u64::from(PAGES) * PAGE, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut out,
                )?;
            }
            out.ok_or_else(windows::core::Error::empty)
        }

        /// Maps page `page` of `buffer` to tile `tile` of `heap` for each
        /// pair (`None`: the page has no tile any more), on the queue, in one
        /// call, ahead of what is submitted next.
        fn map(&self, buffer: &ID3D12Resource, heap: &ID3D12Heap, pairs: &[(u32, Option<u32>)]) {
            let at: Vec<D3D12_TILED_RESOURCE_COORDINATE> =
                pairs.iter().map(|&(page, _)| D3D12_TILED_RESOURCE_COORDINATE { X: page, Y: 0, Z: 0, Subresource: 0 }).collect();
            let sizes = vec![D3D12_TILE_REGION_SIZE { NumTiles: 1, ..Default::default() }; pairs.len()];
            let flags: Vec<D3D12_TILE_RANGE_FLAGS> =
                pairs.iter().map(|&(_, tile)| if tile.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
            let starts: Vec<u32> = pairs.iter().map(|&(_, tile)| tile.unwrap_or(0)).collect();
            let counts = vec![1u32; pairs.len()];
            // SAFETY: a live reserved buffer and a heap of ours; as many
            // regions as ranges, one tile each, in arrays that live through
            // the call.
            unsafe {
                self.queue.UpdateTileMappings(
                    buffer,
                    pairs.len() as u32,
                    Some(at.as_ptr()),
                    Some(sizes.as_ptr()),
                    heap,
                    pairs.len() as u32,
                    Some(flags.as_ptr()),
                    Some(starts.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
            self.check("tiles mapped");
        }
    }

    /// What a page holds before the shader: dword j of page p.
    fn word(p: u32, j: u32, round: u32) -> u32 {
        (round << 28) ^ (p << 16) ^ j
    }

    /// One pool: map scattered pages, fill them, let a shader read, write
    /// and mark some, read back only the marked ones, then let tiles go and
    /// give them to other pages; the buffer's GPU address never moves.
    fn one_pool(rig: &mut Rig, pool: Pool) -> Result<(), String> {
        rig.step = format!("{pool:?}: the pool's heap");
        let (heap, mapped) = rig.pool(pool).map_err(|e| format!("the heap or its mapped buffer was refused: {e}"))?;
        rig.check("the pool");
        rig.step = format!("{pool:?}: the paged buffer");
        let buffer = rig.reserved().map_err(|e| format!("the reserved buffer was refused: {e}"))?;
        // SAFETY: a plain query.
        let address = unsafe { buffer.GetGPUVirtualAddress() };
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
        let upload = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() };
        let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let shared = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let (ring_up, up) = rig.buffer(upload, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ, u64::from(RING) * PAGE);
        let (ring_back, back) = rig.buffer(readback, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, u64::from(RING) * PAGE);
        let (marks, marked) = rig.buffer(shared, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, 4 * 72);
        let (names, named) = rig.buffer(upload, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ, 4 * 72);
        /// The dwords of the page at byte `at` of a mapped buffer of the
        /// test's own.
        fn dwords<'a>(data: *mut u8, at: u64) -> &'a mut [u32] {
            // SAFETY: a page inside a mapped buffer of the test's own, alive
            // to the end of the test, touched by one thread.
            unsafe { std::slice::from_raw_parts_mut(data.add(at as usize).cast::<u32>(), DWORDS as usize) }
        }
        // Where each page's tile is, to reach it through the mapped heap.
        let mut tile_of: [Option<u32>; PAGES as usize] = [None; PAGES as usize];
        let fill = |rig: &mut Rig, tile_of: &[Option<u32>; PAGES as usize], pages: &[u32], round: u32| {
            match &mapped {
                Some((_, heap_data)) => {
                    for &p in pages {
                        let t = tile_of[p as usize].unwrap();
                        for (j, w) in dwords(*heap_data, u64::from(t) * PAGE).iter_mut().enumerate() {
                            *w = word(p, j as u32, round);
                        }
                    }
                }
                None => {
                    for chunk in pages.chunks(RING as usize) {
                        for (slot, &p) in chunk.iter().enumerate() {
                            for (j, w) in dwords(up, slot as u64 * PAGE).iter_mut().enumerate() {
                                *w = word(p, j as u32, round);
                            }
                        }
                        rig.run(|list| unsafe {
                            list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
                            for (slot, &p) in chunk.iter().enumerate() {
                                list.CopyBufferRegion(&buffer, u64::from(p) * PAGE, &ring_up, slot as u64 * PAGE, PAGE);
                            }
                            list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
                        });
                    }
                }
            }
        };
        // Reads pages back (only the ones asked), dword lists by page.
        let read = |rig: &mut Rig, tile_of: &[Option<u32>; PAGES as usize], pages: &[u32]| -> Vec<Vec<u32>> {
            let mut out = Vec::new();
            match &mapped {
                Some((_, heap_data)) => {
                    for &p in pages {
                        out.push(dwords(*heap_data, u64::from(tile_of[p as usize].unwrap()) * PAGE).to_vec());
                    }
                }
                None => {
                    for chunk in pages.chunks(RING as usize) {
                        rig.run(|list| unsafe {
                            list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                            for (slot, &p) in chunk.iter().enumerate() {
                                list.CopyBufferRegion(&ring_back, slot as u64 * PAGE, &buffer, u64::from(p) * PAGE, PAGE);
                            }
                            list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                        });
                        for slot in 0..chunk.len() {
                            out.push(dwords(back, slot as u64 * PAGE).to_vec());
                        }
                    }
                }
            }
            out
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

        // 1. 40 scattered pages of 64 on tiles 0..40.
        rig.step = format!("{pool:?}: 40 scattered pages mapped");
        let first: Vec<u32> = (0..40).map(|i| (i * 7) % PAGES).collect();
        let pairs: Vec<(u32, Option<u32>)> = first.iter().enumerate().map(|(t, &p)| (p, Some(t as u32))).collect();
        rig.map(&buffer, &heap, &pairs);
        for &(p, t) in &pairs {
            tile_of[p as usize] = t;
        }
        rig.step = format!("{pool:?}: the pages filled");
        fill(rig, &tile_of, &first, 1);
        // 2. A shader touches 20 of them; an unmapped page reads zeros.
        rig.step = format!("{pool:?}: the shader over 20 pages");
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
        rig.step = format!("{pool:?}: the marked pages read back");
        let got = read(rig, &tile_of, &marked_pages);
        check_pages("marked pages", &marked_pages, &got, &touched, 1)?;
        let rest: Vec<u32> = first[20..24].to_vec();
        let got = read(rig, &tile_of, &rest);
        check_pages("untouched pages", &rest, &got, &[], 1)?;
        // 4. Ten touched pages let their tiles go; ten new pages take them.
        rig.step = format!("{pool:?}: tiles given to other pages");
        let gone: Vec<u32> = touched[..10].to_vec();
        let new: Vec<u32> = (0..PAGES).filter(|p| !first.contains(p)).take(10).collect();
        let mut pairs: Vec<(u32, Option<u32>)> = gone.iter().map(|&p| (p, None)).collect();
        for (&p, &g) in new.iter().zip(&gone) {
            pairs.push((p, tile_of[g as usize]));
        }
        rig.map(&buffer, &heap, &pairs);
        for &(p, t) in &pairs {
            tile_of[p as usize] = t;
        }
        fill(rig, &tile_of, &new, 2);
        let (marked_pages, empty_read) = touch(rig, &new, gone[0]);
        let mut want_marked = new.clone();
        want_marked.sort_unstable();
        if marked_pages != want_marked {
            return Err(format!("after the move the pages marked are {marked_pages:?}, wanted {want_marked:?}"));
        }
        if empty_read != 0 {
            return Err(format!("a page whose tile went reads {empty_read:#x}, not zero"));
        }
        let got = read(rig, &tile_of, &new);
        check_pages("pages on reused tiles", &new, &got, &new, 2)?;
        // SAFETY: a plain query.
        let moved = unsafe { buffer.GetGPUVirtualAddress() };
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
    /// `VICTORY_PAGED_TEST_PC_POOL=1` too (tiles in the PC's memory are a
    /// path no game run has taken on that driver).
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
        for pool in pools {
            match one_pool(&mut rig, pool) {
                Ok(()) => eprintln!("{pool:?}: every step passed {}", if rig.card { "ON THE CARD" } else { "on the software adapter" }),
                Err(why) => {
                    eprintln!("{pool:?}: {why} ({})", rig.step);
                    failed.push(pool);
                }
            }
        }
        // The card's pool is what a paged window needs at the least; the
        // PC's pool is the question this test asks, and either answer is one.
        assert!(!failed.contains(&Pool::Card), "the pool in the card's memory failed");
    }
}

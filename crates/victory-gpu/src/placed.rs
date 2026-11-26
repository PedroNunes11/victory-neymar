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
//! A range of a heap is never given out twice: the system zeroes a heap when
//! it makes it, so a texture sampled before its first upload has run reads
//! zeros, as a committed one does, and never another texture's bytes. The
//! price is the memory of textures let go in a heap where others live. Past
//! [`most_wasted`] of it, textures are committed again until heaps whose
//! last texture went have given theirs back.
//!
//! What keeps a heap: every texture in it holds it through an object of ours
//! in its private data, which the host releases with the texture's last
//! reference, whoever held that (a descriptor slot, a list, the resource
//! table); the heap itself goes once what runs on the card has finished
//! ([`crate::host::retire`]).
//!
//! After the shared heaps of the VIctory-Omega-Fork (commit b1ac3140d,
//! `placed.rs`, by Gooseman; https://github.com/NitricAcidDev/VIctory-Omega-Fork,
//! GPL-3.0), written anew for this tree.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, IUnknown, IUnknown_Vtbl, Interface};

/// The size of a heap.
const HEAP: u64 = 32 << 20;
/// The biggest texture placed in one (so the end of a heap wastes at most this).
const LARGEST: u64 = 4 << 20;
const PAGE: u64 = 4 << 10;
const BLOCK: u64 = 64 << 10;

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

/// Since the start: textures placed, and committed instead for the waste.
static PLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// For the memory line: the heaps' bytes, their textures' bytes; textures
/// placed, and committed instead for the waste, since the start.
#[cfg_attr(not(any(test, feature = "diagnostics")), allow(dead_code))]
pub(crate) fn counts() -> [u64; 4] {
    [&HELD, &LIVE, &PLACED[0], &PLACED[1]].map(|n| n.load(Ordering::Relaxed))
}

/// The heaps textures are placed in now and how much of each is given out:
/// one for textures that start on 4 KiB, one for those on 64 KiB.
static OPEN: Mutex<[Option<(Arc<Heap>, u64)>; 2]> = Mutex::new([None, None]);

/// References of heaps whose texture went, let go outside that texture's own
/// release ([`let_go`]).
static GONE: Mutex<Vec<Arc<Heap>>> = Mutex::new(Vec::new());

/// What a placed texture holds through its private data: an `IUnknown` of
/// ours, released when the texture is destroyed.
#[repr(C)]
struct Token {
    vtable: *const IUnknown_Vtbl,
    references: AtomicU32,
    heap: Option<Arc<Heap>>,
    bytes: u64,
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
            LIVE.fetch_sub(token.bytes, Ordering::Relaxed);
            // The texture is being destroyed around this: the heap's
            // reference goes afterwards.
            if let Some(heap) = token.heap.take() {
                GONE.lock().unwrap_or_else(|e| e.into_inner()).push(heap);
            }
        }
        left
    }
}

impl Token {
    fn make(heap: Arc<Heap>, bytes: u64) -> IUnknown {
        LIVE.fetch_add(bytes, Ordering::Relaxed);
        let token = Box::new(Token {
            vtable: &TOKEN_VTABLE,
            references: AtomicU32::new(1),
            heap: Some(heap),
            bytes,
        });
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

/// Lets go of the heap references of textures that went since the last
/// call, once what runs on the card now has finished.
pub(crate) fn let_go() {
    let gone = std::mem::take(&mut *GONE.lock().unwrap_or_else(|e| e.into_inner()));
    if !gone.is_empty() {
        crate::host::retire(gone);
    }
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
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    // Neither a live texture's nor still to be given out.
    let unused: u64 = open.iter().flatten().map(|(_, used)| HEAP - used).sum();
    let wasted = HELD.load(Ordering::Relaxed).saturating_sub(LIVE.load(Ordering::Relaxed)).saturating_sub(unused);
    if wasted > most_wasted() {
        PLACED[1].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let kind = usize::from(alignment == BLOCK);
    let room = open[kind]
        .as_ref()
        .map(|(heap, used)| (heap.clone(), used.next_multiple_of(alignment)))
        .filter(|(_, at)| at + bytes <= HEAP);
    let (heap, at) = match room {
        Some(room) => room,
        None => (Arc::new(make_heap(device)?), 0),
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the range lies in the heap and was never given to another texture.
    if let Err(e) = unsafe { device.CreatePlacedResource(&heap.heap, at, &desc, state, None, &mut made) } {
        warn_once(&e);
    }
    let made = made?;
    let token = Token::make(heap.clone(), bytes);
    // SAFETY: a live texture, which takes a reference of the token.
    if unsafe { made.SetPrivateDataInterface(&TOKEN, &token) }.is_err() {
        // Nothing would keep the heap: the texture goes, the caller commits one.
        return None;
    }
    // (The heap that was open before lives on in its textures.)
    open[kind] = Some((heap, at + bytes));
    PLACED[0].fetch_add(1, Ordering::Relaxed);
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

fn make_heap(device: &ID3D12Device) -> Option<Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: HEAP,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: BLOCK,
        // (Zeroed: see the head of this file.)
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
                windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(
                    &crate::host::warp_adapter().ok()?,
                    D3D_FEATURE_LEVEL_12_0,
                    &mut device,
                )
                .ok()?;
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

    /// Small and middling textures share heaps, small ones on 4 KiB; each
    /// holds its own texels while the others are written (before and after
    /// the heaps they began in were closed); one never written reads zeros;
    /// half of them going gives no heap back while the rest live there, all
    /// of them going gives back every heap but the open ones.
    #[test]
    fn textures_in_shared_heaps_keep_their_own_texels_and_their_heaps_go_with_them() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rig) = Rig::new() else {
            eprintln!("SKIPPED: no software adapter");
            return;
        };
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
        assert!(after[0] >= after[1] && after[1] > before[1], "{after:?}");
        // The 60 small ones take 4 KiB steps, not 64 KiB ones.
        let small: u64 = (0..N)
            .filter(|i| kinds[i % kinds.len()].0 <= 64)
            // SAFETY: a live resource.
            .map(|i| unsafe { rig.device.GetResourceAllocationInfo(0, &[textures[i].GetDesc()]) }.SizeInBytes)
            .sum();
        assert!(small <= 60 * (32 << 10), "{small}");
        let pitch = 4096u32;
        let bytes = u64::from(pitch) * 1024;
        let upload = rig.buffer(D3D12_HEAP_TYPE_UPLOAD, bytes, D3D12_RESOURCE_STATE_GENERIC_READ);
        let back = rig.buffer(D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_STATE_COPY_DEST);
        let mut at = std::ptr::null_mut();
        // SAFETY: the mapping holds the buffer's bytes for its life.
        unsafe { upload.Map(0, None, Some(&mut at)).unwrap() };
        // Each texture but every tenth filled with its own number, in an order other than theirs.
        let number = |i: usize| if i.is_multiple_of(10) { 0 } else { i as u8 + 1 };
        for i in (0..N).map(|k| k * 67 % N).filter(|&i| number(i) != 0) {
            let (side, format, _) = kinds[i % kinds.len()];
            // SAFETY: the mapped upload buffer; the list is waited for.
            unsafe { at.cast::<u8>().write_bytes(number(i), bytes as usize) };
            rig.run(|list| unsafe {
                list.CopyTextureRegion(&level0(&textures[i]), 0, 0, 0, &footprint(&upload, side, format, pitch), None)
            });
        }
        for (i, texture) in textures.iter().enumerate() {
            let (side, format, texel) = kinds[i % kinds.len()];
            // SAFETY: a list of ours; both resources live.
            rig.run(|list| unsafe {
                list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.CopyTextureRegion(&footprint(&back, side, format, pitch), 0, 0, 0, &level0(texture), None);
            });
            let mut read = std::ptr::null_mut();
            // SAFETY: the read-back buffer's mapping, read after the list ran.
            unsafe { back.Map(0, None, Some(&mut read)).unwrap() };
            for row in [0, side / 2, side - 1] {
                // SAFETY: inside the mapping (`row` < 1024 rows of `pitch` bytes).
                let got = unsafe { std::slice::from_raw_parts(read.cast::<u8>().add((row * pitch) as usize), (side * texel) as usize) };
                assert!(got.iter().all(|&b| b == number(i)), "texture {i} row {row}: {:?}", &got[..8.min(got.len())]);
            }
            // SAFETY: as above.
            unsafe { back.Unmap(0, None) };
        }
        // Half of them go: their heaps live on in the others.
        let held = counts()[0];
        let mut textures = textures;
        textures.truncate(N / 2);
        let_go();
        assert_eq!(counts()[0], held, "no heap given back while textures live in it");
        assert!(counts()[1] < after[1]);
        // All of them: every heap goes but the two open ones (once what other
        // tests' submissions run has finished: `crate::host::retire`).
        drop(textures);
        for _ in 0..400 {
            let_go();
            if counts()[0] <= before[0] + 2 * HEAP {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(counts()[0] <= before[0] + 2 * HEAP, "{:?} {before:?}", counts());
        assert_eq!(counts()[1], before[1]);
        // SAFETY: a plain query.
        assert!(unsafe { rig.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Targets, depth and unordered access textures and big ones are not
    /// placed; past the waste allowed none is, and then one is again.
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
        let small = desc(256, 9, DXGI_FORMAT_BC7_UNORM);
        let kept = make(&rig.device, &small, state).expect("placed");
        // (As if so much of the heaps were wasted.)
        HELD.fetch_add(4 << 30, Ordering::Relaxed);
        let refused = make(&rig.device, &small, state);
        HELD.fetch_sub(4 << 30, Ordering::Relaxed);
        assert!(refused.is_none());
        assert!(make(&rig.device, &small, state).is_some());
        // Off by default: the switch says no.
        assert!(create(&rig.device, &small, state).is_none());
        drop(kept);
        let_go();
    }
}

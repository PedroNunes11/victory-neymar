//! Refreshes of host copies of guarded view memory while the title's
//! threads write it: nothing written is lost, with the blocks made read-only
//! again under the guards' lock or outside it (`VICTORY_GUARD_OUTSIDE`).
//!
//! The title memory is a placeholder reservation with a pagefile section
//! mapped into it 64 KiB block by block, as the runtime maps physical
//! pages; host copies (`Shadow`s over plain host memory) cover it.
//!
//! From fxceless/victory-fork pull request #1315 (KainanP, `shadow/
//! guard_bench.rs`, GPL-3.0-or-later): its correctness test and the helpers
//! it needs; its benchmarks are not taken.

use super::Shadow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    PAGE_NOACCESS, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const BLOCK: usize = 64 << 10;

/// A plain buffer on the software adapter: a copy's `buffer`, which the
/// refresh never touches.
fn warp_buffer() -> Option<ID3D12Resource> {
    // SAFETY: plain DXGI and D3D12 creation calls on the software adapter.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device).ok()?;
        let device = device?;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 64 << 10,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
            .ok()?;
        buffer
    }
}

/// `blocks` blocks of title view memory: a placeholder with a pagefile
/// section mapped into it a block at a time.
struct Title {
    at: usize,
    blocks: usize,
    section: windows_sys::Win32::Foundation::HANDLE,
    views: Vec<MEMORY_MAPPED_VIEW_ADDRESS>,
}

impl Title {
    fn new(blocks: usize) -> Title {
        // SAFETY: a fresh placeholder and pagefile section, split and
        // mapped block by block; all ours until dropped.
        unsafe {
            let at = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                blocks * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(at, 0);
            let bytes = (blocks * BLOCK) as u64;
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                (bytes >> 32) as u32,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let mut views = Vec::with_capacity(blocks);
            for block in 0..blocks {
                let place = at + block * BLOCK;
                if block + 1 < blocks {
                    VirtualFree(
                        place as *mut _,
                        BLOCK,
                        MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                    );
                }
                let view = MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    place as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(view.Value as usize, place);
                views.push(view);
            }
            crate::watch::title_memory_remapped(at, blocks * BLOCK);
            Title {
                at,
                blocks,
                section,
                views,
            }
        }
    }

    fn len(&self) -> usize {
        self.blocks * BLOCK
    }
}

impl Drop for Title {
    fn drop(&mut self) {
        crate::watch::title_memory_remapped(self.at, self.len());
        // SAFETY: our views, placeholder and section.
        unsafe {
            for &view in &self.views {
                UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
            }
            for block in 0..self.blocks {
                VirtualFree((self.at + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(self.section);
        }
        crate::watch::title_memory_remapped(self.at, self.len());
    }
}

/// A host copy over `[address, address + len)` of title memory, in host
/// memory of its own (`side`, kept alive beside it).
struct Copy {
    shadow: Arc<Shadow>,
    side: Box<[u8]>,
}

fn copies(buffer: &ID3D12Resource, title: &Title, span: usize, overlap: bool) -> Vec<Copy> {
    let mut ranges: Vec<(usize, usize)> = (0..title.len() / span)
        .map(|i| (title.at + i * span, span))
        .collect();
    if overlap {
        // Copies of the same memory that cross the others' edges, as a
        // texture inside a buffer: the journal tells each what another's
        // asking found.
        ranges.extend((0..title.len() / span - 1).map(|i| (title.at + i * span + span / 2, span)));
    }
    ranges
        .into_iter()
        .map(|(address, len)| {
            let mut side = vec![0u8; len].into_boxed_slice();
            let shadow = Arc::new(Shadow::new(
                buffer.clone(),
                side.as_mut_ptr(),
                0,
                address as u64,
                len as u64,
            ));
            Copy { shadow, side }
        })
        .collect()
}

/// One flush: every copy refreshed whole, on this thread or (`wide`) on
/// the helper threads. The bytes copied.
fn flush(copies: &[Copy], wide: bool) -> u64 {
    let bytes = Arc::new(AtomicU64::new(0));
    if wide {
        let jobs: Vec<Box<dyn FnOnce() + Send>> = copies
            .iter()
            .map(|c| {
                let (shadow, bytes) = (c.shadow.clone(), bytes.clone());
                Box::new(move || {
                    let n = shadow.update(shadow.start as usize, shadow.len as usize);
                    bytes.fetch_add(n as u64, Relaxed);
                }) as Box<dyn FnOnce() + Send>
            })
            .collect();
        super::wide_run(jobs);
    } else {
        for c in copies {
            let n = c
                .shadow
                .update(c.shadow.start as usize, c.shadow.len as usize);
            bytes.fetch_add(n as u64, Relaxed);
        }
    }
    bytes.load(Relaxed)
}

/// Sets its flag when dropped.
struct Stop<'a>(&'a AtomicBool);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Relaxed);
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Writers and flushes interleaved, on one thread and on the helper
/// threads, with copies that overlap: no write is lost (a refresh that
/// began after a write holds it, or a later one), none is made up or
/// taken back (a copy never holds a value newer than was written, nor
/// older than it held after the flush before), and once the writers stop
/// one flush makes every copy equal to title memory.
#[test]
fn flushes_under_writes_lose_nothing() {
    let Some(buffer) = warp_buffer() else {
        return;
    };
    const BLOCKS: usize = 96;
    const WRITERS: usize = 3;
    // Each writer counts up in its own words, spread over every block.
    const SLOTS: usize = 64;
    let title = Title::new(BLOCKS);
    let (at, len) = (title.at, title.len());
    let slot = |w: usize, i: usize| -> usize {
        // Block `i` (of the writer's slots), at a place of the writer's.
        let block = (i * 37 + w * 11) % BLOCKS;
        at + block * BLOCK + (w * SLOTS + i) * 8 * 13 % (BLOCK - 8) / 8 * 8
    };
    let written: Vec<Vec<AtomicU64>> = (0..WRITERS)
        .map(|_| (0..SLOTS).map(|_| AtomicU64::new(0)).collect())
        .collect();
    // Each writer's number, told before its store: no word of its holds
    // more. Both rounds count on from where the one before stopped.
    let latest: Vec<AtomicU64> = (0..WRITERS).map(|_| AtomicU64::new(0)).collect();
    for (wide, outside) in [(false, false), (true, false), (false, true), (true, true)] {
        crate::watch::GUARD_OUTSIDE_TEST.store(if outside { 2 } else { 1 }, Relaxed);
        let copies = copies(&buffer, &title, 8 * BLOCK, true);
        flush(&copies, false);
        let stop = AtomicBool::new(false);
        // What each copy held at each slot after the flush before.
        let mut held: Vec<Vec<u64>> = vec![vec![0; WRITERS * SLOTS]; copies.len()];
        std::thread::scope(|s| {
            for (w, mine) in written.iter().enumerate() {
                let (stop, latest) = (&stop, &latest[w]);
                s.spawn(move || {
                    let mut n = latest.load(Relaxed);
                    let mut rng = Rng(0xfeed + w as u64);
                    while !stop.load(Relaxed) {
                        n += 1;
                        let i = rng.next() as usize % SLOTS;
                        latest.store(n, std::sync::atomic::Ordering::Release);
                        // SAFETY: inside the title memory (the handler opens
                        // a guarded block).
                        unsafe { (slot(w, i) as *mut u64).write_volatile(n) };
                        // Published after the store: a flush that reads this
                        // number before it begins finds the store done.
                        mine[i].store(n, std::sync::atomic::Ordering::Release);
                        if n % 64 == 0 {
                            std::thread::yield_now();
                        }
                    }
                });
            }
            // The writers stop however this ends (a failed check too: the
            // scope waits for them).
            let _stop = Stop(&stop);
            for _ in 0..100 {
                let floor: Vec<u64> = written
                    .iter()
                    .flat_map(|w| {
                        w.iter()
                            .map(|v| v.load(std::sync::atomic::Ordering::Acquire))
                    })
                    .collect();
                flush(&copies, wide);
                let ceiling: Vec<u64> = latest
                    .iter()
                    .map(|v| v.load(std::sync::atomic::Ordering::Acquire))
                    .collect();
                for (c, copy) in copies.iter().enumerate() {
                    let start = copy.shadow.start as usize;
                    for w in 0..WRITERS {
                        for i in 0..SLOTS {
                            let address = slot(w, i);
                            if address < start || address + 8 > start + copy.side.len() {
                                continue;
                            }
                            let k = w * SLOTS + i;
                            // SAFETY: inside the copy's side, written by the
                            // flush that has returned.
                            let value = unsafe {
                                (copy.side.as_ptr().add(address - start) as *const u64)
                                    .read_unaligned()
                            };
                            assert!(
                                value >= floor[k],
                                "wide {wide} outside {outside}: copy {c} lost a write at {address:#x}: holds {value}, {} was written before the flush",
                                floor[k]
                            );
                            assert!(
                                value <= ceiling[w],
                                "wide {wide} outside {outside}: copy {c} holds {value} at {address:#x}, never written ({} at most)",
                                ceiling[w]
                            );
                            assert!(
                                value >= held[c][k],
                                "wide {wide} outside {outside}: copy {c} went back at {address:#x}: {value} after {}",
                                held[c][k]
                            );
                            held[c][k] = value;
                        }
                    }
                }
            }
            stop.store(true, Relaxed);
        });
        flush(&copies, wide);
        for (c, copy) in copies.iter().enumerate() {
            let start = copy.shadow.start as usize;
            let n = copy.side.len().min(at + len - start);
            // SAFETY: title memory we mapped; readable whether guarded or not.
            let now = unsafe { std::slice::from_raw_parts(start as *const u8, n) };
            assert!(
                copy.side[..n] == *now,
                "wide {wide} outside {outside}: copy {c} differs from title memory after a quiet flush"
            );
        }
    }
    crate::watch::GUARD_OUTSIDE_TEST.store(0, Relaxed);
    // The rounds with the switch on made blocks read-only outside the locks.
    assert!(crate::watch::OUTSIDE[0].load(Relaxed) > 0, "no block was made read-only outside the locks");
}

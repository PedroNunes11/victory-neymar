//! Item 2, step R1 (memory plan): how fast the CPU writes memory the card
//! sees in the PC's RAM (a CUSTOM heap, write-back, L0), and how fast the
//! card reads it by compute and copies it into its own memory, against the
//! same reads from the card's memory. Not the title, not the runtime.
//!
//! Conditions of Pedro's yes (27/11): heaps of 64 MiB, 256 MiB and 1 GiB,
//! one size at a time (at most 1 GiB in the PC's RAM and 1 GiB on the card
//! at once); every submission under 50 ms (256 MiB a dispatch or copy); an
//! ordinary heap (no section, no heap opened on an address, no tiles, no
//! ReBAR); everything let go when it ends.
//!
//!   cargo run --release -p victory-gpu --example r1_bandwidth
#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    imp::run()
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
mod imp {
    use std::time::Instant;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::{Interface, s};

    const MIB: u64 = 1 << 20;
    /// The most one submission reads or copies: ~10 ms over the bus.
    const CHUNK: u64 = 256 * MIB;
    const SHADER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer C : register(b0) { uint threads; uint pad; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint4 acc = 0;
    [unroll] for (uint k = 0; k < 16; k++) {
        acc ^= src.Load4((id.x + k * threads) * 16);
    }
    uint v = acc.x ^ acc.y ^ acc.z ^ acc.w;
    if (v == 0x9e3779b9u) { dst.Store(0, v); }
}
"#;

    struct Gpu {
        device: ID3D12Device,
        direct: ID3D12CommandQueue,
        copy: ID3D12CommandQueue,
        fence: ID3D12Fence,
        value: u64,
        event: HANDLE,
        root: ID3D12RootSignature,
        pso: ID3D12PipelineState,
        out: ID3D12Resource,
    }

    fn buffer_desc(size: u64, uav: bool) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
            ..Default::default()
        }
    }

    fn committed(device: &ID3D12Device, heap: &D3D12_HEAP_PROPERTIES, size: u64, uav: bool) -> windows::core::Result<ID3D12Resource> {
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation, out pointer ours.
        unsafe { device.CreateCommittedResource(heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(size, uav), D3D12_RESOURCE_STATE_COMMON, None, &mut out)? };
        out.ok_or_else(windows::core::Error::empty)
    }

    fn heap(kind: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
        D3D12_HEAP_PROPERTIES { Type: kind, CreationNodeMask: 1, VisibleNodeMask: 1, ..Default::default() }
    }

    /// The PC's RAM, seen by the card, cached for the CPU.
    fn ram_heap() -> D3D12_HEAP_PROPERTIES {
        D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        }
    }

    impl Gpu {
        fn new() -> windows::core::Result<Gpu> {
            // SAFETY: plain creation calls; out pointers ours; blobs read as their own bytes.
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
                let adapter = factory.EnumAdapters1(0)?;
                let desc = adapter.GetDesc1()?;
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
                println!("adapter: {name}, {} MB of its own memory", desc.DedicatedVideoMemory >> 20);
                let mut device: Option<ID3D12Device> = None;
                windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                let device = device.ok_or_else(windows::core::Error::empty)?;
                let queue = |kind| device.CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC { Type: kind, ..Default::default() });
                let (direct, copy) = (queue(D3D12_COMMAND_LIST_TYPE_DIRECT)?, queue(D3D12_COMMAND_LIST_TYPE_COPY)?);
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
                let event = CreateEventW(None, false, false, None)?;
                let mut code: Option<ID3DBlob> = None;
                let mut errors: Option<ID3DBlob> = None;
                let compiled = D3DCompile(SHADER.as_ptr().cast(), SHADER.len(), s!("r1"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, Some(&mut errors));
                if let Err(e) = compiled {
                    if let Some(errors) = errors {
                        let text = std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize());
                        eprintln!("{}", String::from_utf8_lossy(text));
                    }
                    return Err(e);
                }
                let code = code.ok_or_else(windows::core::Error::empty)?;
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
                let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })?;
                let out = committed(&device, &heap(D3D12_HEAP_TYPE_DEFAULT), 4096, true)?;
                Ok(Gpu { device, direct, copy, fence, value: 0, event, root, pso, out })
            }
        }

        /// Runs one list on `queue` and waits for it: its milliseconds, wall clock.
        fn run(&mut self, kind: D3D12_COMMAND_LIST_TYPE, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<f64> {
            // SAFETY: lists recorded and run here, waited for before they go.
            unsafe {
                let alloc: ID3D12CommandAllocator = self.device.CreateCommandAllocator(kind)?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, kind, &alloc, None)?;
                record(&list);
                list.Close()?;
                let queue = if kind == D3D12_COMMAND_LIST_TYPE_COPY { &self.copy } else { &self.direct };
                let began = Instant::now();
                queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
                self.value += 1;
                queue.Signal(&self.fence, self.value)?;
                self.fence.SetEventOnCompletion(self.value, self.event)?;
                WaitForSingleObject(self.event, INFINITE);
                Ok(began.elapsed().as_secs_f64() * 1e3)
            }
        }

        /// The card reads `[offset, offset + bytes)` of `src` by compute.
        fn read(&mut self, src: &ID3D12Resource, offset: u64, bytes: u64) -> windows::core::Result<f64> {
            let (root, pso, out) = (self.root.clone(), self.pso.clone(), self.out.clone());
            // SAFETY: GPU addresses of live buffers, inside them.
            let (at, out_at) = unsafe { (src.GetGPUVirtualAddress() + offset, out.GetGPUVirtualAddress()) };
            let threads = (bytes / 256) as u32;
            self.run(D3D12_COMMAND_LIST_TYPE_DIRECT, |l| unsafe {
                l.SetComputeRootSignature(&root);
                l.SetPipelineState(&pso);
                l.SetComputeRoot32BitConstant(0, threads, 0);
                l.SetComputeRootShaderResourceView(1, at);
                l.SetComputeRootUnorderedAccessView(2, out_at);
                l.Dispatch(threads / 256, 1, 1);
            })
        }

        /// The copy queue copies `bytes` from `src` to `dst` at `offset`.
        fn copy(&mut self, dst: &ID3D12Resource, src: &ID3D12Resource, offset: u64, bytes: u64) -> windows::core::Result<f64> {
            let (dst, src) = (dst.clone(), src.clone());
            self.run(D3D12_COMMAND_LIST_TYPE_COPY, |l| unsafe { l.CopyBufferRegion(&dst, offset, &src, offset, bytes) })
        }
    }

    impl Drop for Gpu {
        fn drop(&mut self) {
            // SAFETY: our event.
            unsafe {
                let _ = CloseHandle(self.event);
            }
        }
    }

    fn best(times: &[f64]) -> f64 {
        times.iter().copied().fold(f64::MAX, f64::min)
    }

    /// GB/s of `bytes` in `ms`.
    fn rate(bytes: u64, ms: f64) -> f64 {
        bytes as f64 / 1e9 / (ms / 1e3)
    }

    pub(super) fn run() -> windows::core::Result<()> {
        let mut gpu = Gpu::new()?;
        let pattern: Vec<u8> = (0..64 * MIB as usize).map(|i| (i * 2654435761usize >> 13) as u8).collect();
        for size in [64 * MIB, 256 * MIB, 1024 * MIB] {
            let ram = committed(&gpu.device, &ram_heap(), size, false)?;
            let card = committed(&gpu.device, &heap(D3D12_HEAP_TYPE_DEFAULT), size, false)?;
            // CPU writes into the RAM heap, mapped.
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: a CPU-visible buffer, mapped whole; written inside it.
            let write_ms = unsafe {
                ram.Map(0, None, Some(&mut data))?;
                let began = Instant::now();
                let mut at = 0u64;
                while at < size {
                    let n = (size - at).min(pattern.len() as u64) as usize;
                    std::ptr::copy_nonoverlapping(pattern.as_ptr(), data.cast::<u8>().add(at as usize), n);
                    at += n as u64;
                }
                let ms = began.elapsed().as_secs_f64() * 1e3;
                ram.Unmap(0, None);
                ms
            };
            let chunk = size.min(CHUNK);
            let chunks = size / chunk;
            // The copy queue: RAM heap -> the card's memory.
            let mut copy = Vec::new();
            for _ in 0..3 {
                let mut ms = 0.0;
                for c in 0..chunks {
                    ms += gpu.copy(&card, &ram, c * chunk, chunk)?;
                }
                copy.push(ms);
            }
            // Compute reads from each.
            let mut times = [Vec::new(), Vec::new()];
            for _ in 0..5 {
                for (k, src) in [&ram, &card].into_iter().enumerate() {
                    let mut ms = 0.0;
                    for c in 0..chunks {
                        ms += gpu.read(src, c * chunk, chunk)?;
                    }
                    times[k].push(ms);
                }
            }
            println!(
                "{:>5} MiB: CPU writes the RAM heap {:.1} GB/s ({:.2} ms); copy queue RAM -> card {:.1} GB/s ({:.2} ms); card reads by compute: RAM heap {:.1} GB/s ({:.2} ms), its own memory {:.1} GB/s ({:.2} ms)",
                size / MIB,
                rate(size, write_ms),
                write_ms,
                rate(size, best(&copy)),
                best(&copy),
                rate(size, best(&times[0])),
                best(&times[0]),
                rate(size, best(&times[1])),
                best(&times[1]),
            );
        }
        println!("done; everything let go");
        Ok(())
    }
}

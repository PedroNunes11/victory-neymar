//! A least amount of indirect light, while the title's ray-traced light
//! does not run.
//!
//! The title's indirect diffuse light comes from probes it fills by tracing
//! rays, which do not run here yet. Its compute program that resolves them
//! leaves garbage instead (bright magenta on leaves, cloth and paper,
//! blocks of colours indoors), and whatever the sun or a lamp does not reach
//! directly is black. Right after that program, its output is replaced by a
//! constant, which the title's own lighting then applies with the surfaces'
//! colours. The program is told by a hash of the name its container carries
//! (no name of the title's in this source).
//!
//! `VICTORY_AMBIENT=<level>` sets it (0.6 by default, 0 for none); `off`
//! keeps the title's own output.
//! shortcut: the same light everywhere, from everywhere; gone once rays run.
//!
//! After Belobug/VIctory-Belobug-AMD (`ambient.rs`, GPL-3.0-or-later).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, host};
use crate::object::This;

/// The program's name, as its container carries it: its length and its
/// 64-bit FNV-1a, and the 3 bytes it starts with (every compute program's).
const NAME: (usize, u64, &[u8; 3]) = (31, 0x29db_9b1e_7101_035a, b"CS_");

/// Whether `container` is the program that resolves the indirect light.
pub(crate) fn resolves_light(container: &[u8]) -> bool {
    let (len, hash, start) = NAME;
    container.windows(len).any(|w| w.starts_with(start) && fnv(w) == hash)
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Where it writes the indirect diffuse light (R9G9B9E5 as 32-bit
/// integers): the table's user-data slot and the dword in it.
pub(crate) const LIGHT: (u8, u32) = (1, 176);

/// 0.6 looked balanced where this was made. `VICTORY_AMBIENT=off` leaves
/// what the title's program wrote; 0 is no indirect diffuse light at all.
const DEFAULT: f32 = 0.6;

/// The level; `None` if off.
pub(crate) fn level() -> Option<f32> {
    static LEVEL: OnceLock<f32> = OnceLock::new();
    let level = *LEVEL.get_or_init(|| {
        let set = std::env::var("VICTORY_AMBIENT").unwrap_or_default();
        if set.trim() == "off" {
            return -1.0;
        }
        set.trim().parse().ok().filter(|l: &f32| l.is_finite() && *l >= 0.0).unwrap_or(DEFAULT)
    });
    (level >= 0.0).then_some(level)
}

/// The light's colour: warm white, or what `VICTORY_AMBIENT_TINT=r,g,b`
/// gives. With `VICTORY_AMBIENT_FILE=<file>` (diagnostics builds) the file's
/// `level r g b` is read again twice a second, to tune both by eye in a
/// running game.
fn tint() -> [f32; 3] {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, tint)) = *live {
        return tint;
    }
    static SET: OnceLock<[f32; 3]> = OnceLock::new();
    *SET.get_or_init(|| {
        let set: Vec<f32> = std::env::var("VICTORY_AMBIENT_TINT")
            .unwrap_or_default()
            .split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect();
        match set[..] {
            [r, g, b] => [r, g, b],
            _ => [1.0, 0.98, 0.94],
        }
    })
}

/// The level and colour last read from `VICTORY_AMBIENT_FILE`, if any.
static LIVE: std::sync::Mutex<Option<(f32, [f32; 3])>> = std::sync::Mutex::new(None);

/// The level from `VICTORY_AMBIENT_FILE`, if it gave one; starts the thread
/// that reads the file.
fn live_level() -> Option<f32> {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if !cfg!(feature = "diagnostics") {
            return;
        }
        let Some(file) = std::env::var_os("VICTORY_AMBIENT_FILE") else { return };
        let _ = std::thread::Builder::new().name("victory-ambient".into()).spawn(move || {
            loop {
                let read: Vec<f32> = std::fs::read_to_string(&file)
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter_map(|v| v.parse().ok())
                    .collect();
                if let [level, r, g, b] = read[..] {
                    let new = Some((level.max(0.0), [r, g, b]));
                    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
                    if *live != new {
                        tracing::info!(level, r, g, b, "ambient light set from the file");
                        *live = new;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });
    });
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).map(|(level, _)| level)
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), DescriptorTable(UAV(u0))"
cbuffer Floor : register(b0) { float level; float3 tint; };
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float most = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(most, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (most * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    light.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    // What the resolve left is not kept: without rays its inputs are never
    // written and it holds whatever their memory did (bright magenta on
    // translucent surfaces, blocks of colours indoors: run v6, 2026-11-25).
    light[at.xy] = pack(tint * level);
}
"#;

/// Views, used in turn: more than the passes of every list in flight.
const SLOTS: u32 = 1024;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: AtomicU32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("ambient"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let text = |b: &ID3DBlob| {
        // SAFETY: the blob's own bytes.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }
            .to_vec()
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            tracing::warn!("ambient light: {}", String::from_utf8_lossy(&text(&errors)));
        }
        return Err(e);
    }
    let code = text(&code.ok_or_else(windows::core::Error::empty)?);
    // SAFETY: the root signature is inside the shader; plain creations.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        })?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let views = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: SLOTS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            root,
            pso,
            views,
            step: device.GetDescriptorHandleIncrementSize(kind),
            next: AtomicU32::new(0),
        })
    }
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Raises what `light` holds to the floor, in `list`, after the program that
/// wrote it as an image. Whether it recorded anything (the list's bindings
/// are then ours).
pub(crate) fn raise(list: &HostList, light: This) -> bool {
    let (Some(level), Some(host)) = (level(), host()) else {
        return false;
    };
    let Some(light) = crate::resource::host_resource(light) else {
        return false;
    };
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    let Some(pass) = PASS
        .get_or_init(|| {
            make(&host.device)
                .inspect_err(|e| tracing::warn!(error = %e, "no ambient light"))
                .ok()
        })
        .as_ref()
    else {
        return false;
    };
    // SAFETY: a live resource.
    let d = unsafe { light.GetDesc() };
    static ONCE: std::sync::Once = std::sync::Once::new();
    if d.Format != DXGI_FORMAT_R32_UINT {
        ONCE.call_once(|| tracing::warn!(format = d.Format.0, "no ambient light: another format"));
        return false;
    }
    ONCE.call_once(|| {
        tracing::info!(level, width = d.Width, height = d.Height, "ambient light");
    });
    let slot = pass.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
    list.keep(light);
    let l = &list.list;
    uav_barrier(l);
    // SAFETY: an open list; a live texture the list keeps, in the
    // unordered-access state as the program before left it; a view in a slot
    // no list in flight reads.
    unsafe {
        let cpu = pass.views.GetCPUDescriptorHandleForHeapStart();
        let gpu = pass.views.GetGPUDescriptorHandleForHeapStart();
        let at = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: cpu.ptr + (slot * pass.step) as usize,
        };
        let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        host.device
            .CreateUnorderedAccessView(light, None::<&ID3D12Resource>, Some(&view), at);
        l.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pso);
        let [r, g, b] = tint();
        let level = live_level().unwrap_or(level);
        let constants = [level, r, g, b];
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(
            1,
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu.ptr + u64::from(slot * pass.step),
            },
        );
        l.Dispatch((d.Width as u32).div_ceil(8), d.Height.div_ceil(8), 1);
    }
    uav_barrier(l);
    true
}

#[cfg(test)]
mod tests {
    /// The pass compiles with the system's compiler and the host takes it.
    #[test]
    fn ambient_pass_builds() {
        let Some(host) = crate::host::host() else {
            return;
        };
        super::make(&host.device).expect("the ambient pass");
    }

    /// The program is told by the hash only where the whole name matches.
    #[test]
    fn the_name_is_told_by_its_hash() {
        assert!(!super::resolves_light(b"CS_ a container with another program in it"));
        assert!(!super::resolves_light(&[0u8; 64]));
        assert_eq!(super::fnv(b""), 0xcbf2_9ce4_8422_2325);
    }
}

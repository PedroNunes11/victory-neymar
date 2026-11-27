//! Textures shaders write that the title's own code then reads on the CPU
//! (`VICTORY_TEXTURE_BACK=1`, off by default).
//!
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it
//! stays in the texture's host copy. The title's ocean is one: its wave
//! passes write a 256x256 height map (R32_FLOAT) and a displacement map
//! (R32G32_FLOAT), and its CPU code samples both to float boats on. It read
//! memory nothing had written, and boats sank into the void (the first boat
//! mission cannot be finished: 26/11).
//!
//! A list whose shaders write a texture of one of the kinds below, through
//! unordered access, copies the mip and slices that view exposes into a
//! read-back buffer of ours when it closes; once the list has run, the
//! texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them. The title reads them
//! a frame late, as it would before the console's GPU ran. A read-back whose
//! title memory was remapped, replaced or released meanwhile is dropped.
//!
//! shortcut: by kind, not by texture: textures of these kinds the title
//! never reads are copied back too (about a megabyte per such list).
//!
//! Provenance: written again for this tree after JMAN730/project-flamingo
//! commit cdc03a8fe32f (fever308, 2026-11-26, "port water texture readback"),
//! which ports fxceless/victory-fork pull request 999 (Geckomacabre, commits
//! 895c0b8f7 and 04df43508; the read watch that found ~30 title addresses
//! reading these kinds) with that fork's later fixes: only the subresources
//! a view exposes, no unaddressable layouts, the texture's state kept.
//! All GPL-3.0.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds the title's CPU code was seen reading (the upstream read
/// watch): (width, height, console format).
const WATER: &str = "256x256f41,256x256f16,64x64f16,128x128f36";

/// `VICTORY_TEXTURE_BACK`: unset or `0` none; `1` the water kinds; else a
/// list of `<width>x<height>f<format>`.
fn parse_kinds(spec: Option<&str>) -> Vec<(u64, u32, u32)> {
    let list = match spec.map(str::trim) {
        None | Some("") | Some("0") => return Vec::new(),
        Some("1") => WATER,
        Some(list) => list,
    };
    list.split(',')
        .filter_map(|k| {
            let (size, format) = k.trim().split_once('f')?;
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
        })
        .collect()
}

fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        let kinds = parse_kinds(std::env::var("VICTORY_TEXTURE_BACK").ok().as_deref());
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty() && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// A read-back on its way to title memory `at..end`, which a remap, a
/// replaced or a released resource there makes stale.
struct Ticket {
    at: usize,
    end: usize,
    valid: AtomicBool,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { at, end, valid: AtomicBool::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|t| t.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(stale: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|t| {
        let Some(ticket) = t.upgrade() else { return false };
        if stale(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

/// The title released the texture at title memory `at`.
pub(crate) fn released(at: usize) {
    invalidate(|t| t.at == at);
}

/// Title memory `at..at + len` was mapped again (`crate::watch`).
pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|t| at < t.end && t.at < end);
}

/// A texture a list's shaders write, and the subresources (title index:
/// `slice * mips + mip`) its views exposed.
pub(crate) struct Target {
    owner: usize,
    copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    at: usize,
    end: usize,
    subresources: BTreeSet<u32>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        let copy = crate::resource::host_copy(resource)?;
        // Levels given tiles on demand may have no memory: left out.
        if copy.tiled.is_some() {
            return None;
        }
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        if !layout.addressable() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(layout = ?layout.desc, "a texture to copy back has a layout with no address equation: left out"));
            return None;
        }
        Some(Self { owner: resource as usize, copy, layout, at, end, subresources: BTreeSet::new() })
    }

    pub(crate) fn owner(&self) -> usize {
        self.owner
    }

    pub(crate) fn copy(&self) -> &Arc<crate::resource::HostTexture> {
        &self.copy
    }

    /// A view writes `mip` of `slices` (every slice when `None`).
    pub(crate) fn include(&mut self, mip: u32, slices: Option<std::ops::Range<u32>>) {
        let (mips, depth) = (self.layout.desc.mips, self.layout.desc.depth);
        let slices = slices.unwrap_or(0..depth);
        if mip < mips && slices.start < slices.end && slices.end <= depth {
            self.subresources.extend(slices.map(|s| s * mips + mip));
        }
    }
}

/// The mip and slices an unordered access view `words` (title words) writes.
pub(crate) fn view_writes(words: &[u32; 10]) -> Option<(u32, Option<std::ops::Range<u32>>)> {
    let desc = crate::descriptor::host::uav_desc(words);
    match desc.ViewDimension {
        D3D12_UAV_DIMENSION_TEXTURE2D => {
            // SAFETY: the member ViewDimension names.
            let v = unsafe { desc.Anonymous.Texture2D };
            (v.PlaneSlice == 0).then_some((v.MipSlice, Some(0..1)))
        }
        D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
            // SAFETY: as above.
            let v = unsafe { desc.Anonymous.Texture2DArray };
            let end = if v.ArraySize == u32::MAX { None } else { Some(v.FirstArraySlice.checked_add(v.ArraySize)?) };
            (v.PlaneSlice == 0).then_some((v.MipSlice, end.map(|end| v.FirstArraySlice..end)))
        }
        _ => None,
    }
}

struct Part {
    title_index: u32,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    rows: u32,
}

/// A recorded read-back, landed once its list has run ([`land`]).
pub(crate) struct Back {
    buffer: ID3D12Resource,
    parts: Vec<Part>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

/// Records the copy of `target`'s subresources into a read-back buffer;
/// the host texture is in the copy-source state.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    let host = crate::host::host()?;
    let texture = &target.copy.texture;
    // SAFETY: a live texture the list keeps.
    let desc = unsafe { texture.GetDesc() };
    let mips = target.layout.desc.mips;
    // A host copy with fewer levels than the title's: indices differ; left out.
    if u32::from(desc.MipLevels) != mips {
        return None;
    }
    let mut parts = Vec::with_capacity(target.subresources.len());
    let mut total = 0u64;
    for title_index in target.subresources {
        let offset = total.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))?;
        let (mut footprint, mut rows) = (D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(), 0u32);
        // SAFETY: outputs for one subresource.
        unsafe { host.device.GetCopyableFootprints(&desc, title_index, 1, offset, Some(&mut footprint), Some(&mut rows), None, None) };
        // The tiler reads whole pitched rows, the last one's padding too.
        let f = footprint.Footprint;
        total = footprint.Offset.checked_add(u64::from(f.RowPitch) * u64::from(rows) * u64::from(f.Depth))?;
        parts.push(Part { title_index, footprint, rows });
    }
    if parts.is_empty() {
        return None;
    }
    let buffer = host.read_back_buffer(total)?;
    for part in &parts {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: part.footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: part.title_index },
        };
        // SAFETY: an open list; the texture is a copy source.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    let ticket = track(target.at, target.end);
    Some(Back { buffer, parts, layout: target.layout, ticket })
}

/// Read-backs landed and dropped as stale, since the start.
pub(crate) static LANDED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Once its list has run: the texels tiled into the texture's title memory.
pub(crate) fn land(back: &Back) {
    let (at, end) = (back.ticket.at, back.ticket.end);
    let size = end - at;
    // Lock order: the mapping, then the tickets (a remap waits for us).
    let _mapping = crate::watch::title_memory_reading_range(at..end);
    let _tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    if !back.ticket.valid.load(Ordering::Acquire) {
        LANDED[1].fetch_add(1, Ordering::Relaxed);
        return;
    }
    // What the parts do not cover keeps the title's bytes.
    let mut tiled = vec![0u8; size];
    if !crate::host::read_into(at as *const u8, &mut tiled) {
        return;
    }
    let mut data = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run.
    if unsafe { back.buffer.Map(0, None, Some(&mut data)) }.is_err() || data.is_null() {
        return;
    }
    let mips = back.layout.desc.mips;
    let mut tiled_all = true;
    for part in &back.parts {
        let f = part.footprint.Footprint;
        let (pitch, depth_pitch) = (f.RowPitch as usize, f.RowPitch as usize * part.rows as usize);
        // SAFETY: the part's bytes inside the mapped buffer (sized in `record`).
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(part.footprint.Offset as usize), depth_pitch * f.Depth as usize) };
        let (mip, slice) = (part.title_index % mips, part.title_index / mips);
        if let Err(e) = back.layout.tile(&mut tiled, mip, slice, linear, pitch, depth_pitch) {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled"));
            tiled_all = false;
            break;
        }
    }
    // SAFETY: mapped above.
    unsafe { back.buffer.Unmap(0, None) };
    if !tiled_all {
        return;
    }
    crate::host::copy_mapped(at as *mut u8, tiled.as_ptr(), size, true);
    let landed = LANDED[0].fetch_add(1, Ordering::Relaxed);
    if landed < 8 || (landed + 1).is_power_of_two() {
        tracing::info!(
            n = landed + 1,
            stale = LANDED[1].load(Ordering::Relaxed),
            frame = crate::frame::current(),
            at = format_args!("{at:#x}"),
            bytes = size,
            layout = ?back.layout.desc,
            "a texture read-back landed in title memory (VICTORY_TEXTURE_BACK)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_names_the_kinds() {
        assert!(parse_kinds(None).is_empty());
        assert!(parse_kinds(Some("0")).is_empty());
        assert_eq!(parse_kinds(Some("1")).len(), 4);
        assert!(parse_kinds(Some("1")).contains(&(256, 256, 41)));
        assert_eq!(parse_kinds(Some("8x4f7, bad, 2x2f3")), vec![(8, 4, 7), (2, 2, 3)]);
    }

    #[test]
    fn only_read_backs_over_changed_memory_go_stale() {
        let first = track(0x1000, 0x2000);
        let next = track(0x2000, 0x3000);
        let far = track(0x4000, 0x5000);
        remapped(0x0f00, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        released(0x4000);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        assert!(track(0x1000, 0x2000).valid.load(Ordering::Acquire));
    }
}

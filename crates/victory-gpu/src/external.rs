//! Host work that a host library records into a title's command list in
//! place of a console one (the host's FidelityFX upscaler, say): the host
//! device, a title texture's host copy in the state the library is told,
//! and the host list itself.
//!
//! Ported from ka1qux/BRVIctory-ka1qux, `crates/victory-gpu/src/external.rs`
//! (commit cea1b44df, read at 6a7e871d43; GPL-3.0), 2026-11-26. There it is
//! the upstream project's `external` module (Marvisak/VIctory, GPL-3.0),
//! with the input preparation, the output bookkeeping and the settling
//! before a library lets go of its resources taken from DexNom/VIctory
//! (GPL-3.0). Changed here: a title-written input is uploaded with this
//! tree's `texture::upload` (all its levels, as a shader's read does), and
//! the settling first runs what the queues hold (`VICTORY_ASYNC_SUBMIT`).

use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource,
};

use crate::object::Object;

/// The host D3D12 device, created on first use; `None` without a host GPU.
pub fn host_device() -> Option<ID3D12Device> {
    crate::host::host().map(|h| h.device.clone())
}

/// A title texture's host copy, moved to the host resource `state` in the
/// title list `list`, and the state it is in: `state` without the bits the
/// host copy cannot take (see `host::legal_state`). What the list records
/// next finds it there and must leave it there. The list holds the texture
/// until it is reset. `None` for a buffer, or without a host list.
pub fn host_texture(
    list: *const Object,
    resource: *const Object,
    state: u32,
) -> Option<(ID3D12Resource, u32)> {
    let host = crate::list::host_list(list)?;
    let texture = crate::resource::host_resource(resource)?.clone();
    crate::list::ensure_state(host, resource, state);
    host.keep(&texture);
    let state = crate::host::legal_state(&texture, state);
    Some((texture, state))
}

/// Brings a texture the title made and a host library reads up to date, the
/// way a compute shader's read of it would: a texture placed over another
/// one's memory is copied from it on the host, a texture the title wrote
/// itself is uploaded from its memory, and a depth texture shaders also
/// write gets what they wrote. `None` for a buffer or without a host list.
pub fn prepare_input(list: *const Object, resource: *const Object) -> Option<()> {
    let list = crate::list::host_list(list)?;
    let texture = crate::resource::host_resource(resource)?;
    let aliased = crate::resource::alias_of(resource)
        .is_some_and(|source| crate::texture::alias(list, resource, source));
    if !aliased && crate::texture::title_written(texture) {
        crate::texture::upload(list, resource, texture);
    }
    list.twin_use(resource, false, false);
    Some(())
}

/// A host library recorded the contents of `resource` on the host's list:
/// the title's memory no longer has them, and is not read back over them.
pub fn output_written(resource: *const Object) {
    crate::texture::host_written(resource);
}

/// Settles what the title has submitted so far, before a host library lets
/// go of GPU resources that work in flight may still use. With
/// `VICTORY_ASYNC_SUBMIT` what the queues hold runs first, as before a
/// change of `crate::ab` variant: a list still in a queue's line names them
/// too.
pub fn finish_submitted() {
    crate::queue::before_sync();
    crate::host::settle_for_fence();
}

/// Runs `f` with the host list behind the title list `list` (open for
/// recording); `None` without one. What `f` binds (descriptor heaps, root
/// signatures, pipeline) is replaced before the list's next draw or
/// dispatch.
pub fn record<R>(
    list: *const Object,
    f: impl FnOnce(&ID3D12GraphicsCommandList) -> R,
) -> Option<R> {
    let mut out = None;
    crate::list::with_state(list, |host, state| {
        out = Some(f(&host.list));
        state.host_state_lost();
    });
    out
}

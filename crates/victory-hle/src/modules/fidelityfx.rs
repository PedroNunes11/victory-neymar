//! `amd_fidelityfx_loader_xbox.dll`: AMD FidelityFX's API, served by the
//! host's PC build of it on the host GPU when the user supplies one (the SDK's
//! `amd_fidelityfx_upscaler_dx12.dll`, see [`search_dirs`] for where it is
//! looked for). Without it, or if it offers no upscaler for the host GPU,
//! this module stays out of the registry and the title's own console DLLs run
//! as before. Nothing is downloaded and no DLL ships with this project.
//!
//! Both builds have the same API (the FidelityFX SDK's, MIT): five functions
//! over chains of descriptions, each starting with a type id and the next
//! one's address; contexts are opaque pointers. The console-specific parts
//! are translated: the backend description names the console device (type
//! 0x0100_0002; on PC type 2 with the host device), and a dispatch names the
//! title's command list and resources (their host list and host copies, in
//! the states the title gives, see `victory_gpu::external`). The rest passes
//! through unchanged: sizes, flags, jitter, motion-vector scale, camera
//! values, the resource descriptions, the context. Only the upscaler: other
//! effects the title asks for (frame generation) are refused.
//!
//! Ported from ka1qux/BRVIctory-ka1qux,
//! `crates/victory-hle/src/modules/fidelityfx.rs` (commit cea1b44df, with
//! the ultra-performance preset of 55e9c246e; read at 6a7e871d43; GPL-3.0),
//! 2026-11-26. There the module is the upstream project's (Marvisak/VIctory,
//! GPL-3.0) with the context registry, the device substitution in queries,
//! the checks of a dispatch against what its context was made for, the
//! input preparation and the settling before a context is destroyed taken
//! from DexNom/VIctory (GPL-3.0). The names and layouts of the descriptions
//! are those of the FidelityFX SDK's `ffx_api.h` and `ffx_upscale.h` (MIT,
//! AMD). Left out here: the title profile that makes a Series X title pick
//! FidelityFX at a preset's render size (`apps/victory/src/title_profile.rs`
//! there); a preset only names the size the first upscaling is checked
//! against.
#![allow(non_snake_case)]
// Title code calls these through its import table.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, OnceLock};

use victory_gpu::object::Object;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::core::Interface;

use crate::{HleModule, hle_export};

/// The PC build's upscaler, which has the five functions itself (FidelityFX
/// SDK, `Kits/FidelityFX/signedbin`).
pub const UPSCALER_DLL: &str = "amd_fidelityfx_upscaler_dx12.dll";

// Return codes.
const OK: u32 = 0;
const UNKNOWN_TYPE: u32 = 2;
const NO_PROVIDER: u32 = 4;
const PARAMETER: u32 = 6;

// Description types.
const CREATE_UPSCALE: u64 = 0x1_0000;
const DISPATCH_UPSCALE: u64 = 0x1_0001;
const CONSOLE_BACKEND: u64 = 0x100_0002;
const HOST_BACKEND: u64 = 2;
const GET_VERSIONS: u64 = 4;
const GET_PROVIDER_VERSION: u64 = 6;
const MEMORY_USAGE_WITH_DEVICE: u64 = 0x1_0009;
const GET_RENDER_RESOLUTION: u64 = 0x1_0003;
/// `FFX_UPSCALE_ENABLE_DISPLAY_RESOLUTION_MOTION_VECTORS`.
const DISPLAY_RESOLUTION_MOTION_VECTORS: u32 = 1 << 1;

/// What AMD's documentation calls the named scaling modes: per-dimension
/// factors 1.5 (Quality), 1.7 (Balanced), 2.0 (Performance), 3.0 (Ultra
/// Performance) (FidelityFX SDK, `docs/techniques/super-resolution-ml.md`,
/// "Scaling modes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl Preset {
    /// `quality`, `balanced`, `performance` or `ultra-performance` (also
    /// `ultra_performance` or `ultra`), in any case.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "quality" => Some(Self::Quality),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            "ultra-performance" | "ultra_performance" | "ultra" => Some(Self::UltraPerformance),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::UltraPerformance => "ultra-performance",
        }
    }

    /// The SDK's `FFX_UPSCALE_QUALITY_MODE_*` value.
    pub fn quality_mode(self) -> u32 {
        match self {
            Self::Quality => 1,
            Self::Balanced => 2,
            Self::Performance => 3,
            Self::UltraPerformance => 4,
        }
    }

    /// The per-dimension factor the output is larger than the input by.
    pub fn ratio(self) -> f32 {
        match self {
            Self::Quality => 1.5,
            Self::Balanced => 1.7,
            Self::Performance => 2.0,
            Self::UltraPerformance => 3.0,
        }
    }

    /// The share of the output size the title renders at.
    pub fn scale(self) -> f32 {
        1.0 / self.ratio()
    }

    /// The render size for `output`, as the SDK's render-resolution query
    /// rounds it (down).
    pub fn render_size(self, output: [u32; 2]) -> [u32; 2] {
        output.map(|n| (n as f32 * self.scale()) as u32)
    }
}

/// What the user asked of host FSR (`VICTORY_FSR`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Unset: the host's FidelityFX serves the title's own FidelityFX calls
    /// (what Series S makes) when the user supplied the DLLs.
    Auto,
    /// `off` or `0`: never, whatever DLLs are there.
    Off,
    /// A preset: as `Auto`, and the first upscaling says whether the title
    /// renders at the preset's size. Making a title that does not pick
    /// FidelityFX itself (Series X) pick it is not in this tree.
    Preset(Preset),
}

impl Request {
    /// Reads `VICTORY_FSR`-style text: `None` (unset) is [`Request::Auto`].
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::Auto);
        };
        if value.eq_ignore_ascii_case("off") || value == "0" {
            return Ok(Self::Off);
        }
        Preset::from_name(value).map(Self::Preset).ok_or_else(|| {
            format!("VICTORY_FSR={value}: expected quality, balanced, performance, ultra-performance or off")
        })
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var("VICTORY_FSR").ok().as_deref())
    }
}

/// The folders searched for the DLLs, in order: the configured one
/// (`VICTORY_FIDELITYFX_DIR`, `[paths].fidelityfx_dir`), then a `fidelityfx`
/// folder beside `victory.exe`. Each also by its `signedbin` and
/// `Kits/FidelityFX/signedbin` subfolders, so the SDK's root can be named.
pub fn search_dirs(configured: Option<&Path>, exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let roots = configured
        .map(Path::to_path_buf)
        .into_iter()
        .chain(exe_dir.map(|d| d.join("fidelityfx")));
    roots
        .flat_map(|root| {
            [
                root.clone(),
                root.join("signedbin"),
                root.join("Kits").join("FidelityFX").join("signedbin"),
            ]
        })
        .collect()
}

/// The first folder of [`search_dirs`] that has `dll`.
pub fn locate(dll: &str, configured: Option<&Path>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    search_dirs(configured, exe_dir)
        .into_iter()
        .find(|dir| dir.join(dll).is_file())
}

// What the title passes and what the PC build expects are the same shapes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    kind: u64,
    next: *mut Header,
}

/// An upscaler context: flags, largest render and output sizes, a message
/// callback.
#[repr(C)]
#[derive(Clone, Copy)]
struct CreateUpscale {
    header: Header,
    flags: u32,
    max_render: [u32; 2],
    max_output: [u32; 2],
    message: usize,
}

/// The backend description: a device.
#[repr(C)]
struct Backend {
    header: Header,
    device: *mut c_void,
}

/// A resource, its description (type, format, sizes, mips, flags, usage)
/// and the state it is in.
#[repr(C)]
#[derive(Clone, Copy)]
struct Resource {
    resource: *mut c_void,
    description: [u32; 8],
    state: u32,
}

/// One upscaling: the command list, then colour, depth, motion vectors,
/// exposure, reactive mask, transparency mask and output; then jitter,
/// motion-vector scale, sizes, sharpening, frame time, pre-exposure, reset,
/// camera and flags, which pass through.
#[repr(C)]
#[derive(Clone, Copy)]
struct DispatchUpscale {
    header: Header,
    list: *mut c_void,
    resources: [Resource; 7],
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render_size: [u32; 2],
    upscale_size: [u32; 2],
    settings: [u32; 10],
}

const _: () = assert!(size_of::<CreateUpscale>() == 0x30);
const _: () = assert!(size_of::<Resource>() == 0x30);
const _: () = assert!(size_of::<DispatchUpscale>() == 0x1b0);

/// The versions a backend offers: count in and out, ids, names.
#[repr(C)]
#[derive(Clone, Copy)]
struct GetVersions {
    header: Header,
    create_kind: u64,
    device: *mut c_void,
    count: *mut u64,
    ids: *mut u64,
    names: *mut *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryUsageWithDevice {
    header: Header,
    device: *mut c_void,
    max_render: [u32; 2],
    max_output: [u32; 2],
    flags: u32,
    usage: *mut c_void,
}

#[repr(C)]
struct ProviderVersion {
    header: Header,
    id: u64,
    name: *const c_char,
}

type Create = unsafe extern "C" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
/// Configure, query and dispatch.
type Call = unsafe extern "C" fn(*mut *mut c_void, *mut Header) -> u32;

/// The five functions of the API.
#[derive(Clone, Copy)]
struct Entries {
    create: Create,
    destroy: Destroy,
    configure: Call,
    query: Call,
    dispatch: Call,
}

struct Library(windows_sys::Win32::Foundation::HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this owns the reference acquired by LoadLibraryExW.
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.0);
        }
    }
}

/// The host's FidelityFX: its entries and the host device it works on.
struct Host {
    entries: Entries,
    /// The host device the entries are told (a fake in the tests).
    device: *mut c_void,
    /// Settles what the title submitted, before a context is destroyed.
    settle: fn(),
    _device: Option<ID3D12Device>,
    _library: Option<Library>,
}

// SAFETY: D3D12 devices are free-threaded; the rest is code.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

static HOST: OnceLock<Host> = OnceLock::new();
/// The preset the user asked for, to say whether the title's render size is
/// the one it implies.
static EXPECTED: OnceLock<Preset> = OnceLock::new();

struct Context {
    // Serializes recording and destruction for this particular temporal history.
    live: Mutex<bool>,
    first_dispatch: std::sync::Once,
    // The FFX API requires pointers in the creation descriptor to remain
    // live through destroy, including our replacement backend.
    creation: CreateChain,
}

struct CreateChain {
    description: Box<CreateUpscale>,
    _backend: Box<Backend>,
}

// SAFETY: after Create, both boxed descriptors are retained immutably.
// The device is held by Host; the remaining extension pointers belong
// to the title under the FFX context lifetime contract.
unsafe impl Send for CreateChain {}
// SAFETY: as above, no thread mutates the retained descriptors.
unsafe impl Sync for CreateChain {}

static CONTEXTS: Mutex<BTreeMap<usize, Arc<Context>>> = Mutex::new(BTreeMap::new());

fn fits(extent: [u32; 2], maximum: [u32; 2]) -> bool {
    extent[0] != 0 && extent[1] != 0 && extent[0] <= maximum[0] && extent[1] <= maximum[1]
}

/// The output size of a dispatch (0 means the context's largest), if the
/// render and output sizes lie within what the context was made for.
fn dispatch_output(
    render: [u32; 2],
    output: [u32; 2],
    max_render: [u32; 2],
    max_output: [u32; 2],
) -> Option<[u32; 2]> {
    let output = if output == [0, 0] { max_output } else { output };
    (fits(render, max_render) && fits(output, max_output) && fits(render, output)).then_some(output)
}

/// The smallest size resource `index` of a dispatch must cover.
fn minimum_extent(index: usize, flags: u32, render: [u32; 2], output: [u32; 2]) -> [u32; 2] {
    match index {
        6 => output,
        2 if flags & DISPLAY_RESOLUTION_MOTION_VECTORS != 0 => output,
        3 => [1, 1], // Optional exposure texture.
        _ => render,
    }
}

macro_rules! once {
    ($level:ident, $($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::$level!($($arg)*));
    }};
}

/// Serves the title's FidelityFX calls with the PC upscaler found by
/// [`search_dirs`] if it offers one for the host GPU; else the title's own
/// DLLs run, with one line in the log saying why. Call once, before the
/// import registry is first used: it holds this module only when serving.
pub fn use_host(configured: Option<&Path>, exe_dir: Option<&Path>, request: Request) {
    let preset = match request {
        Request::Off => {
            tracing::info!("FidelityFX: VICTORY_FSR=off, the title's own DLLs run");
            return;
        }
        Request::Preset(preset) => Some(preset),
        Request::Auto => None,
    };
    let Some(dir) = locate(UPSCALER_DLL, configured, exe_dir) else {
        let searched = search_dirs(configured, exe_dir);
        if configured.is_some() || preset.is_some() {
            tracing::warn!(
                searched = ?searched,
                "FidelityFX: {UPSCALER_DLL} is not in the folders searched \
                 (VICTORY_FIDELITYFX_DIR, or a fidelityfx folder beside victory.exe); the title's own DLLs run"
            );
        } else {
            tracing::info!(
                "no FidelityFX folder (VICTORY_FIDELITYFX_DIR or a fidelityfx folder beside victory.exe): the title's own DLLs run"
            );
        }
        return;
    };
    match load(&dir) {
        Ok((host, versions)) => {
            tracing::info!(folder = %dir.display(), versions, "FidelityFX: the host's upscaler serves the title's");
            if let Some(preset) = preset {
                let _ = EXPECTED.set(preset);
            }
            let _ = HOST.set(host);
        }
        Err(why) => tracing::warn!(
            folder = %dir.display(),
            "FidelityFX: {why}; the title's own DLLs run"
        ),
    }
}

/// Whether the host's FidelityFX serves the title's.
pub fn serving() -> bool {
    HOST.get().is_some()
}

fn load(dir: &Path) -> Result<(Host, String), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        LoadLibraryExW,
    };
    // The search flags below want a full path.
    let path = std::path::absolute(dir.join(UPSCALER_DLL)).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!("no {UPSCALER_DLL} there"));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // Its own folder and System32 only, never the current folder.
    let search = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32;
    // SAFETY: a NUL-terminated path to the user's FidelityFX build.
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), search) };
    if module.is_null() {
        return Err(format!(
            "{UPSCALER_DLL} did not load ({})",
            std::io::Error::last_os_error()
        ));
    }
    let library = Library(module);
    let find = |name: &CStr| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .map(|f| f as *const c_void)
            .ok_or_else(|| format!("{UPSCALER_DLL} has no {}", name.to_string_lossy()))
    };
    let device = victory_gpu::external::host_device().ok_or("there is no host GPU")?;
    // SAFETY: the API's exports, with its signatures.
    let entries = unsafe {
        Entries {
            create: std::mem::transmute::<*const c_void, Create>(find(c"ffxCreateContext")?),
            destroy: std::mem::transmute::<*const c_void, Destroy>(find(c"ffxDestroyContext")?),
            configure: std::mem::transmute::<*const c_void, Call>(find(c"ffxConfigure")?),
            query: std::mem::transmute::<*const c_void, Call>(find(c"ffxQuery")?),
            dispatch: std::mem::transmute::<*const c_void, Call>(find(c"ffxDispatch")?),
        }
    };
    let host = Host {
        entries,
        device: device.as_raw(),
        settle: victory_gpu::external::finish_submitted,
        _device: Some(device),
        _library: Some(library),
    };
    let versions = versions(&host)?;
    Ok((host, versions))
}

/// The upscaler versions the host DLL offers for the host GPU, by name; an
/// error if none.
fn versions(host: &Host) -> Result<String, String> {
    let mut count = 0u64;
    let mut query = GetVersions {
        header: Header {
            kind: GET_VERSIONS,
            next: null_mut(),
        },
        create_kind: CREATE_UPSCALE,
        device: host.device,
        count: &mut count,
        ids: null_mut(),
        names: null_mut(),
    };
    // SAFETY: a version query naming the host device.
    let code = unsafe { (host.entries.query)(null_mut(), &mut query.header) };
    if code != OK || count == 0 {
        return Err(format!(
            "it offers no upscaler for the host GPU (code {code})"
        ));
    }
    let mut names = vec![null::<c_char>(); count as usize];
    query.names = names.as_mut_ptr();
    // SAFETY: as above, with room for `count` names.
    let code = unsafe { (host.entries.query)(null_mut(), &mut query.header) };
    if code != OK {
        return Err(format!(
            "upscaler version names could not be queried (code {code})"
        ));
    }
    Ok(names
        .iter()
        .filter(|n| !n.is_null())
        // SAFETY: NUL-terminated names the DLL keeps.
        .map(|&n| unsafe { CStr::from_ptr(n) }.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", "))
}

/// Puts the host backend after the title's upscaler description, in place
/// of the console's backend that follows it (where the title puts it);
/// anywhere else the console's is left for the host DLL to ignore, ours
/// coming first.
fn link(upscale: &mut CreateUpscale, backend: &mut Backend) {
    let after = upscale.header.next;
    // SAFETY: the title's chain.
    backend.header.next = match unsafe { after.as_ref() } {
        Some(h) if h.kind == CONSOLE_BACKEND => h.next,
        _ => after,
    };
    backend.header.kind = HOST_BACKEND;
    upscale.header.next = &raw mut backend.header;
}

/// A FidelityFX resource state as the host resource state it means (as the
/// PC backend reads it); `None` for generic read and indirect arguments,
/// which a host texture cannot be in.
fn host_state(state: u32) -> Option<u32> {
    Some(match state {
        0x1 | 0x80 => 0,  // common, present
        0x2 => 0x8,       // unordered access
        0x4 => 0x40,      // compute read
        0x8 => 0x80,      // pixel read
        0xc => 0xc0,      // both
        0x10 => 0x800,    // copy source
        0x20 => 0x400,    // copy destination
        0x100 => 0x4,     // render target
        0x200 => 0x10,    // depth write
        _ => return None, // generic read, indirect arguments
    })
}

fn create_context(
    host: &Host,
    context: *mut *mut c_void,
    desc: *mut Header,
    memory: *const c_void,
) -> u32 {
    if context.is_null() || desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes a chain of descriptions.
    let kind = unsafe { (*desc).kind };
    if kind != CREATE_UPSCALE {
        once!(
            warn,
            kind = format_args!("{kind:#x}"),
            "FidelityFX: only the upscaler runs on the host; another effect was refused"
        );
        return NO_PROVIDER;
    }
    // SAFETY: an upscaler description, by its type.
    let mut upscale = Box::new(unsafe { desc.cast::<CreateUpscale>().read() });
    let mut backend = Box::new(Backend {
        header: Header {
            kind: HOST_BACKEND,
            next: null_mut(),
        },
        device: host.device,
    });
    link(&mut upscale, &mut backend);
    // SAFETY: a PC chain; the title's later descriptions live for the call.
    let code = unsafe { (host.entries.create)(context, &mut upscale.header, memory) };
    if code != OK {
        tracing::error!(code, render = ?upscale.max_render, output = ?upscale.max_output, flags = format_args!("{:#x}", upscale.flags), "FidelityFX: the host refused the title's upscaler");
        return code;
    }
    let mut version = ProviderVersion {
        header: Header {
            kind: GET_PROVIDER_VERSION,
            next: null_mut(),
        },
        id: 0,
        name: null(),
    };
    // SAFETY: a provider query on the context just made.
    let query_code = unsafe { (host.entries.query)(context, &mut version.header) };
    // SAFETY: a NUL-terminated name the DLL keeps, if any.
    let name = (!version.name.is_null())
        .then(|| unsafe { CStr::from_ptr(version.name) }.to_string_lossy());
    tracing::info!(
        provider = name.as_deref().unwrap_or("?"),
        query_code,
        id = format_args!("{:#x}", version.id),
        render = ?upscale.max_render,
        output = ?upscale.max_output,
        flags = format_args!("{:#x}", upscale.flags),
        "FidelityFX: upscaler context on the host"
    );
    // SAFETY: a successful SDK creation wrote an opaque context into this slot.
    let handle = unsafe { *context } as usize;
    CONTEXTS.lock().unwrap_or_else(|e| e.into_inner()).insert(
        handle,
        Arc::new(Context {
            live: Mutex::new(true),
            first_dispatch: std::sync::Once::new(),
            creation: CreateChain {
                description: upscale,
                _backend: backend,
            },
        }),
    );
    OK
}

fn destroy_context(host: &Host, context: *mut *mut c_void, memory: *const c_void) -> u32 {
    if context.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes its context. Capture its value before destroy clears it.
    let handle = unsafe { *context } as usize;
    let ctx = CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle)
        .cloned();
    let Some(ctx) = ctx else { return PARAMETER };
    let mut live = ctx.live.lock().unwrap_or_else(|e| e.into_inner());
    if !*live {
        return PARAMETER;
    }
    // The title releases a context after submitting its frames. The host
    // may still be executing their lists, even after the title's Execute
    // returns.
    (host.settle)();
    // SAFETY: the host's context, made by `create_context`.
    let code = unsafe { (host.entries.destroy)(context, memory) };
    if code == OK {
        *live = false;
        let mut contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
        // A concurrent Create may already have reused the released SDK address.
        if contexts
            .get(&handle)
            .is_some_and(|current| Arc::ptr_eq(current, &ctx))
        {
            contexts.remove(&handle);
        }
    }
    code
}

fn query_context(host: &Host, context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes a description.
    let kind = unsafe { (*desc).kind };
    // Substitute the native device; the SDK returns values through the
    // title's original output pointers in both request structures.
    if kind == GET_VERSIONS {
        // SAFETY: a version query, by its type.
        let mut query = unsafe { *desc.cast::<GetVersions>() };
        query.device = host.device;
        // SAFETY: the same query naming the host device.
        return unsafe { (host.entries.query)(context, &mut query.header) };
    }
    if kind == MEMORY_USAGE_WITH_DEVICE {
        // SAFETY: a memory query, by its type.
        let mut query = unsafe { *desc.cast::<MemoryUsageWithDevice>() };
        query.device = host.device;
        // SAFETY: the same query naming the host device.
        return unsafe { (host.entries.query)(context, &mut query.header) };
    }
    // SAFETY: as above.
    let code = unsafe { (host.entries.query)(context, desc) };
    if kind == GET_RENDER_RESOLUTION && code != OK {
        tracing::warn!(
            code,
            "FidelityFX provider refused the render-resolution query"
        );
    }
    code
}

fn dispatch_context(host: &Host, context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes a description.
    let kind = unsafe { (*desc).kind };
    if kind != DISPATCH_UPSCALE {
        once!(
            warn,
            kind = format_args!("{kind:#x}"),
            "FidelityFX: only upscaling runs on the host; a dispatch was refused"
        );
        return UNKNOWN_TYPE;
    }
    // SAFETY: an upscaling, by its type.
    let mut dispatch = unsafe { desc.cast::<DispatchUpscale>().read() };
    if context.is_null() {
        once!(warn, "FidelityFX dispatch arrived without a context");
        return PARAMETER;
    }
    if let Some(index) = [0, 1, 2, 6]
        .into_iter()
        .find(|&i| dispatch.resources[i].resource.is_null())
    {
        once!(
            warn,
            index,
            "FidelityFX dispatch is missing a required texture"
        );
        return PARAMETER;
    }
    // SAFETY: the title passes the same opaque context slot as for creation.
    let handle = unsafe { *context } as usize;
    let ctx = CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle)
        .cloned();
    let Some(ctx) = ctx else {
        once!(
            warn,
            "FidelityFX dispatch referenced an unregistered context"
        );
        return PARAMETER;
    };
    let live = ctx.live.lock().unwrap_or_else(|e| e.into_inner());
    if !*live {
        return PARAMETER;
    }
    let create = &ctx.creation.description;
    let Some(output_size) = dispatch_output(
        dispatch.render_size,
        dispatch.upscale_size,
        create.max_render,
        create.max_output,
    ) else {
        once!(warn, render = ?dispatch.render_size, output = ?dispatch.upscale_size,
            max_render = ?create.max_render, max_output = ?create.max_output,
            "FidelityFX rejected dispatch dimensions outside the context's allocations");
        return PARAMETER;
    };
    let list = dispatch.list.cast::<Object>().cast_const();
    let output = dispatch.resources[6].resource.cast::<Object>();
    // Alive for the call (the list holds them too).
    let mut textures = Vec::new();
    let mut backing_extents = [None; 7];
    for (index, r) in dispatch.resources.iter_mut().enumerate() {
        if r.resource.is_null() {
            continue;
        }
        if index < 6
            && victory_gpu::external::prepare_input(list, r.resource.cast::<Object>()).is_none()
        {
            once!(
                warn,
                index,
                "FidelityFX input could not be prepared on the title command list"
            );
            return PARAMETER;
        }
        let wanted = host_state(r.state);
        let texture = wanted.and_then(|w| {
            victory_gpu::external::host_texture(list, r.resource.cast::<Object>(), w)
                .filter(|&(_, state)| state == w)
        });
        let Some((texture, _)) = texture else {
            once!(
                warn,
                index,
                state = r.state,
                "FidelityFX: a resource has no host texture in its state; the upscaling did not run"
            );
            return PARAMETER;
        };
        // SAFETY: a live texture.
        let desc = unsafe { texture.GetDesc() };
        backing_extents[index] = Some((desc.Width, desc.Height));
        let needed = minimum_extent(index, create.flags, dispatch.render_size, output_size);
        if desc.Width < u64::from(needed[0]) || desc.Height < needed[1] {
            once!(
                warn,
                index,
                ?needed,
                width = desc.Width,
                height = desc.Height,
                "FidelityFX resource cannot cover the active dispatch rectangle"
            );
            return PARAMETER;
        }
        r.resource = texture.as_raw();
        textures.push(texture);
    }
    once!(info, formats = ?textures.iter().map(|t| {
        // SAFETY: a live texture.
        unsafe { t.GetDesc() }.Format.0
    }).collect::<Vec<_>>(), "FidelityFX: first upscaling, host formats of its textures");
    let Some(code) = victory_gpu::external::record(list, |host_list| {
        dispatch.list = host_list.as_raw();
        // SAFETY: the PC dispatch on host objects; the context is the host's.
        unsafe { (host.entries.dispatch)(context, &raw mut dispatch.header) }
    }) else {
        once!(
            warn,
            "FidelityFX could not record to the title command list"
        );
        return PARAMETER;
    };
    if code == OK {
        victory_gpu::external::output_written(output);
        ctx.first_dispatch.call_once(|| {
            tracing::info!(render = ?dispatch.render_size, output = ?output_size, ?backing_extents,
                jitter = ?dispatch.jitter, motion_vector_scale = ?dispatch.motion_scale,
                "FidelityFX recorded the title's temporal dispatch");
            if let Some(preset) = EXPECTED.get() {
                report_preset(*preset, dispatch.render_size, output_size);
            }
        });
    } else {
        once!(
            warn,
            code,
            "FidelityFX provider rejected the title dispatch"
        );
    }
    code
}

/// Says whether the render size the title upscales from is the one the
/// preset implies for its output size (it is not when the title's own
/// choice was not changed).
fn report_preset(preset: Preset, render: [u32; 2], output: [u32; 2]) {
    let expected = preset.render_size(output);
    if render == expected {
        tracing::info!(preset = preset.name(), ?render, ?output, "FidelityFX: the title renders at the preset's size");
    } else {
        tracing::warn!(
            preset = preset.name(),
            ?render,
            ?expected,
            ?output,
            "FidelityFX: the title renders at another size than the preset implies"
        );
    }
}

extern "C" fn ffxCreateContext(
    context: *mut *mut c_void,
    desc: *mut Header,
    memory: *const c_void,
) -> u32 {
    HOST.get()
        .map_or(NO_PROVIDER, |host| create_context(host, context, desc, memory))
}

extern "C" fn ffxDestroyContext(context: *mut *mut c_void, memory: *const c_void) -> u32 {
    HOST.get()
        .map_or(NO_PROVIDER, |host| destroy_context(host, context, memory))
}

/// Settings (key-value pairs, debug messages) name nothing console-specific.
extern "C" fn ffxConfigure(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    // SAFETY: the title passes its context and descriptions.
    HOST.get()
        .map_or(NO_PROVIDER, |h| unsafe { (h.entries.configure)(context, desc) })
}

extern "C" fn ffxQuery(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    HOST.get()
        .map_or(NO_PROVIDER, |host| query_context(host, context, desc))
}

extern "C" fn ffxDispatch(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    HOST.get()
        .map_or(NO_PROVIDER, |host| dispatch_context(host, context, desc))
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "fidelityfx",
        dlls: &["amd_fidelityfx_loader_xbox.dll"],
        exports: vec![
            hle_export!(ffxConfigure),
            hle_export!(ffxCreateContext),
            hle_export!(ffxDestroyContext),
            hle_export!(ffxDispatch),
            hle_export!(ffxQuery),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    #[test]
    fn presets_follow_the_sdks_scaling_modes() {
        for (preset, ratio, mode) in [
            (Preset::Quality, 1.5, 1),
            (Preset::Balanced, 1.7, 2),
            (Preset::Performance, 2.0, 3),
            (Preset::UltraPerformance, 3.0, 4),
        ] {
            assert_eq!(preset.ratio(), ratio);
            assert_eq!(preset.quality_mode(), mode);
            assert_eq!(Preset::from_name(preset.name()), Some(preset));
        }
        // What each preset renders at, for the two output sizes of the title.
        assert_eq!(Preset::Quality.render_size([3840, 2160]), [2560, 1440]);
        assert_eq!(Preset::Balanced.render_size([3840, 2160]), [2258, 1270]);
        assert_eq!(Preset::Performance.render_size([3840, 2160]), [1920, 1080]);
        assert_eq!(Preset::UltraPerformance.render_size([3840, 2160]), [1280, 720]);
        assert_eq!(Preset::Quality.render_size([1920, 1080]), [1280, 720]);
        assert_eq!(Preset::from_name(" Quality "), Some(Preset::Quality));
        assert_eq!(Preset::from_name("ultra"), Some(Preset::UltraPerformance));
        assert_eq!(Preset::from_name("native"), None);
    }

    #[test]
    fn the_request_reads_the_switch() {
        assert_eq!(Request::parse(None), Ok(Request::Auto));
        assert_eq!(Request::parse(Some("")), Ok(Request::Auto));
        assert_eq!(Request::parse(Some("off")), Ok(Request::Off));
        assert_eq!(Request::parse(Some("0")), Ok(Request::Off));
        assert_eq!(
            Request::parse(Some("Balanced")),
            Ok(Request::Preset(Preset::Balanced))
        );
        assert!(Request::parse(Some("sharp")).unwrap_err().contains("sharp"));
    }

    #[test]
    fn the_dlls_are_looked_for_beside_the_executable_and_in_the_sdk_layout() {
        let configured = tempfile::tempdir().unwrap();
        let exe = tempfile::tempdir().unwrap();
        assert_eq!(
            locate(UPSCALER_DLL, Some(configured.path()), Some(exe.path())),
            None
        );
        // The SDK's own layout, named by its root.
        let sdk = configured.path().join("Kits/FidelityFX/signedbin");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(sdk.join(UPSCALER_DLL), b"").unwrap();
        assert_eq!(
            locate(UPSCALER_DLL, Some(configured.path()), Some(exe.path())),
            Some(sdk)
        );
        // A `fidelityfx` folder beside the executable, with the configured
        // folder having none: that one.
        let other = tempfile::tempdir().unwrap();
        let beside = exe.path().join("fidelityfx");
        std::fs::create_dir_all(&beside).unwrap();
        std::fs::write(beside.join(UPSCALER_DLL), b"").unwrap();
        assert_eq!(
            locate(UPSCALER_DLL, Some(other.path()), Some(exe.path())),
            Some(beside.clone())
        );
        assert_eq!(locate(UPSCALER_DLL, None, Some(exe.path())), Some(beside));
        // A folder named but empty is not found, and neither is nothing.
        assert_eq!(locate(UPSCALER_DLL, Some(other.path()), None), None);
        assert_eq!(locate(UPSCALER_DLL, None, None), None);
    }

    #[test]
    fn validates_logical_rectangles_against_the_current_context() {
        for display in [[1920, 1080], [2560, 1440], [3840, 2160]] {
            for divisor in [1.0_f32, 1.5, 1.7, 2.0, 3.0] {
                let render = display.map(|v| (v as f32 / divisor) as u32);
                assert_eq!(
                    dispatch_output(render, display, render, display),
                    Some(display)
                );
                assert_eq!(
                    dispatch_output(render, [0, 0], render, display),
                    Some(display)
                );
                assert_eq!(minimum_extent(2, 0x29, render, display), render);
                assert_eq!(minimum_extent(2, 0x2b, render, display), display);
                assert_eq!(minimum_extent(6, 0x29, render, display), display);
                assert_eq!(minimum_extent(3, 0x29, render, display), [1, 1]);
            }
        }
        assert_eq!(
            dispatch_output([0, 720], [0, 0], [1280, 720], [2560, 1440]),
            None
        );
        assert_eq!(
            dispatch_output([1281, 720], [0, 0], [1280, 720], [2560, 1440]),
            None
        );
        assert_eq!(
            dispatch_output([1280, 720], [0, 1440], [1280, 720], [2560, 1440]),
            None
        );
        assert_eq!(
            dispatch_output([1280, 720], [3000, 1440], [1280, 720], [2560, 1440]),
            None
        );
        assert_eq!(
            dispatch_output([1280, 720], [960, 540], [1280, 720], [2560, 1440]),
            None
        );
    }

    fn header(kind: u64, next: *mut Header) -> Header {
        Header { kind, next }
    }

    /// The title's chain: upscaler, console backend, version.
    #[test]
    fn the_console_backend_becomes_the_hosts() {
        let mut version = header(0x1_000b, null_mut());
        let mut console = Backend {
            header: header(CONSOLE_BACKEND, &raw mut version),
            device: 0x1234 as *mut c_void,
        };
        let mut upscale = CreateUpscale {
            header: header(CREATE_UPSCALE, &raw mut console.header),
            flags: 0x29,
            max_render: [1280, 720],
            max_output: [1920, 1080],
            message: 0,
        };
        let mut backend = Backend {
            header: header(0, null_mut()),
            device: 0x5678 as *mut c_void,
        };
        link(&mut upscale, &mut backend);
        assert_eq!(upscale.header.next, &raw mut backend.header);
        assert_eq!(backend.header.kind, HOST_BACKEND);
        assert_eq!(backend.header.next, &raw mut version);
        assert_eq!(backend.device, 0x5678 as *mut c_void);
        // Without a console backend right after it, ours goes in front.
        let mut upscale = CreateUpscale {
            header: header(CREATE_UPSCALE, &raw mut version),
            ..upscale
        };
        link(&mut upscale, &mut backend);
        assert_eq!(backend.header.next, &raw mut version);
    }

    /// Without the PC build the title's own DLL serves its imports.
    #[test]
    fn without_the_dll_the_titles_own_runs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).err().unwrap().contains(UPSCALER_DLL));
        assert!(!serving());
        // Asked for or not, a missing DLL is a line in the log, not an error.
        use_host(Some(dir.path()), None, Request::Preset(Preset::Quality));
        use_host(None, Some(dir.path()), Request::Auto);
        use_host(Some(dir.path()), None, Request::Off);
        assert!(!serving());
        let symbol = victory_pe::ImportSymbol::Name {
            hint: 0,
            name: "ffxCreateContext".into(),
        };
        let resolver = crate::HleResolver::new(crate::HostPolicy::none());
        assert_eq!(
            resolver.classify("amd_fidelityfx_loader_xbox.dll", &symbol),
            None
        );
    }

    #[test]
    fn resource_states_read_as_the_pc_backend_reads_them() {
        assert_eq!(host_state(0xc), Some(0xc0));
        assert_eq!(host_state(0x2), Some(0x8));
        assert_eq!(host_state(0x200), Some(0x10));
        assert_eq!(host_state(0x14), None);
    }

    // A provider of our own standing in for AMD's DLL, which these tests do
    // not have.
    const FAKE_DEVICE: usize = 0x5678;
    /// The fake's counters are shared: the tests using it take turns.
    static FAKE_LOCK: Mutex<()> = Mutex::new(());
    static FAKE_CHAIN_KINDS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static FAKE_DEVICE_SEEN: AtomicUsize = AtomicUsize::new(0);
    static FAKE_DESTROYED: AtomicU32 = AtomicU32::new(0);
    static FAKE_SETTLED: AtomicU32 = AtomicU32::new(0);
    static FAKE_NAME: &CStr = c"4.1.2";

    unsafe extern "C" fn fake_create(
        context: *mut *mut c_void,
        desc: *mut Header,
        _memory: *const c_void,
    ) -> u32 {
        // Record the chain the provider is given, and make a context.
        let mut kinds = FAKE_CHAIN_KINDS.lock().unwrap();
        kinds.clear();
        let mut at = desc;
        while !at.is_null() {
            // SAFETY: the test's chain.
            let h = unsafe { &*at };
            if h.kind == HOST_BACKEND {
                // SAFETY: a backend description, by its type.
                let backend = unsafe { &*at.cast::<Backend>() };
                FAKE_DEVICE_SEEN.store(backend.device as usize, Ordering::SeqCst);
            }
            kinds.push(h.kind);
            at = h.next;
        }
        // SAFETY: the test's context slot.
        unsafe { *context = Box::into_raw(Box::new(0u64)).cast() };
        OK
    }

    unsafe extern "C" fn fake_destroy(context: *mut *mut c_void, _memory: *const c_void) -> u32 {
        FAKE_DESTROYED.fetch_add(1, Ordering::SeqCst);
        // SAFETY: made by `fake_create`.
        unsafe {
            drop(Box::from_raw((*context).cast::<u64>()));
            *context = null_mut();
        }
        OK
    }

    unsafe extern "C" fn fake_call(_context: *mut *mut c_void, desc: *mut Header) -> u32 {
        // SAFETY: the test's description.
        let kind = unsafe { (*desc).kind };
        if kind == GET_PROVIDER_VERSION {
            // SAFETY: a provider version, by its type.
            let version = unsafe { &mut *desc.cast::<ProviderVersion>() };
            version.id = 0x41_0102;
            version.name = FAKE_NAME.as_ptr();
        }
        if kind == GET_VERSIONS {
            // SAFETY: a version query, by its type.
            let query = unsafe { &mut *desc.cast::<GetVersions>() };
            FAKE_DEVICE_SEEN.store(query.device as usize, Ordering::SeqCst);
            // SAFETY: the test's count and names.
            unsafe { *query.count = 1 };
            if !query.names.is_null() {
                // SAFETY: room for the one name.
                unsafe { *query.names = FAKE_NAME.as_ptr() };
            }
        }
        OK
    }

    fn fake_settle() {
        FAKE_SETTLED.fetch_add(1, Ordering::SeqCst);
    }

    fn fake_host() -> Host {
        Host {
            entries: Entries {
                create: fake_create,
                destroy: fake_destroy,
                configure: fake_call,
                query: fake_call,
                dispatch: fake_call,
            },
            device: FAKE_DEVICE as *mut c_void,
            settle: fake_settle,
            _device: None,
            _library: None,
        }
    }

    /// The title's creation, through a provider that is not AMD's: the chain
    /// it gets, the context kept, the settling before it is destroyed.
    #[test]
    fn a_context_is_made_on_the_host_device_and_settled_before_it_is_destroyed() {
        let _turn = FAKE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = fake_host();
        assert_eq!(versions(&host).unwrap(), "4.1.2");
        assert_eq!(FAKE_DEVICE_SEEN.load(Ordering::SeqCst), FAKE_DEVICE);
        let mut version = header(0x1_000b, null_mut());
        let mut console = Backend {
            header: header(CONSOLE_BACKEND, &raw mut version),
            device: 0x1234 as *mut c_void,
        };
        let mut upscale = CreateUpscale {
            header: header(CREATE_UPSCALE, &raw mut console.header),
            flags: 0x29,
            max_render: [1280, 720],
            max_output: [1920, 1080],
            message: 0,
        };
        FAKE_DEVICE_SEEN.store(0, Ordering::SeqCst);
        let mut context: *mut c_void = null_mut();
        assert_eq!(
            create_context(&host, &mut context, &mut upscale.header, null()),
            OK
        );
        assert!(!context.is_null());
        // The provider saw the upscaler, the host backend in the console's
        // place, and the rest of the title's chain.
        assert_eq!(
            *FAKE_CHAIN_KINDS.lock().unwrap(),
            [CREATE_UPSCALE, HOST_BACKEND, 0x1_000b]
        );
        assert_eq!(FAKE_DEVICE_SEEN.load(Ordering::SeqCst), FAKE_DEVICE);
        let handle = context as usize;
        assert!(CONTEXTS.lock().unwrap().contains_key(&handle));
        let (settled, destroyed) = (
            FAKE_SETTLED.load(Ordering::SeqCst),
            FAKE_DESTROYED.load(Ordering::SeqCst),
        );
        assert_eq!(destroy_context(&host, &mut context, null()), OK);
        assert_eq!(FAKE_SETTLED.load(Ordering::SeqCst), settled + 1);
        assert_eq!(FAKE_DESTROYED.load(Ordering::SeqCst), destroyed + 1);
        assert!(!CONTEXTS.lock().unwrap().contains_key(&handle));
    }

    #[test]
    fn other_effects_and_bad_descriptions_are_refused() {
        let _turn = FAKE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = fake_host();
        let mut context: *mut c_void = null_mut();
        let mut other = header(0x2_0001, null_mut());
        assert_eq!(
            create_context(&host, &mut context, &mut other, null()),
            NO_PROVIDER
        );
        assert_eq!(
            create_context(&host, null_mut(), &mut other, null()),
            PARAMETER
        );
        assert_eq!(
            create_context(&host, &mut context, null_mut(), null()),
            PARAMETER
        );
        // A dispatch of another effect, one without a context, one for a
        // context nobody made, one missing a texture.
        assert_eq!(
            dispatch_context(&host, &mut context, &mut other),
            UNKNOWN_TYPE
        );
        let mut dispatch = DispatchUpscale {
            header: header(DISPATCH_UPSCALE, null_mut()),
            list: null_mut(),
            resources: [Resource {
                resource: null_mut(),
                description: [0; 8],
                state: 0,
            }; 7],
            jitter: [0.0; 2],
            motion_scale: [0.0; 2],
            render_size: [1280, 720],
            upscale_size: [1920, 1080],
            settings: [0; 10],
        };
        assert_eq!(
            dispatch_context(&host, null_mut(), &mut dispatch.header),
            PARAMETER
        );
        assert_eq!(
            dispatch_context(&host, &mut context, &mut dispatch.header),
            PARAMETER
        );
        let mut made: *mut c_void = null_mut();
        let mut upscale = CreateUpscale {
            header: header(CREATE_UPSCALE, null_mut()),
            flags: 0,
            max_render: [1280, 720],
            max_output: [1920, 1080],
            message: 0,
        };
        assert_eq!(
            create_context(&host, &mut made, &mut upscale.header, null()),
            OK
        );
        // Textures that are not there (nor a title list to take them from).
        for resource in &mut dispatch.resources[..3] {
            resource.resource = 0x10 as *mut c_void;
            resource.state = 0x4;
        }
        dispatch.resources[6].resource = 0x10 as *mut c_void;
        assert_eq!(
            dispatch_context(&host, &mut made, &mut dispatch.header),
            PARAMETER
        );
        // Sizes beyond what the context was made for.
        dispatch.render_size = [1281, 720];
        assert_eq!(
            dispatch_context(&host, &mut made, &mut dispatch.header),
            PARAMETER
        );
        assert_eq!(destroy_context(&host, &mut made, null()), OK);
        // Destroyed: not twice.
        assert_eq!(destroy_context(&host, &mut made, null()), PARAMETER);
    }
}

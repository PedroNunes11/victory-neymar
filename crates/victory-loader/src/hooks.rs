//! Opt-in compatibility hooks for known title builds.
//!
//! Ported from ZigZagBoi1/ScarlettPort (`runtime/victory`, GPL-3.0-or-later).
//! Only per-build offsets and short byte signatures live here (asset policy:
//! nothing of the title itself). Every hook checks the build's hash and the
//! exact bytes it replaces, and does nothing otherwise.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// First 16 hex digits of the BLAKE3 of the Series X 1.0.2.1 executable.
const BUILD_1_0_2_1: &str = "b99f876b74d4a77f";

/// Environment variable that turns the watchdog hook on.
pub(crate) const NO_WATCHDOG_ENV: &str = "VICTORY_NO_WATCHDOG";

/// One code patch: `rva` must hold exactly `expect` before `new` is written.
struct Patch<'a> {
    what: &'static str,
    rva: usize,
    expect: &'a [u8],
    new: &'a [u8],
}

/// The title's watchdog thread ends the process on purpose when its main
/// thread has not advanced for about 30 one-second ticks. Two places do it:
/// the handler it calls (`mov dword ptr [0], imm32`) and, when the handler
/// returns or is unset, a fallback right after the call (`mov dword ptr [0],
/// 0; int3`). Jumping over the whole block returns the loop to its normal
/// path; the first patch makes the handler harmless if something else calls it.
const WATCHDOG: &[Patch<'static>] = &[
    Patch {
        what: "watchdog: skip the call and the fallback fault",
        rva: 0x2215c27,
        // mov rax, [rip + disp32] (the handler pointer)
        expect: &[0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04],
        // jmp +0x18 (to the loop's normal continuation), then NOPs
        new: &[0xEB, 0x18, 0x90, 0x90, 0x90, 0x90, 0x90],
    },
    Patch {
        what: "watchdog: handler's deliberate fault",
        rva: 0x23cf683,
        expect: &[0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D],
        // an eleven-byte NOP
        new: &[0x66, 0x66, 0x2E, 0x0F, 0x1F, 0x84, 0, 0, 0, 0, 0],
    },
];

/// Applies the watchdog patches whose bytes match. Returns how many applied.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash`, and no title
/// code is running yet.
pub(crate) unsafe fn disarm_watchdog(base: usize, hash: &blake3::Hash) -> usize {
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        return 0;
    }
    WATCHDOG
        .iter()
        // SAFETY: forwarded from this function's contract.
        .filter(|p| unsafe { apply(base, p) })
        .count()
}

/// Environment variable that pins the title's internal render scale
/// (`0.2..=1.0` of the output size). Testing lever, off by default.
pub(crate) const RENDER_SCALE_ENV: &str = "VICTORY_RENDER_SCALE";
/// Environment variable that makes the title use its own temporal upscaler
/// on every console profile (Series S otherwise picks FSR).
pub(crate) const OWN_UPSCALER_ENV: &str = "VICTORY_OWN_UPSCALER";
/// Environment variable that makes the title pick FSR (its FidelityFX path)
/// on every console profile, the Series X one included: the host's FidelityFX
/// then serves it when the user supplied AMD's PC DLLs (frame generation
/// takes that upscale's depth and motion vectors). [`OWN_UPSCALER_ENV`] wins.
pub(crate) const FORCE_FSR_ENV: &str = "VICTORY_FORCE_FSR";

/// A patch whose bytes are computed at load time.
struct OwnedPatch {
    what: &'static str,
    rva: usize,
    expect: Vec<u8>,
    new: Vec<u8>,
}

/// The upscaler choice: `mov eax, 6` (own upscaler) then `cmovne eax, ecx`
/// (FSR when the profile asks for it). NOPing the cmovne keeps 6.
const OWN_UPSCALER: Patch<'static> = Patch {
    what: "upscaler: always the title's own",
    rva: 0x7e9426,
    expect: &[0x0F, 0x45, 0xC1],
    new: &[0x0F, 0x1F, 0x00],
};

/// Read-only float constants of this build usable as the FSR scale:
/// value, RVA.
/// The same choice made FSR's: `mov eax, 7` (the id the code gives FSR,
/// `mov ecx, 7; sbb ecx, 0` before it), and the `cmovne` a three-byte NOP;
/// and FSR's render size made the scaled one: where the code records whether
/// the scene is scaled (`movzx eax, al; mov [r14+0x90], eax; test al, al;
/// jnz +10`) it records FSR's quality mode 1 (Quality: 1.5x, 2560x1440 for
/// 3840x2160, the console's own render size) and always takes the scaled
/// branch. With the choice alone (run 154) FSR was asked to render at
/// 3840x2160 and the title stopped at an assertion while it started.
/// Both places and bytes are those of DexNom/VIctory's
/// `apps/victory/src/native_1440p.rs` `prepare_fsr_4k` (GPL-3.0), which
/// patches the image for its Series X FSR at 4K; Kaique's BRVIctory and
/// Marvisak/VIctory PR 1793 do the same from a profile the user keeps.
const FORCE_FSR: [Patch<'static>; 2] = [
    Patch {
        what: "upscaler: always FSR",
        rva: 0x7e9421,
        expect: &[0xB8, 0x06, 0, 0, 0, 0x0F, 0x45, 0xC1],
        new: &[0xB8, 0x07, 0, 0, 0, 0x0F, 0x1F, 0x00],
    },
    Patch {
        what: "upscaler: FSR renders at the Quality size",
        rva: 0x28ef69a,
        expect: &[0x0F, 0xB6, 0xC0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0x84, 0xC0, 0x75, 0x0A],
        // mov eax, 1; mov [r14+0x90], eax; jmp +10
        new: &[0xB8, 0x01, 0, 0, 0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0xEB, 0x0A],
    },
];

const FSR_SCALES: [(f32, usize); 4] = [
    (0.5, 0x5fc6edc),
    (0.4, 0x5fc6f90),
    (1.0 / 3.0, 0x615a240),
    (0.75, 0x6159b04),
];

/// The render-scale patches for `scale`, with the image at `base`:
/// - the own upscaler's minimum scale (an immediate, 2/3 by default) set to
///   `scale` and its maximum made equal to the minimum (`vmaxss` → `vmovss`),
///   so the scale is fixed;
/// - FSR's scale (a load of 2/3 on Series S) pointed at a constant equal to
///   `scale`, when this build has one, and its maximum size made the scaled
///   size (vtable slot 11 → slot 10's function).
fn render_scale_patches(base: usize, scale: f32, fsr: bool) -> Vec<OwnedPatch> {
    let mut patches = vec![
        OwnedPatch {
            what: "render scale: own upscaler minimum",
            rva: 0x28f1e7e,
            expect: vec![0xAB, 0xAA, 0x2A, 0x3F],
            new: scale.to_le_bytes().to_vec(),
        },
        OwnedPatch {
            what: "render scale: own upscaler maximum = minimum",
            rva: 0x28f1e87,
            expect: vec![0xC5, 0xFA, 0x5F, 0x86, 0x90, 0, 0, 0],
            new: vec![0xC5, 0xFA, 0x10, 0x86, 0x90, 0, 0, 0],
        },
    ];
    if !fsr {
        return patches;
    }
    match FSR_SCALES.iter().find(|(v, _)| (v - scale).abs() < 1e-3) {
        Some(&(_, constant)) => {
            // vmovss xmm0, [rip + disp32]; the next instruction is at +8.
            const LOAD: usize = 0x28ef6b2;
            let disp = (constant as i64 - (LOAD as i64 + 8)) as i32;
            let mut new = vec![0xC5, 0xFA, 0x10, 0x05];
            new.extend_from_slice(&disp.to_le_bytes());
            patches.push(OwnedPatch {
                what: "render scale: FSR scale",
                rva: LOAD,
                expect: vec![0xC5, 0xFA, 0x10, 0x05, 0xA2, 0xE2, 0x86, 0x03],
                new,
            });
            patches.push(OwnedPatch {
                what: "render scale: FSR maximum = scaled size",
                rva: 0x6300ba8,
                expect: ((base + 0x28ef6f0) as u64).to_le_bytes().to_vec(),
                new: ((base + 0x28ef680) as u64).to_le_bytes().to_vec(),
            });
        }
        None => tracing::warn!(
            scale,
            "no FSR constant for this scale (0.5, 0.4, 1/3, 0.75); FSR keeps its own (use {OWN_UPSCALER_ENV}=1)"
        ),
    }
    patches
}

/// The scale `VICTORY_RENDER_SCALE` asks for, if valid.
fn parse_scale(text: &str) -> Option<f32> {
    let v: f32 = text.trim().parse().ok()?;
    (0.2..=1.0).contains(&v).then_some(v)
}

/// Applies the resolution levers asked for in the environment
/// ([`RENDER_SCALE_ENV`], [`OWN_UPSCALER_ENV`]). Returns how many applied.
///
/// # Safety
/// As `disarm_watchdog`.
pub(crate) unsafe fn resolution_levers(base: usize, hash: &blake3::Hash) -> usize {
    let scale = std::env::var(RENDER_SCALE_ENV).ok();
    let own = std::env::var_os(OWN_UPSCALER_ENV).is_some_and(|v| v != "0");
    let force_fsr = std::env::var_os(FORCE_FSR_ENV).is_some_and(|v| v != "0");
    if scale.is_none() && !own && !force_fsr {
        return 0;
    }
    // Every image the loader maps comes here (the title's DLLs too); only
    // the 1.0.2.1 executable has the patched places.
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        tracing::debug!("resolution levers: not the 1.0.2.1 executable; nothing patched");
        return 0;
    }
    let mut applied = 0;
    if own {
        // SAFETY: forwarded from this function's contract.
        applied += usize::from(unsafe { apply(base, &OWN_UPSCALER) });
        if force_fsr {
            tracing::warn!("{FORCE_FSR_ENV} left out: {OWN_UPSCALER_ENV} asks for the title's own upscaler");
        }
    } else if force_fsr {
        // Both or neither: the choice alone stops the title (run 154).
        // SAFETY: forwarded from this function's contract.
        let matched = FORCE_FSR.iter().all(|p| unsafe { matches(base, p) });
        if matched {
            // SAFETY: forwarded from this function's contract.
            applied += FORCE_FSR.iter().filter(|p| unsafe { apply(base, p) }).count();
        } else {
            tracing::warn!("{FORCE_FSR_ENV}: this executable's bytes are not the expected ones; nothing patched");
        }
    }
    if let Some(text) = scale {
        match parse_scale(&text) {
            Some(scale) => {
                // With the own upscaler forced, FSR is left as it is.
                for p in render_scale_patches(base, scale, !own) {
                    let p = Patch {
                        what: p.what,
                        rva: p.rva,
                        expect: &p.expect,
                        new: &p.new,
                    };
                    // SAFETY: forwarded from this function's contract.
                    applied += usize::from(unsafe { apply(base, &p) });
                }
            }
            None => tracing::warn!(text, "{RENDER_SCALE_ENV} wants a number in 0.2..=1.0"),
        }
    }
    applied
}

/// Whether `p`'s place holds the bytes it expects.
///
/// # Safety
/// See `disarm_watchdog`.
unsafe fn matches(base: usize, p: &Patch<'_>) -> bool {
    // SAFETY: the offset is inside the mapped .text of this exact build.
    unsafe { std::slice::from_raw_parts((base + p.rva) as *const u8, p.expect.len()) == p.expect }
}

/// # Safety
/// See `disarm_watchdog`.
unsafe fn apply(base: usize, p: &Patch<'_>) -> bool {
    let at = (base + p.rva) as *mut u8;
    // SAFETY: the offset is inside the mapped .text of this exact build.
    let now = unsafe { std::slice::from_raw_parts(at, p.expect.len()) };
    if now != p.expect || p.new.len() != p.expect.len() {
        tracing::warn!(what = p.what, "hook skipped: the bytes are not the expected ones");
        return false;
    }
    let mut old = 0;
    // SAFETY: changing the protection of those bytes only, restored below.
    if unsafe { VirtualProtect(at as _, p.new.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return false;
    }
    // SAFETY: writable now; the bytes are the ones checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(p.new.as_ptr(), at, p.new.len());
        VirtualProtect(at as _, p.new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as _, p.new.len());
    }
    tracing::info!(what = p.what, "hook applied");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_outside_the_floor_are_refused() {
        assert_eq!(parse_scale("0.5"), Some(0.5));
        assert_eq!(parse_scale(" 1 "), Some(1.0));
        assert_eq!(parse_scale("0.1"), None);
        assert_eq!(parse_scale("1.5"), None);
        assert_eq!(parse_scale("half"), None);
    }

    #[test]
    fn half_scale_patches_both_upscalers() {
        let base = 0x7ff0_0000_0000;
        let p = render_scale_patches(base, 0.5, true);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].new, 0.5f32.to_le_bytes());
        // The FSR load now reads 0.5 at 0x5fc6edc: disp from the next instruction.
        assert_eq!(p[2].new, [0xC5, 0xFA, 0x10, 0x05, 0x22, 0x78, 0x6D, 0x03]);
        assert_eq!(p[3].new, ((base + 0x28ef680) as u64).to_le_bytes());
        assert!(p.iter().all(|p| p.new.len() == p.expect.len()));
    }

    /// Forcing FSR changes only the own upscaler's id (6 to 7) and the
    /// conditional move, over the bytes the own-upscaler patch also expects.
    #[test]
    fn forcing_fsr_rewrites_only_the_choice() {
        let [choice, size] = &FORCE_FSR;
        assert!(FORCE_FSR.iter().all(|p| p.expect.len() == p.new.len()));
        let changed = |p: &Patch<'_>| -> Vec<usize> { (0..p.new.len()).filter(|&i| p.expect[i] != p.new[i]).collect() };
        assert_eq!(changed(choice), [1, 6, 7]);
        assert_eq!(choice.rva + 5, OWN_UPSCALER.rva);
        assert_eq!(&choice.expect[5..], OWN_UPSCALER.expect);
        // The size: `mov eax, 1` (the Quality mode) in place of `movzx eax,
        // al`, the same store into [r14+0x90] after it, the conditional jump
        // made unconditional over the same distance, ending where it ended.
        assert_eq!(&size.new[..5], [0xB8, 1, 0, 0, 0]);
        assert_eq!(&size.new[5..12], &size.expect[3..10]);
        assert_eq!(&size.expect[12..], [0x75, 0x0A]);
        assert_eq!(&size.new[12..], [0xEB, 0x0A]);
    }

    #[test]
    fn odd_scales_patch_only_the_own_upscaler() {
        assert_eq!(render_scale_patches(0, 0.6, true).len(), 2);
        assert_eq!(render_scale_patches(0, 0.5, false).len(), 2);
    }
}

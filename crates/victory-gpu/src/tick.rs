//! Timestamps for the timers taken thousands of times a frame (a list's
//! steps, the refreshes of copies, the slots the descriptor heap sync
//! changes).
//!
//! `std::time::Instant` asks the system's performance counter: 24.5 ns a
//! call on the Ryzen 7 9800X3D here, 53 ns for a timer (its start, its end,
//! the sum). The submitting thread reads it some 200,000 times a frame
//! (~31,000 refreshes and ~20,000 list steps, two or three reads each;
//! ~8,500 slots of the heap sync, up to eleven each), ~5 ms of a frame the
//! card waits for. With
//! `VICTORY_CHEAP_TIMERS=1` (live; off by default) a [`Tick`] reads the
//! processor's time-stamp counter instead: 7.3 ns a call, 15 ns a timer,
//! the same nanoseconds once the counter's rate is known (measured against
//! `Instant` over the first ~200 ms of timers). Only on a processor whose
//! counter keeps one rate in every power state (invariant TSC); elsewhere,
//! and while the rate is not known yet, as before.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// A moment: a counter reading, or (top bit set) nanoseconds since [`base`]
/// through `Instant`. Times between two of different kinds (the switch
/// flipped between them) are 0.
#[derive(Clone, Copy)]
pub(crate) struct Tick(u64);

const BY_INSTANT: u64 = 1 << 63;

/// Whether ticks read the counter now.
fn cheap() -> bool {
    static ON: crate::live::Switch = crate::live::Switch::new("CHEAP_TIMERS", 0);
    ON.on() && rate() != 0
}

/// When and at what counter reading ticks began: what the counter's rate is
/// measured from.
fn base() -> &'static (Instant, u64) {
    static BASE: OnceLock<(Instant, u64)> = OnceLock::new();
    BASE.get_or_init(|| (Instant::now(), counter()))
}

/// Nanoseconds per counter tick, times 2^32; 0 until known (or never, on a
/// processor without an invariant counter).
fn rate() -> u64 {
    static RATE: AtomicU64 = AtomicU64::new(0);
    static INVARIANT: OnceLock<bool> = OnceLock::new();
    let rate = RATE.load(Relaxed);
    if rate != 0 || !*INVARIANT.get_or_init(invariant) {
        return rate;
    }
    let &(began, read) = base();
    let ns = began.elapsed().as_nanos();
    if ns < 200_000_000 {
        return 0;
    }
    let ticks = u128::from(counter().saturating_sub(read)).max(1);
    let rate = ((ns << 32) / ticks).clamp(1, u128::from(u64::MAX)) as u64;
    RATE.store(rate, Relaxed);
    rate
}

/// Whether the counter keeps one rate in every power state (CPUID
/// 8000_0007h, EDX bit 8).
fn invariant() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let top = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        top >= 0x8000_0007 && core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

#[inline]
fn counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: RDTSC exists on every x86-64 processor.
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

impl Tick {
    #[inline]
    pub(crate) fn now() -> Tick {
        if cheap() {
            Tick(counter() & !BY_INSTANT)
        } else {
            Tick(base().0.elapsed().as_nanos() as u64 | BY_INSTANT)
        }
    }

    /// Nanoseconds from `self` to `later` (0 if `later` is earlier).
    #[inline]
    pub(crate) fn ns_to(self, later: Tick) -> u64 {
        match (self.0 & BY_INSTANT, later.0 & BY_INSTANT) {
            (0, 0) => ((u128::from(later.0.saturating_sub(self.0)) * u128::from(rate())) >> 32) as u64,
            (BY_INSTANT, BY_INSTANT) => later.0.saturating_sub(self.0),
            _ => 0,
        }
    }

    /// Nanoseconds since `self`.
    #[inline]
    pub(crate) fn ns(self) -> u64 {
        self.ns_to(Tick::now())
    }

    /// Microseconds since `self`.
    #[inline]
    pub(crate) fn us(self) -> u64 {
        self.ns() / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the counter on, a tick measures what `Instant` measures, once
    /// the counter's rate is known.
    #[test]
    fn counter_ticks_measure_what_instant_does() {
        if !invariant() {
            eprintln!("SKIPPED counter_ticks_measure_what_instant_does: no invariant TSC");
            return;
        }
        let _ = base();
        std::thread::sleep(std::time::Duration::from_millis(220));
        assert_ne!(rate(), 0);
        let (by_counter, by_instant) = (Tick(counter()), Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(30));
        let timed = by_instant.elapsed().as_nanos() as u64;
        let counted = by_counter.ns_to(Tick(counter()));
        let timed_now = by_instant.elapsed().as_nanos() as u64;
        // Ticks of the two kinds (the switch flipped between them): 0.
        assert_eq!(by_counter.ns_to(Tick(timed_now | BY_INSTANT)), 0);
        assert!(counted >= timed * 98 / 100 && counted <= timed_now * 102 / 100, "{counted} ns by the counter, {timed}..{timed_now} by Instant");
    }

    /// Ticks through `Instant` (the switch off) measure as before.
    #[test]
    fn instant_ticks_measure_time() {
        let began = Tick::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ns = began.ns();
        assert!((5_000_000..1_000_000_000).contains(&ns), "{ns}");
    }
}

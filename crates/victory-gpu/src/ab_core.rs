//! In-session A/B: boot once, then cycle variants of live switches
//! ([`crate::live`]) in timed windows and measure each window.
//!
//! - `VICTORY_AB="A:;B:TEXTURE_WIDE=1;C:TEXTURE_WIDE=1,WIDE_REFRESH=1"`: the
//!   variants, in order (`A:` is the boot values). A bare name means `=1`.
//! - `VICTORY_AB_WINDOW_S` (20): seconds per window; the first
//!   `VICTORY_AB_SETTLE_S` (1) of each are not measured.
//! - `VICTORY_AB_WARMUP_S` (30): seconds in the world before the first window.
//! - `VICTORY_AB_CYCLES` (0, no end): stop after this many A..Z cycles.
//!
//! Windows start once the title is in the world (1,500+ draws a frame for
//! 60 frames). A change is asked of the caller ([`Scheduler::frame`]
//! returns the variant), which settles every flight and applies it
//! ([`apply`]) at the frame boundary. One `ab window` line per window; an
//! `ab summary` table after every full cycle and at the end. The logic here
//! is platform-neutral and tested; [`crate::ab`] drives it.
//!
//! Ported from ZigZagBoi1/ScarlettPort, `runtime/victory/crates/victory-core/
//! src/ab.rs` (fxceless/victory-fork lineage, branch `fx-cand4` `efcc7b9`;
//! GPL-3.0-or-later, its authors), 2026-11-26. Changed here: no per-second
//! meter (this fork's frame line is every five seconds): a window measures
//! the frame times between presents only; fast boot's world detector is not
//! here.

use std::fmt::Write as _;

/// One variant: a name and the live switches it sets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Variant {
    pub(crate) name: String,
    pub(crate) sets: Vec<(String, u64)>,
}

/// Parses a schedule; every name must be live ([`crate::live::check`]).
pub(crate) fn parse_schedule(s: &str) -> Result<Vec<Variant>, String> {
    let mut out = Vec::new();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, body) = part.split_once(':').unwrap_or((part, ""));
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("variant without a name: {part:?}"));
        }
        let mut sets = Vec::new();
        // Switches between commas or plus signs (a `+` passes through a
        // run script that splits its own settings at commas).
        for kv in body.split([',', '+']).map(str::trim).filter(|k| !k.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
            let k = crate::live::short(k).to_string();
            crate::live::check(&k).map_err(|e| format!("variant {name}: {e}"))?;
            let v = crate::live::parse(v).ok_or_else(|| format!("variant {name}: {k}={v} is not a number"))?;
            sets.push((k, v));
        }
        if out.iter().any(|o: &Variant| o.name == name) {
            return Err(format!("variant {name} twice"));
        }
        out.push(Variant { name: name.to_string(), sets });
    }
    if out.len() < 2 {
        return Err("an A/B schedule needs two variants or more".into());
    }
    Ok(out)
}

/// Every switch any variant sets is put back to boot, then `v`'s are set.
pub(crate) fn apply(all: &[Variant], v: &Variant) {
    for o in all {
        for (k, _) in &o.sets {
            crate::live::reset(k);
        }
    }
    for (k, val) in &v.sets {
        let _ = crate::live::set(k, *val);
    }
}

/// One measured window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Window {
    pub(crate) variant: usize,
    pub(crate) frames: usize,
    pub(crate) fps: f64,
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) removed: bool,
}

/// Nearest-rank percentile of sorted `v` (`q` in 0..=100).
pub(crate) fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((q / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Median (mean of the middle two for an even count).
pub(crate) fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => 0.0,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

/// Closes a window from its frame times.
pub(crate) fn close(variant: usize, mut ms: Vec<f64>, measured_s: f64, removed: bool) -> Window {
    ms.sort_by(f64::total_cmp);
    Window {
        variant,
        frames: ms.len(),
        fps: if measured_s > 0.0 { ms.len() as f64 / measured_s } else { 0.0 },
        p50: percentile(&ms, 50.0),
        p95: percentile(&ms, 95.0),
        p99: percentile(&ms, 99.0),
        removed,
    }
}

/// The `ab window` line.
pub(crate) fn window_line(v: &[Variant], w: &Window, index: usize) -> String {
    format!(
        "ab window {index} variant={} frames={} fps={:.1} p50={:.2} p95={:.2} p99={:.2} device-removed={}",
        v[w.variant].name,
        w.frames,
        w.fps,
        w.p50,
        w.p95,
        w.p99,
        u8::from(w.removed)
    )
}

/// Per variant over its windows: median of window values, spread (max-min).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) variant: usize,
    pub(crate) windows: usize,
    pub(crate) fps: f64,
    pub(crate) fps_spread: f64,
    pub(crate) p50: f64,
    pub(crate) p50_spread: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) removed: usize,
}

pub(crate) fn aggregate(n_variants: usize, windows: &[Window]) -> Vec<Row> {
    (0..n_variants)
        .map(|i| {
            let ws: Vec<&Window> = windows.iter().filter(|w| w.variant == i).collect();
            let col = |g: fn(&Window) -> f64| ws.iter().map(|w| g(w)).collect::<Vec<_>>();
            let spread = |v: &[f64]| {
                v.iter().copied().fold(f64::NEG_INFINITY, f64::max) - v.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let fps = col(|w| w.fps);
            let p50 = col(|w| w.p50);
            Row {
                variant: i,
                windows: ws.len(),
                fps: median(&fps),
                fps_spread: if ws.is_empty() { 0.0 } else { spread(&fps) },
                p50: median(&p50),
                p50_spread: if ws.is_empty() { 0.0 } else { spread(&p50) },
                p95: median(&col(|w| w.p95)),
                p99: median(&col(|w| w.p99)),
                removed: ws.iter().filter(|w| w.removed).count(),
            }
        })
        .collect()
}

/// The summary table (one line per variant, after a header).
pub(crate) fn summary(v: &[Variant], windows: &[Window]) -> String {
    let mut s = String::from("ab summary: variant windows fps(spread) p50ms(spread) p95 p99 removed | switches\n");
    for r in aggregate(v.len(), windows) {
        let sets: Vec<String> = v[r.variant].sets.iter().map(|(k, x)| format!("{k}={x}")).collect();
        let _ = writeln!(
            s,
            "ab summary: {} {} {:.1}(±{:.1}) {:.2}(±{:.2}) {:.2} {:.2} {} | {}",
            v[r.variant].name,
            r.windows,
            r.fps,
            r.fps_spread / 2.0,
            r.p50,
            r.p50_spread / 2.0,
            r.p95,
            r.p99,
            r.removed,
            if sets.is_empty() { "boot".into() } else { sets.join(",") }
        );
    }
    s
}

#[derive(Debug)]
enum Phase {
    /// Not in the world yet.
    Waiting,
    /// In the world, variant 0 applied, until `until` ns.
    Warmup { until: u64 },
    /// Measuring `variant` from `start`; frames before `measure_from` skipped.
    Window { variant: usize, start: u64, measure_from: u64 },
    Done,
}

/// What the caller does at this frame boundary.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    Nothing,
    /// Settle every flight, then [`apply`] this variant.
    Apply(usize),
    /// A window closed (its line), maybe followed by a summary.
    Closed { line: String, summary: Option<String>, next: Option<usize> },
}

/// The timing configuration, in ns.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) window: u64,
    pub(crate) settle: u64,
    pub(crate) warmup: u64,
    pub(crate) cycles: u64,
}

impl Timing {
    pub(crate) fn from_env() -> Self {
        let num = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(d);
        let ns = |s: f64| (s.max(0.0) * 1e9) as u64;
        let window = ns(num("VICTORY_AB_WINDOW_S", 20.0).max(2.0));
        Timing {
            window,
            settle: ns(num("VICTORY_AB_SETTLE_S", 1.0)).min(window / 2),
            warmup: ns(num("VICTORY_AB_WARMUP_S", 30.0)),
            cycles: num("VICTORY_AB_CYCLES", 0.0) as u64,
        }
    }
}

pub(crate) struct Scheduler {
    pub(crate) variants: Vec<Variant>,
    t: Timing,
    phase: Phase,
    ms: Vec<f64>,
    last_frame: Option<u64>,
    pub(crate) windows: Vec<Window>,
    removed: bool,
}

impl Scheduler {
    pub(crate) fn new(variants: Vec<Variant>, t: Timing) -> Self {
        Scheduler { variants, t, phase: Phase::Waiting, ms: Vec::new(), last_frame: None, windows: Vec::new(), removed: false }
    }

    pub(crate) fn done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// A frame was presented at `now` ns; `in_world` from the detector.
    pub(crate) fn frame(&mut self, now: u64, in_world: bool) -> Step {
        let dt = self.last_frame.replace(now).map(|l| now.saturating_sub(l) as f64 / 1e6);
        match self.phase {
            Phase::Waiting if in_world => {
                self.phase = Phase::Warmup { until: now + self.t.warmup };
                Step::Apply(0)
            }
            Phase::Waiting | Phase::Done => Step::Nothing,
            Phase::Warmup { until } => {
                if now >= until {
                    self.open(0, now);
                }
                Step::Nothing
            }
            Phase::Window { variant, start, measure_from } => {
                if now >= measure_from
                    && let Some(dt) = dt
                {
                    self.ms.push(dt);
                }
                if now < start + self.t.window && !self.removed {
                    return Step::Nothing;
                }
                let measured = now.saturating_sub(measure_from) as f64 / 1e9;
                let w = close(variant, std::mem::take(&mut self.ms), measured, self.removed);
                self.windows.push(w);
                let line = window_line(&self.variants, self.windows.last().expect("pushed"), self.windows.len() - 1);
                let n = self.variants.len();
                let cycle_end = variant + 1 == n;
                let cycles_done = self.windows.len() as u64 / n as u64;
                let stop = self.removed || (cycle_end && self.t.cycles > 0 && cycles_done >= self.t.cycles);
                let summary = (cycle_end || stop).then(|| summary(&self.variants, &self.windows));
                if stop {
                    self.phase = Phase::Done;
                    return Step::Closed { line, summary, next: (variant != 0 && !self.removed).then_some(0) };
                }
                let next = (variant + 1) % n;
                self.open(next, now);
                Step::Closed { line, summary, next: Some(next) }
            }
        }
    }

    fn open(&mut self, variant: usize, now: u64) {
        self.ms.clear();
        self.phase = Phase::Window { variant, start: now, measure_from: now + self.t.settle };
    }

    /// The host device was removed: the window closes at the next frame
    /// (if one comes) marked, and the schedule ends.
    pub(crate) fn device_removed(&mut self) {
        self.removed = true;
    }

    /// The summary so far (at exit).
    pub(crate) fn summary(&self) -> String {
        summary(&self.variants, &self.windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn sched(window_s: u64, warmup_s: u64, cycles: u64) -> Scheduler {
        let v = parse_schedule("A:;B:TEXTURE_WIDE=1;C:TEXTURE_WIDE=1,WIDE_REFRESH").unwrap();
        Scheduler::new(v, Timing { window: window_s * S, settle: S, warmup: warmup_s * S, cycles })
    }

    #[test]
    fn parses_and_refuses() {
        let v = parse_schedule("A:;B:VICTORY_TEXTURE_WIDE=1;C:TEXTURE_WIDE=1,WIDE_REFRESH").unwrap();
        assert_eq!(v.len(), 3);
        assert!(v[0].sets.is_empty());
        assert_eq!(v[1].sets, vec![("TEXTURE_WIDE".into(), 1)]);
        assert_eq!(v[2].sets, vec![("TEXTURE_WIDE".into(), 1), ("WIDE_REFRESH".into(), 1)]);
        let plus = parse_schedule("A:;B:TEXTURE_WIDE=1+WIDE_REFRESH").unwrap();
        assert_eq!(plus[1].sets, v[2].sets, "a plus sign joins switches as a comma does");
        assert!(parse_schedule("A:;B:ASYNC_SUBMIT=1").unwrap_err().contains("boot-only"));
        assert!(parse_schedule("A:;B:NOPE=1").unwrap_err().contains("not a known"));
        assert!(parse_schedule("A:").is_err());
        assert!(parse_schedule("A:;A:WIDE_REFRESH").is_err());
    }

    #[test]
    fn percentiles_and_median() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 95.0), 95.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    }

    /// Runs `secs` seconds at `fps`, returning the steps that were not Nothing.
    fn run(s: &mut Scheduler, from: u64, secs: u64, fps: u64, in_world: bool) -> (u64, Vec<Step>) {
        let mut out = Vec::new();
        let dt = S / fps;
        let mut t = from;
        while t < from + secs * S {
            t += dt;
            let st = s.frame(t, in_world);
            if st != Step::Nothing {
                out.push(st);
            }
        }
        (t, out)
    }

    #[test]
    fn waits_for_world_warms_up_then_cycles() {
        let mut s = sched(10, 5, 2);
        let (t, steps) = run(&mut s, 0, 3, 50, false);
        assert!(steps.is_empty());
        let (t, steps) = run(&mut s, t, 5, 50, true);
        assert_eq!(steps, vec![Step::Apply(0)]);
        // 2 cycles of 3 windows of 10 s.
        let (_, steps) = run(&mut s, t, 70, 50, true);
        let nexts: Vec<Option<usize>> = steps
            .iter()
            .map(|st| match st {
                Step::Closed { next, .. } => *next,
                _ => panic!("{st:?}"),
            })
            .collect();
        // The last goes back to A (boot values) and the schedule ends.
        assert_eq!(nexts, vec![Some(1), Some(2), Some(0), Some(1), Some(2), Some(0)]);
        assert!(s.done());
        assert_eq!(s.windows.len(), 6);
        let order: Vec<usize> = s.windows.iter().map(|w| w.variant).collect();
        assert_eq!(order, vec![0, 1, 2, 0, 1, 2]);
        // 9 measured seconds at 50 fps (the settle second skipped).
        for w in &s.windows {
            assert!((w.frames as i64 - 450).abs() <= 2, "{w:?}");
            assert!((w.fps - 50.0).abs() < 0.5, "{w:?}");
            assert!((w.p50 - 20.0).abs() < 0.01);
        }
        let summaries = steps.iter().filter(|st| matches!(st, Step::Closed { summary: Some(_), .. })).count();
        assert_eq!(summaries, 2);
    }

    #[test]
    fn device_removed_ends_and_marks() {
        let mut s = sched(10, 0, 0);
        let (t, _) = run(&mut s, 0, 1, 50, true);
        let (t, _) = run(&mut s, t, 12, 50, true);
        s.device_removed();
        let st = s.frame(t + S / 50, true);
        match st {
            Step::Closed { line, summary, next } => {
                assert!(line.contains("device-removed=1"), "{line}");
                assert!(summary.is_some());
                assert_eq!(next, None);
            }
            _ => panic!("{st:?}"),
        }
        assert!(s.done());
    }

    #[test]
    fn aggregates_median_of_windows_and_spread() {
        let w = |variant, fps: f64, p50: f64| Window { variant, frames: 100, fps, p50, p95: p50 * 2.0, p99: p50 * 3.0, removed: false };
        let ws = vec![w(0, 30.0, 33.0), w(1, 40.0, 25.0), w(0, 34.0, 29.0), w(1, 42.0, 24.0), w(0, 31.0, 32.0)];
        let rows = aggregate(2, &ws);
        assert_eq!(rows[0].windows, 3);
        assert_eq!(rows[0].fps, 31.0);
        assert_eq!(rows[0].fps_spread, 4.0);
        assert_eq!(rows[0].p50, 32.0);
        assert_eq!(rows[1].fps, 41.0);
        assert_eq!(rows[1].p50_spread, 1.0);
        let v = parse_schedule("A:;B:WIDE_REFRESH=1").unwrap();
        let s = summary(&v, &ws);
        assert_eq!(s.lines().count(), 3);
        assert!(s.contains("ab summary: B 2 41.0(±1.0)"), "{s}");
        assert!(s.contains("| WIDE_REFRESH=1"));
    }

    /// Ours: switching variants resets what the last one set first.
    #[test]
    fn apply_resets_others_first() {
        static TEX: crate::live::Switch = crate::live::Switch::new("TEXTURE_WIDE", 0);
        static WIDE: crate::live::Switch = crate::live::Switch::new("WIDE_REFRESH", 0);
        let _one = crate::live::tests_lock();
        let v = parse_schedule("A:;B:TEXTURE_WIDE=1;C:WIDE_REFRESH").unwrap();
        apply(&v, &v[1]);
        assert_eq!((TEX.get(), WIDE.get()), (1, 0));
        apply(&v, &v[2]);
        assert_eq!((TEX.get(), WIDE.get()), (0, 1));
        apply(&v, &v[0]);
        assert_eq!((TEX.get(), WIDE.get()), (0, 0));
    }
}

//! Performance instrumentation, active only with `--features perf` (the
//! `cargo perf` alias). Without the feature every function here is an empty
//! inline no-op, so debug and release builds carry no measurement code.
//! Measurements print as `perf:`-prefixed lines on stderr.
//!
//! Targets: cold startup under ~200ms to first frame, input latency sub-frame
//! (< 16.7ms), idle memory in tens of MB rather than hundreds, idle CPU ~0%.
//!
//! Reading the numbers depends on the rig. Under WSLg without GPU passthrough
//! the Vulkan driver is llvmpipe — software rendering — so every frame,
//! memory, and CPU figure is a floor: a pass there holds on real hardware, but
//! a miss may be llvmpipe's rather than the code's. Get a native datapoint
//! before optimizing against one.

#[cfg(feature = "perf")]
pub use real::*;

#[cfg(feature = "perf")]
mod real {
    use std::sync::{Once, OnceLock};
    use std::time::Instant;

    use gpui::Window;

    static LAUNCH: OnceLock<Instant> = OnceLock::new();
    static FIRST_FRAME: Once = Once::new();

    /// First statement of `main`: anchors the startup measurement.
    pub fn launch() {
        let _ = LAUNCH.set(Instant::now());
    }

    /// Called on every render; logs once, after the first frame is drawn.
    pub fn first_frame(window: &Window) {
        FIRST_FRAME.call_once(|| {
            let t0 = *LAUNCH.get().expect("perf::launch is main's first statement");
            window.on_next_frame(move |_, _| {
                eprintln!("perf: startup → first frame {:.1?}{}", t0.elapsed(), rss());
            });
        });
    }

    /// Top of the key handler: logs keystroke → the next drawn frame. Fires
    /// after gpui finishes drawing, so it covers app work + layout + paint
    /// but not the compositor's present.
    pub fn key(name: &str, window: &Window) {
        let t0 = Instant::now();
        let name = name.to_owned();
        window.on_next_frame(move |_, _| {
            eprintln!("perf: key {name:?} → frame {:.1?}", t0.elapsed());
        });
    }

    /// Pair around a vault scan: `let t = perf::t0(); …; perf::scan_done(t, n)`.
    pub fn t0() -> Option<Instant> {
        Some(Instant::now())
    }

    pub fn scan_done(t0: Option<Instant>, files: usize) {
        if let Some(t) = t0 {
            eprintln!("perf: vault scan {files} files in {:.1?}", t.elapsed());
        }
    }

    /// `", rss 48MB"`, or empty where /proc is unavailable (non-Linux).
    fn rss() -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                let kb: u64 = s
                    .lines()
                    .find(|l| l.starts_with("VmRSS:"))?
                    .split_whitespace()
                    .nth(1)?
                    .parse()
                    .ok()?;
                Some(format!(", rss {}MB", kb / 1024))
            })
            .unwrap_or_default()
    }
}

#[cfg(not(feature = "perf"))]
pub use noop::*;

#[cfg(not(feature = "perf"))]
mod noop {
    use std::time::Instant;

    use gpui::Window;

    #[inline(always)]
    pub fn launch() {}
    #[inline(always)]
    pub fn first_frame(_: &Window) {}
    #[inline(always)]
    pub fn key(_: &str, _: &Window) {}
    #[inline(always)]
    pub fn t0() -> Option<Instant> {
        None
    }
    #[inline(always)]
    pub fn scan_done(_: Option<Instant>, _: usize) {}
}

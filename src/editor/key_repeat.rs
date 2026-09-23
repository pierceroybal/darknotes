//! Self-driven key auto-repeat (macOS only): the state behind
//! `Editor::on_key`'s echo swallowing and `arm_key_repeat`'s replay loop.
//!
//! macOS's native auto-repeat stays slow even at its fastest setting, so
//! there darknotes replays a held key on its own cadence. That makes the
//! repeat loop a generator the app itself must stop, and the app's `KeyUp`
//! can't be the only off-switch. AppKit never delivers `keyUp:` to the
//! window while Cmd is held, so a key released under Cmd (`?` still down as
//! a finger lands on Cmd for a shortcut) would replay forever. gpui also
//! drops a platform event outright if it arrives while its handler is
//! already on the stack.
//!
//! So every tick first asks the HID system whether any key has come up since
//! the press (`CGEventSourceSecondsSinceLastEventType`). That record sits
//! below AppKit's event routing — no Cmd swallow, focus change, or dropped
//! event can hide a release from it — and needs no Input Monitoring
//! permission. If it can't answer, the tick stops: a missed repeat is
//! harmless, an unintended one is not.
//!
//! Everywhere else the backend's native repeat passes straight through, one
//! pulse per repeat, as in any other app there. A self-driven loop needs
//! proof the key is still down on every tick, and Linux (X11/WSLg, Wayland)
//! offers no such query — only the pulses themselves, and anything timed off
//! those is a guess that can overshoot.

use std::time::{Duration, Instant};

use gpui::{KeyDownEvent, Keystroke};

pub struct KeyRepeat {
    delay: Duration,
    interval: Duration,
    hold: Option<Hold>,
}

struct Hold {
    /// What each tick replays. Echoes overwrite it: the OS re-translates a
    /// held key's modifiers on every pulse, so a released Shift shows up here.
    stroke: Keystroke,
    pressed: Instant,
}

impl KeyRepeat {
    pub fn new(delay_ms: u64, interval_ms: u64) -> Self {
        Self {
            delay: Duration::from_millis(delay_ms),
            interval: Duration::from_millis(interval_ms),
            hold: None,
        }
    }

    /// Self-driven repeat runs only on macOS, and only with a nonzero
    /// interval; otherwise native repeat passes through untouched.
    pub fn enabled(&self) -> bool {
        cfg!(target_os = "macos") && !self.interval.is_zero()
    }

    pub fn delay(&self) -> Duration {
        self.delay
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a raw `KeyDown` is the backend echoing the held key rather
    /// than a new press. An echo is recognized by `is_held`, falling back to
    /// a `key` compare: gpui's macOS backend re-dispatches non-printing keys
    /// (backspace, arrows, escape, …) through the input system with
    /// `is_held` forced to `false`. The compare can't be the primary test
    /// because macOS folds Shift into `key` for symbol keys, so releasing
    /// Shift mid-hold renames the still-held key (`:` → `;`) and a `key`
    /// match would misread its next pulse as a fresh press, inserting a
    /// stray character.
    pub fn absorb_echo(&mut self, ev: &KeyDownEvent) -> bool {
        let Some(hold) = &mut self.hold else { return false };
        if !(ev.is_held || hold.stroke.key == ev.keystroke.key) {
            return false;
        }
        hold.stroke = ev.keystroke.clone();
        true
    }

    /// A fresh press: it becomes the held key, replacing any other.
    pub fn press(&mut self, stroke: Keystroke, now: Instant) {
        self.hold = Some(Hold { stroke, pressed: now });
    }

    /// Any key came up (or the window lost focus): the hold is over. Only
    /// one key repeats at a time, and the Shift fold means a `KeyUp` can
    /// report a different `key` than its `KeyDown`, so which key is moot.
    pub fn release(&mut self) {
        self.hold = None;
    }

    /// One tick of the replay loop: the stroke to replay, or `None` once the
    /// hold is over — the loop's cue to stop. `released_since` is the
    /// platform probe (`hid_released_since` in the app); only a definite
    /// "no key has come up" keeps the hold alive.
    pub fn tick(
        &mut self,
        released_since: impl FnOnce(Instant) -> Option<bool>,
    ) -> Option<Keystroke> {
        let hold = self.hold.as_ref()?;
        if released_since(hold.pressed) != Some(false) {
            self.hold = None;
            return None;
        }
        Some(hold.stroke.clone())
    }
}

/// Has any key come up at the HID level since `since`? Read from the
/// system's input state rather than this app's event stream, so it sees
/// releases AppKit never delivered to us. `None` if it can't tell.
#[cfg(target_os = "macos")]
pub fn hid_released_since(since: Instant) -> Option<bool> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    const HID_SYSTEM_STATE: i32 = 1; // kCGEventSourceStateHIDSystemState
    const KEY_UP: u32 = 11; // kCGEventKeyUp; modifier releases are FlagsChanged
    // SAFETY: a pure query of global input state; no pointers, no preconditions.
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, KEY_UP) };
    let ago = Duration::try_from_secs_f64(secs).ok()?;
    Some(ago < since.elapsed())
}

#[cfg(not(target_os = "macos"))]
pub fn hid_released_since(_since: Instant) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn ks(key: &str, shift: bool) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers: Modifiers { shift, ..Default::default() },
        }
    }

    fn down(key: &str, shift: bool, is_held: bool) -> KeyDownEvent {
        KeyDownEvent { keystroke: ks(key, shift), is_held }
    }

    const HELD: fn(Instant) -> Option<bool> = |_| Some(false);
    const RELEASED: fn(Instant) -> Option<bool> = |_| Some(true);
    const UNKNOWN: fn(Instant) -> Option<bool> = |_| None;

    #[test]
    fn nothing_held_is_not_an_echo() {
        let mut r = KeyRepeat::new(500, 30);
        assert!(!r.absorb_echo(&down("j", false, true)));
    }

    #[test]
    fn echo_by_is_held_survives_the_shift_fold() {
        let mut r = KeyRepeat::new(500, 30);
        r.press(ks("?", true), Instant::now());
        // Shift released mid-hold: macOS renames the still-held key.
        assert!(r.absorb_echo(&down("/", false, true)));
        assert_eq!(r.tick(HELD).map(|s| s.key).as_deref(), Some("/"));
    }

    #[test]
    fn echo_by_same_key_when_not_flagged_held() {
        let mut r = KeyRepeat::new(500, 30);
        r.press(ks("backspace", false), Instant::now());
        assert!(r.absorb_echo(&down("backspace", false, false)));
        assert!(!r.absorb_echo(&down("k", false, false)));
    }

    #[test]
    fn key_up_stops_the_hold() {
        let mut r = KeyRepeat::new(500, 30);
        r.press(ks("?", true), Instant::now());
        r.release();
        assert!(r.tick(HELD).is_none());
    }

    #[test]
    fn probe_release_stops_without_a_key_up() {
        let mut r = KeyRepeat::new(500, 30);
        r.press(ks("?", true), Instant::now());
        assert!(r.tick(HELD).is_some());
        assert!(r.tick(RELEASED).is_none());
        // And it stays over.
        assert!(r.tick(HELD).is_none());
    }

    #[test]
    fn unknown_probe_fails_safe() {
        let mut r = KeyRepeat::new(500, 30);
        r.press(ks("j", false), Instant::now());
        assert!(r.tick(UNKNOWN).is_none());
    }

    #[test]
    fn enabled_only_on_macos_with_an_interval() {
        assert!(!KeyRepeat::new(500, 0).enabled());
        assert_eq!(KeyRepeat::new(500, 30).enabled(), cfg!(target_os = "macos"));
    }
}

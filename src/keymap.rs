//! The keymap layer: key sequences → registry command names, resolved as an
//! override before the built-in vim grammar. Bindings map keys to *commands*
//! (never to other keys), so there is no recursive-remap machinery. Keys
//! buffer while they form a proper prefix of some binding; the editor arms a
//! `timeoutlen` timer on `pending()` and replays the buffer as ordinary input
//! when the match breaks or the timer fires.

use gpui::{Keystroke, Modifiers};

use crate::config;

/// Where a binding is live. `Global` applies in every editor-pane mode (the
/// Ctrl-chord layer); `Normal`/`Insert` gate on the vim mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    Global,
    Normal,
    Insert,
}

struct Binding {
    ctx: Ctx,
    seq: Vec<Keystroke>,
    command: String,
}

/// Outcome of feeding one keystroke: `replay` goes to the built-in path (in
/// order), then `command` (if any) runs. Both empty = the key was buffered.
pub struct Resolution {
    pub replay: Vec<Keystroke>,
    pub command: Option<String>,
}

pub struct Resolver {
    /// User bindings first, then defaults — the first exact match wins, so a
    /// user binding on the same sequence shadows the built-in one.
    bindings: Vec<Binding>,
    /// In-progress prefix of some binding. The editor arms the timeout while
    /// non-empty and `flush`es on fire.
    buf: Vec<Keystroke>,
}

impl Resolver {
    /// Build from the `[keymap.*]` config tables plus the built-in defaults.
    /// An unparseable key or unknown command name warns and drops that binding
    /// — same policy as a config parse error: never brick the editor.
    pub fn new(cfg: &config::Keymap, defaults: &[(Ctx, &str, &str)], commands: &[&str]) -> Self {
        let mut bindings = Vec::new();
        let user =
            [(Ctx::Global, &cfg.global), (Ctx::Normal, &cfg.normal), (Ctx::Insert, &cfg.insert)];
        for (ctx, map) in user {
            for (seq, command) in map {
                if !commands.contains(&command.as_str()) {
                    eprintln!("darknotes: keymap: unknown command {command:?} for {seq:?}");
                    continue;
                }
                match parse_seq(seq) {
                    Ok(keys) if keys.is_empty() => {
                        eprintln!("darknotes: keymap: empty key sequence for {command:?}")
                    }
                    Ok(keys) => bindings.push(Binding { ctx, seq: keys, command: command.clone() }),
                    Err(bad) => eprintln!("darknotes: keymap: bad key {bad:?} in {seq:?}"),
                }
            }
        }
        for (ctx, seq, command) in defaults {
            let keys = parse_seq(seq).expect("default binding must parse");
            bindings.push(Binding { ctx: *ctx, seq: keys, command: (*command).to_string() });
        }
        Self { bindings, buf: Vec::new() }
    }

    /// Feed one keystroke against the bindings live in `active` contexts.
    /// Buffered keys no binding wants anymore are surrendered head-first for
    /// replay and the tail retried, so the second `j` of a broken `jj` can
    /// start a fresh `j k` match (vim's mapping retry).
    ///
    /// An exact match fires immediately: a longer binding sharing the prefix
    /// ("space" vs "space f") is shadowed, not awaited — no ambiguity wait.
    pub fn feed(&mut self, active: &[Ctx], ks: &Keystroke) -> Resolution {
        self.buf.push(ks.clone());
        let mut replay = Vec::new();
        loop {
            let mut is_prefix = false;
            for b in self.bindings.iter().filter(|b| active.contains(&b.ctx)) {
                if b.seq.len() < self.buf.len() || !seq_matches(&b.seq, &self.buf) {
                    continue;
                }
                if b.seq.len() == self.buf.len() {
                    self.buf.clear();
                    return Resolution { replay, command: Some(b.command.clone()) };
                }
                is_prefix = true;
            }
            if is_prefix {
                return Resolution { replay, command: None }; // buffered; caller arms the timer
            }
            replay.push(self.buf.remove(0));
            if self.buf.is_empty() {
                return Resolution { replay, command: None };
            }
        }
    }

    /// `true` while a partial sequence is buffered — the editor's cue to arm
    /// the `timeoutlen` timer.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Timeout fired: surrender the buffer for replay as ordinary input.
    pub fn flush(&mut self) -> Vec<Keystroke> {
        std::mem::take(&mut self.buf)
    }

    /// Drop any pending sequence (buffer switch).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// A binding sequence: whitespace-separated gpui keystrokes (`"ctrl-s"`,
/// `"space f"`, `"j k"`). Returns the token that failed to parse.
fn parse_seq(s: &str) -> Result<Vec<Keystroke>, String> {
    s.split_whitespace()
        .map(|tok| Keystroke::parse(tok).map_err(|_| tok.to_string()))
        .collect()
}

fn seq_matches(seq: &[Keystroke], buf: &[Keystroke]) -> bool {
    buf.iter().zip(seq).all(|(a, b)| norm(a) == norm(b))
}

/// Keystroke identity for matching: (key, modifiers), ignoring `key_char`.
/// A shifted letter can arrive as `"P"` or as `"p"` + shift depending on
/// platform; fold both to lowercase + shift.
fn norm(ks: &Keystroke) -> (String, Modifiers) {
    let mut key = ks.key.clone();
    let mut m = ks.modifiers;
    if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
        key.make_ascii_lowercase();
        m.shift = true;
    }
    (key, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const NAMES: &[&str] = &["save", "open-file", "normal-mode"];

    fn k(key: &str) -> Keystroke {
        Keystroke { key: key.into(), key_char: Some(key.into()), modifiers: Modifiers::default() }
    }

    fn ctrl(key: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: None,
            modifiers: Modifiers { control: true, ..Default::default() },
        }
    }

    fn resolver(defaults: &[(Ctx, &str, &str)]) -> Resolver {
        Resolver::new(&config::Keymap::default(), defaults, NAMES)
    }

    #[test]
    fn single_chord_matches() {
        let mut r = resolver(&[(Ctx::Global, "ctrl-s", "save")]);
        let res = r.feed(&[Ctx::Global, Ctx::Normal], &ctrl("s"));
        assert!(res.replay.is_empty());
        assert_eq!(res.command.as_deref(), Some("save"));
        assert!(!r.pending());
    }

    #[test]
    fn sequence_buffers_then_matches() {
        let mut r = resolver(&[(Ctx::Insert, "j k", "normal-mode")]);
        let ctxs = &[Ctx::Global, Ctx::Insert];
        let res = r.feed(ctxs, &k("j"));
        assert!(res.replay.is_empty() && res.command.is_none());
        assert!(r.pending()); // lead key buffered, nothing typed
        let res = r.feed(ctxs, &k("k"));
        assert!(res.replay.is_empty());
        assert_eq!(res.command.as_deref(), Some("normal-mode"));
    }

    #[test]
    fn broken_sequence_replays_then_retries_tail() {
        // `jj` under a `j k` binding: the first `j` replays as text, the
        // second re-buffers as a fresh lead key.
        let mut r = resolver(&[(Ctx::Insert, "j k", "normal-mode")]);
        let ctxs = &[Ctx::Global, Ctx::Insert];
        r.feed(ctxs, &k("j"));
        let res = r.feed(ctxs, &k("j"));
        assert_eq!(res.replay.len(), 1);
        assert!(res.command.is_none());
        assert!(r.pending());
    }

    #[test]
    fn mismatch_replays_everything() {
        let mut r = resolver(&[(Ctx::Insert, "j k", "normal-mode")]);
        let ctxs = &[Ctx::Global, Ctx::Insert];
        r.feed(ctxs, &k("j"));
        let res = r.feed(ctxs, &k("u"));
        assert_eq!(res.replay.iter().map(|ks| ks.key.as_str()).collect::<Vec<_>>(), ["j", "u"]);
        assert!(res.command.is_none());
        assert!(!r.pending());
    }

    #[test]
    fn flush_surrenders_buffer() {
        let mut r = resolver(&[(Ctx::Insert, "j k", "normal-mode")]);
        r.feed(&[Ctx::Insert], &k("j"));
        let flushed = r.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].key, "j");
        assert!(!r.pending());
        assert!(r.flush().is_empty()); // idempotent
    }

    #[test]
    fn context_gates_bindings() {
        let mut r = resolver(&[(Ctx::Insert, "j k", "normal-mode")]);
        // Normal mode: the insert binding is dormant, `j` passes straight through.
        let res = r.feed(&[Ctx::Global, Ctx::Normal], &k("j"));
        assert_eq!(res.replay.len(), 1);
        assert!(!r.pending());
    }

    #[test]
    fn uppercase_key_matches_shift_binding() {
        // Some platforms report Ctrl-Shift-P as key "P" with shift unset.
        let mut r = resolver(&[(Ctx::Global, "ctrl-shift-p", "open-file")]);
        let ks = Keystroke {
            key: "P".into(),
            key_char: None,
            modifiers: Modifiers { control: true, ..Default::default() },
        };
        let res = r.feed(&[Ctx::Global], &ks);
        assert_eq!(res.command.as_deref(), Some("open-file"));
    }

    #[test]
    fn unknown_command_is_dropped() {
        let cfg = config::Keymap {
            insert: BTreeMap::from([("j k".to_string(), "bogus".to_string())]),
            ..Default::default()
        };
        let mut r = Resolver::new(&cfg, &[], NAMES);
        let res = r.feed(&[Ctx::Insert], &k("j"));
        assert_eq!(res.replay.len(), 1); // binding dropped → key passes through
        assert!(!r.pending());
    }

    #[test]
    fn user_binding_shadows_default() {
        let cfg = config::Keymap {
            global: BTreeMap::from([("ctrl-s".to_string(), "open-file".to_string())]),
            ..Default::default()
        };
        let mut r = Resolver::new(&cfg, &[(Ctx::Global, "ctrl-s", "save")], NAMES);
        let res = r.feed(&[Ctx::Global], &ctrl("s"));
        assert_eq!(res.command.as_deref(), Some("open-file"));
    }
}

//! Buffer search: the `/`/`?` prompt state, match scanning (shared with
//! rendering for hlsearch/incsearch), and `n`/`N` navigation.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use crate::config::Search as SearchConfig;
use crate::vim::Mode;

use super::Editor;

/// The last search and the live prompt state. Lives on the editor, not the
/// document — like vim's search register it spans buffer switches.
#[derive(Default)]
pub(super) struct SearchState {
    /// Last submitted query (the `n`/`N` target); empty = no search yet.
    query: String,
    /// Direction of the last search; `n` follows it, `N` reverses it.
    backward: bool,
    /// hlsearch is lit; `:noh` clears it until the next search or `n`/`N`.
    pub(super) hl: bool,
    /// Caret when the `/`/`?` prompt opened — `Some` only while it's open.
    /// Incremental jumps preview from here; cancelling restores to it.
    pub(super) origin: Option<usize>,
}

impl Editor {
    /// The query whose matches should be highlighted right now: the pending
    /// prompt text while a `/`/`?` search is being typed (incsearch preview),
    /// else the last submitted query while hlsearch is lit. Empty = none.
    pub(super) fn search_query(&self) -> String {
        let prompt_open = self.vim.mode == Mode::Command && self.vim.prompt() != ':';
        if prompt_open && self.search_cfg.incsearch {
            self.vim.command_line().to_string()
        } else if !prompt_open && self.search_cfg.hlsearch && self.search.hl {
            self.search.query.clone()
        } else {
            String::new()
        }
    }

    /// A submitted `/`/`?` query. An empty query repeats the last search in the
    /// new direction. The jump starts from where the prompt opened — incsearch
    /// may have dragged the caret elsewhere while typing.
    pub(super) fn do_search(&mut self, query: String, backward: bool) {
        let origin = self.search.origin.take().unwrap_or_else(|| self.doc().caret_offset());
        if !query.is_empty() {
            self.search.query = query;
        }
        if self.search.query.is_empty() {
            self.message = Some("E35: No previous regular expression".into());
            return;
        }
        self.search.backward = backward;
        self.search.hl = true;
        self.doc_mut().jump_to(origin);
        self.find_and_jump(backward, 1);
    }

    /// `n`/`N`: repeat the last search; `reverse` flips its stored direction.
    pub(super) fn search_next(&mut self, reverse: bool, count: usize) {
        if self.search.query.is_empty() {
            self.message = Some("E35: No previous regular expression".into());
            return;
        }
        self.search.hl = true; // `n` after `:noh` re-lights the matches
        self.find_and_jump(self.search.backward != reverse, count);
    }

    /// Jump `count` matches from the caret, honoring wrapscan, with vim's wrap
    /// and not-found messages. The caret stays put when nothing is found.
    fn find_and_jump(&mut self, backward: bool, count: usize) {
        let q = &self.search.query;
        let matches = find_matches(&self.doc().rope, q, search_sensitive(q, &self.search_cfg));
        let mut at = self.doc().caret_offset();
        let mut wrapped = false;
        for _ in 0..count.max(1) {
            match next_match(&matches, at, backward, self.search_cfg.wrapscan) {
                Some((i, w)) => {
                    at = matches[i].0;
                    wrapped |= w;
                }
                None => {
                    self.message = Some(if matches.is_empty() {
                        format!("E486: Pattern not found: {q}")
                    } else if backward {
                        format!("E384: search hit TOP without match for: {q}")
                    } else {
                        format!("E385: search hit BOTTOM without match for: {q}")
                    });
                    return;
                }
            }
        }
        self.doc_mut().jump_to(at);
        if wrapped {
            self.message = Some(if backward {
                "search hit TOP, continuing at BOTTOM".into()
            } else {
                "search hit BOTTOM, continuing at TOP".into()
            });
        }
    }

    /// Track the search-prompt lifecycle around each key: capture the caret
    /// when `/`/`?` opens, live-preview the nearest match while typing
    /// (incsearch), restore the caret on cancel. A submitted search lands via
    /// `apply`, which consumes `origin` before this runs.
    pub(super) fn sync_search_prompt(&mut self, mode_before: Mode) {
        let in_prompt = self.vim.mode == Mode::Command && self.vim.prompt() != ':';
        if in_prompt {
            if mode_before != Mode::Command {
                self.search.origin = Some(self.doc().caret_offset());
            }
            if self.search_cfg.incsearch {
                let origin = self.search.origin.unwrap_or(0);
                let q = self.vim.command_line();
                let jump = (!q.is_empty())
                    .then(|| {
                        let matches =
                            find_matches(&self.doc().rope, q, search_sensitive(q, &self.search_cfg));
                        next_match(&matches, origin, self.vim.prompt() == '?', self.search_cfg.wrapscan)
                            .map(|(i, _)| matches[i].0)
                    })
                    .flatten();
                self.doc_mut().jump_to(jump.unwrap_or(origin)); // no match → sit at origin
            }
        } else if let Some(origin) = self.search.origin.take() {
            self.doc_mut().jump_to(origin); // Esc / backspace-past-prompt cancelled
        }
    }
}

/// Count-preserving case fold for search: a char's first lowercase char. Full
/// `to_lowercase()` can change char counts (ß→ss), which would corrupt the
/// char-offset math that matches share with `Document`.
fn fold(c: char, sensitive: bool) -> char {
    if sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Whether `query` matches case-sensitively under the config: sensitive unless
/// `ignorecase`, except `smartcase` re-sensitizes an uppercase-bearing query.
pub(super) fn search_sensitive(query: &str, cfg: &SearchConfig) -> bool {
    !cfg.ignorecase || (cfg.smartcase && query.chars().any(char::is_uppercase))
}

/// Every match of `query` as absolute char ranges `[start, end)`, scanned per
/// line (a literal one-line query can't span newlines). Overlapping matches
/// step by one char, like vim.
// ponytail: naive O(chars × query) window scan; notes-sized docs make it cheap.
// A folded-copy + memmem search if a profile ever says otherwise.
pub(super) fn find_matches(rope: &ropey::Rope, query: &str, sensitive: bool) -> Vec<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(|c| fold(c, sensitive)).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..rope.len_lines() {
        let start = rope.line_to_char(i);
        // The trailing '\n' rides along harmlessly — the needle never has one.
        let hay: Vec<char> = rope.line(i).chars().map(|c| fold(c, sensitive)).collect();
        for j in 0..hay.len().saturating_sub(needle.len() - 1) {
            if hay[j..j + needle.len()] == needle[..] {
                out.push((start + j, start + j + needle.len()));
            }
        }
    }
    out
}

/// Index into `matches` of the nearest match starting strictly after `from`
/// (strictly before, when `backward`), wrapping around if `wrap`; the second
/// value reports that it wrapped. Strictness is what makes `n` on a match
/// start jump to the *next* one.
fn next_match(
    matches: &[(usize, usize)],
    from: usize,
    backward: bool,
    wrap: bool,
) -> Option<(usize, bool)> {
    if backward {
        match matches.iter().rposition(|&(s, _)| s < from) {
            Some(i) => Some((i, false)),
            None => (wrap && !matches.is_empty()).then(|| (matches.len() - 1, true)),
        }
    } else {
        match matches.iter().position(|&(s, _)| s > from) {
            Some(i) => Some((i, false)),
            None => (wrap && !matches.is_empty()).then_some((0, true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{find_matches, next_match, search_sensitive};
    use crate::config::Search as SearchConfig;
    use ropey::Rope;

    #[test]
    fn find_matches_folds_case_and_counts_chars() {
        // Offsets are char-based: 'é' is one char, two bytes.
        let rope = Rope::from_str("Foo fOO\néfoo\n");
        assert_eq!(find_matches(&rope, "foo", false), vec![(0, 3), (4, 7), (9, 12)]);
        // Sensitive keeps only the exact-case match.
        assert_eq!(find_matches(&rope, "foo", true), vec![(9, 12)]);
        // Overlapping matches step by one; the empty query matches nothing.
        let rope = Rope::from_str("aaaa");
        assert_eq!(find_matches(&rope, "aa", true), vec![(0, 2), (1, 3), (2, 4)]);
        assert!(find_matches(&rope, "", true).is_empty());
    }

    #[test]
    fn search_sensitive_matrix() {
        let cfg = |ignorecase, smartcase| SearchConfig {
            ignorecase,
            smartcase,
            ..Default::default()
        };
        assert!(search_sensitive("foo", &cfg(false, false))); // ignorecase off
        assert!(!search_sensitive("foo", &cfg(true, false)));
        assert!(!search_sensitive("foo", &cfg(true, true))); // all-lower stays loose
        assert!(search_sensitive("Foo", &cfg(true, true))); // uppercase re-sensitizes
        assert!(!search_sensitive("Foo", &cfg(true, false))); // …only with smartcase
    }

    #[test]
    fn next_match_is_strict_and_wraps() {
        let m = [(0, 2), (5, 7), (10, 12)];
        // Strictly after: sitting on a match start jumps to the next one.
        assert_eq!(next_match(&m, 0, false, true), Some((1, false)));
        assert_eq!(next_match(&m, 6, false, true), Some((2, false)));
        // Past the last: wrap around or fail.
        assert_eq!(next_match(&m, 10, false, true), Some((0, true)));
        assert_eq!(next_match(&m, 10, false, false), None);
        // Backward mirrors it.
        assert_eq!(next_match(&m, 10, true, true), Some((1, false)));
        assert_eq!(next_match(&m, 0, true, true), Some((2, true)));
        assert_eq!(next_match(&m, 0, true, false), None);
        assert_eq!(next_match(&[], 0, false, true), None);
    }
}

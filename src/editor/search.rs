//! Buffer search: the `/`/`?` prompt state, match scanning (shared with
//! rendering for hlsearch/incsearch), and `n`/`N` navigation.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::rc::Rc;

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

/// The last match scan, memoized. `find_matches` walks the whole buffer, and
/// within a single keystroke up to two call sites want the same answer — the
/// incsearch jump in `sync_search_prompt`, then `row_ctx` when render builds
/// rows — so without this the document was scanned twice per typed character.
pub(super) struct MatchCache {
    revision: u64,
    query: String,
    sensitive: bool,
    ranges: Rc<Vec<(usize, usize)>>,
}

impl Editor {
    /// Matches of `query` in the active buffer, scanning only when it must.
    ///
    /// Four outcomes, cheapest first:
    ///
    /// 1. **Exact hit** on `(revision, query, sensitivity)` — the common case
    ///    within one keystroke, where the incsearch jump and then render both ask.
    /// 2. **Query extended** — typing another character. The old matches are a
    ///    superset, so `narrow_matches` filters them.
    /// 3. **One line edited** — typing with a search lit. `shift_matches`
    ///    rescans that line and shifts the rest by the edit's char delta.
    /// 4. Anything else rescans the buffer.
    ///
    /// Case 3 is what keeps an edit cheap while hlsearch is on: the whole-buffer
    /// scan is ~5 ms on a 10k-line note, against a ~120 µs row patch, so without
    /// it the scan dominated every keystroke.
    pub(super) fn search_matches(&mut self, query: &str) -> Rc<Vec<(usize, usize)>> {
        let revision = self.doc().revision();
        let sensitive = search_sensitive(query, &self.search_cfg);
        if let Some(c) = &self.match_cache {
            if c.revision == revision && c.sensitive == sensitive && c.query == query {
                return c.ranges.clone();
            }
        }
        // Taken so the rope can be borrowed alongside it; replaced below.
        let cached = self.match_cache.take();
        let rope = self.doc().rope.clone(); // ropey clone shares its backing
        let ranges = 'compute: {
            if let Some(c) = &cached {
                // A sensitivity flip (smartcase seeing an uppercase letter)
                // invalidates the earlier fold, so both reuse paths need it equal.
                if c.sensitive == sensitive {
                    if c.revision == revision
                        && !c.query.is_empty()
                        && query.starts_with(&c.query)
                    {
                        break 'compute narrow_matches(&rope, &c.ranges, query, sensitive);
                    }
                    if c.query == query {
                        if let Some(edit) = self.doc().single_line_edit(c.revision) {
                            break 'compute shift_matches(
                                &rope, &c.ranges, edit, query, sensitive,
                            );
                        }
                    }
                }
            }
            find_matches(&rope, query, sensitive)
        };
        let ranges = Rc::new(ranges);
        self.match_cache = Some(MatchCache {
            revision,
            query: query.to_string(),
            sensitive,
            ranges: ranges.clone(),
        });
        ranges
    }

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

    /// The query the `[n/m]` match counter should track. Same lit/typing
    /// rules as `search_query`, but independent of the `hlsearch` option:
    /// that setting only gates painted highlight color, not `n`/`N`
    /// navigation or the count, so `:nohlsearch` (the option) must not hide
    /// the counter the way it hides color.
    pub(super) fn search_count_query(&self) -> String {
        let prompt_open = self.vim.mode == Mode::Command && self.vim.prompt() != ':';
        if prompt_open && self.search_cfg.incsearch {
            self.vim.command_line().to_string()
        } else if !prompt_open && self.search.hl {
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

    /// Adopt `query` as the search register and light hlsearch, without moving
    /// the caret — what a content-search pick does so `n`/`N` continue from
    /// where it landed.
    pub(super) fn seed_search(&mut self, query: String) {
        self.search.query = query;
        self.search.backward = false;
        self.search.hl = true;
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
        let q = self.search.query.clone();
        let matches = self.search_matches(&q);
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
                let q = self.vim.command_line().to_string();
                let backward = self.vim.prompt() == '?';
                let wrapscan = self.search_cfg.wrapscan;
                let jump = if q.is_empty() {
                    None
                } else {
                    let matches = self.search_matches(&q);
                    next_match(&matches, origin, backward, wrapscan).map(|(i, _)| matches[i].0)
                };
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

/// Whether `needle` (already case-folded) sits at char offset `at`.
///
/// A haystack newline can never equal a needle char — the query comes from a
/// one-line prompt — so this preserves `find_matches`'s rule that a match stays
/// within its line, without needing to know where the lines are.
fn matches_at(rope: &ropey::Rope, at: usize, needle: &[char], sensitive: bool) -> bool {
    let mut chars = rope.chars_at(at);
    needle.iter().all(|&n| chars.next().is_some_and(|c| fold(c, sensitive) == n))
}

/// Matches of a query that *extends* one already scanned, filtered from the
/// previous result instead of rescanning the buffer.
///
/// Every occurrence of `q + more` is an occurrence of `q`, so the previous
/// matches are a superset and only they need re-checking — which is exactly the
/// shape of typing a query: one whole-buffer scan on the first character, then a
/// narrowing pass per keystroke over a shrinking candidate set.
///
/// The caller must have verified that `query` starts with the query `prev` came
/// from *and* that the case-sensitivity is unchanged — smartcase flips it when an
/// uppercase letter arrives, which invalidates the earlier fold.
pub(super) fn narrow_matches(
    rope: &ropey::Rope,
    prev: &[(usize, usize)],
    query: &str,
    sensitive: bool,
) -> Vec<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(|c| fold(c, sensitive)).collect();
    prev.iter()
        .filter(|&&(start, _)| matches_at(rope, start, &needle, sensitive))
        .map(|&(start, _)| (start, start + needle.len()))
        .collect()
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
        push_line_matches(rope, i, &needle, sensitive, &mut out);
    }
    out
}

/// Append line `i`'s matches of the pre-folded `needle`, as absolute char
/// ranges. The single definition of "matches within a line", so the full scan
/// and the incremental update in `shift_matches` cannot drift apart.
fn push_line_matches(
    rope: &ropey::Rope,
    i: usize,
    needle: &[char],
    sensitive: bool,
    out: &mut Vec<(usize, usize)>,
) {
    if needle.is_empty() {
        return;
    }
    let start = rope.line_to_char(i);
    // The trailing '\n' rides along harmlessly — the needle never has one.
    let hay: Vec<char> = rope.line(i).chars().map(|c| fold(c, sensitive)).collect();
    for j in 0..hay.len().saturating_sub(needle.len() - 1) {
        if hay[j..j + needle.len()] == needle[..] {
            out.push((start + j, start + j + needle.len()));
        }
    }
}

/// Matches after a one-line edit, updated from the previous result rather than
/// rescanned.
///
/// A match never spans a newline, so the previous matches partition cleanly at
/// the edited line's boundaries: those before it are untouched, those on it are
/// stale and get rescanned, and those after it are the same matches at offsets
/// shifted by the edit's `delta`. The edited line's *start* offset is identical
/// in both revisions, since every line above it is unchanged.
///
/// `prev` must be the matches of this same query at `edit.before`; the caller
/// establishes that with `Document::single_line_edit`.
pub(super) fn shift_matches(
    rope: &ropey::Rope,
    prev: &[(usize, usize)],
    edit: crate::document::LineEdit,
    query: &str,
    sensitive: bool,
) -> Vec<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(|c| fold(c, sensitive)).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let start = rope.line_to_char(edit.line);
    let end = start + rope.line(edit.line).len_chars();
    // The same line's extent *before* the edit — the coordinates `prev` is in.
    let was_end = (end as isize - edit.delta).max(start as isize) as usize;
    let shift = |o: usize| (o as isize + edit.delta).max(0) as usize;

    let mut out: Vec<(usize, usize)> = Vec::with_capacity(prev.len() + 1);
    // `prev` ascends, so the three groups are contiguous slices of it.
    out.extend(prev.iter().copied().take_while(|&(s, _)| s < start));
    push_line_matches(rope, edit.line, &needle, sensitive, &mut out);
    out.extend(
        prev.iter()
            .skip_while(|&&(s, _)| s < was_end)
            .map(|&(s, e)| (shift(s), shift(e))),
    );
    out
}

/// 1-based index of the match at-or-before `caret` — the `[3/17]` numerator.
/// After `/` or `n`/`N` the caret sits on a match start, so this is exact;
/// between matches it reads as "past match N", and 0 means before the first.
pub(super) fn match_position(matches: &[(usize, usize)], caret: usize) -> usize {
    matches.partition_point(|&(s, _)| s <= caret)
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
    use super::{find_matches, match_position, narrow_matches, next_match, search_sensitive};
    use crate::config::Search as SearchConfig;
    use ropey::Rope;

    #[test]
    fn shifting_matches_after_an_edit_equals_a_fresh_scan() {
        use super::shift_matches;
        use crate::document::LineEdit;

        let query = "widget";
        // Bases with and without a trailing newline — the last line's extent
        // differs between them, and that feeds the shift arithmetic.
        for base in ["widget one\nplain\nwidget two widget\n\nlast widget\n", "a widget\nb"] {
            let original: Vec<String> = base.split('\n').map(str::to_string).collect();
            for (line, new) in original.iter().enumerate().flat_map(|(i, old)| {
                [
                    // Same length, match kept / lost / gained.
                    (i, old.to_uppercase()),
                    // Grow and shrink, including to nothing.
                    (i, format!("{old} widget tail")),
                    (i, format!("{old}{old}")),
                    (i, old.chars().take(2).collect::<String>()),
                    (i, String::new()),
                    // Multi-byte: the char delta differs from the byte delta,
                    // and every offset here is char-based.
                    (i, format!("café {old}")),
                    (i, "café".to_string()),
                ]
            }) {
                let mut edited = original.clone();
                let delta = new.chars().count() as isize
                    - edited[line].chars().count() as isize;
                edited[line] = new.clone();

                let before = Rope::from_str(base);
                let after = Rope::from_str(&edited.join("\n"));
                let edit = LineEdit { before: 1, after: 2, line, delta };

                for sensitive in [false, true] {
                    let prev = find_matches(&before, query, sensitive);
                    assert_eq!(
                        shift_matches(&after, &prev, edit, query, sensitive),
                        find_matches(&after, query, sensitive),
                        "line {line} → {new:?} (delta {delta}), sensitive={sensitive}"
                    );
                }
            }
        }
    }

    #[test]
    fn narrowing_a_query_equals_a_fresh_scan() {
        // Mixed case, overlapping matches, a match split across a newline, and
        // multi-byte text ahead of a hit so char offsets have to be right.
        let rope = Rope::from_str(
            "widget and Widget\nwid WIDGET wide\n\nno match\nwidgetwidget\nwi\nwid\nget\ncafé widget\n",
        );
        for sensitive in [false, true] {
            // Narrow one character at a time, comparing against a full scan at
            // every prefix — the subset property has to hold at each step, not
            // just at the end.
            let full = "widget";
            let mut prev = find_matches(&rope, &full[..1], sensitive);
            for n in 2..=full.len() {
                let q = &full[..n];
                let narrowed = narrow_matches(&rope, &prev, q, sensitive);
                assert_eq!(
                    narrowed,
                    find_matches(&rope, q, sensitive),
                    "query {q:?}, sensitive={sensitive}"
                );
                prev = narrowed;
            }
        }

        // Overlapping matches narrow correctly, including the tail candidate
        // that runs off the end of the buffer.
        let rope = Rope::from_str("aaaa");
        let one = find_matches(&rope, "a", true);
        assert_eq!(one, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
        assert_eq!(narrow_matches(&rope, &one, "aa", true), find_matches(&rope, "aa", true));
        let two = narrow_matches(&rope, &one, "aa", true);
        assert_eq!(narrow_matches(&rope, &two, "aaa", true), find_matches(&rope, "aaa", true));

        // A candidate whose extension would cross a newline is rejected, so
        // narrowing keeps `find_matches`'s stays-within-a-line rule.
        let rope = Rope::from_str("wid\nget\n");
        let prev = find_matches(&rope, "wid", true);
        assert_eq!(prev, vec![(0, 3)]);
        assert!(narrow_matches(&rope, &prev, "widget", true).is_empty());
        assert!(find_matches(&rope, "widget", true).is_empty());
    }

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

    #[test]
    fn match_position_is_one_based_at_or_before() {
        let m = [(5, 7), (10, 12)];
        assert_eq!(match_position(&m, 0), 0); // before the first match
        assert_eq!(match_position(&m, 5), 1); // exactly on a match start
        assert_eq!(match_position(&m, 8), 1); // between matches
        assert_eq!(match_position(&m, 20), 2); // past the last
        assert_eq!(match_position(&[], 3), 0);
    }
}

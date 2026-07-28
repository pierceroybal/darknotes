//! Vault-wide content search: one disk pass snapshots every note line, then
//! each keystroke filters that snapshot in memory. Literal substring, sharing
//! `/`'s case rules — no regex, no fuzzy (a subsequence match is noise on
//! prose, where it's signal on short paths).
//!
//! Disk is the truth, like `tasks::scan`: a line typed into a dirty buffer is
//! searchable once the file is saved.
//!
//! Snapshotting is on-demand (picker open), never per frame or per keystroke.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::vault::Vault;

/// One searchable vault line. `lower` exists so the per-keystroke sweep
/// allocates nothing; both die with the snapshot.
pub struct Line {
    pub path: PathBuf,
    /// 0-based line index within `path`.
    pub line: usize,
    /// The line, whitespace-trimmed — rows read better that way. A query with
    /// leading spaces therefore can't match indentation; mid-line spaces are
    /// unaffected.
    pub text: String,
    /// Chars trimmed off the front, so a hit's offset in `text` maps back to a
    /// caret column in the source line.
    pub indent: usize,
    /// Lowercased `text`, for the filter sweep only. Offsets in it are *not*
    /// valid in `text` — case folding can change a char's byte length — so
    /// `occurrences` re-scans the original.
    lower: String,
}

/// Every non-blank `.md` line in the vault, in vault file order.
// `.md` only, like `tasks::scan`: `vault.files` also holds `.txt`/`.sql`/PDFs,
// and a binary that happened to be valid UTF-8 would inject garbage rows.
pub fn snapshot(vault: &Vault) -> Vec<Line> {
    let t0 = crate::perf::t0();
    let mut out = Vec::new();
    let mut read = 0;
    for path in vault.files.iter().filter(|p| p.extension().is_some_and(|e| e == "md")) {
        // A file that vanished since the vault scan contributes nothing.
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        read += 1;
        out.extend(text.lines().enumerate().filter_map(|(line, raw)| line_of(path, line, raw)));
    }
    crate::perf::grep_snapshot_done(t0, read, out.len());
    out
}

/// One snapshot line, or `None` for a blank one.
fn line_of(path: &Path, line: usize, raw: &str) -> Option<Line> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Line {
        path: path.to_path_buf(),
        line,
        text: trimmed.to_string(),
        indent: raw[..raw.len() - raw.trim_start().len()].chars().count(),
        lower: trimmed.to_lowercase(),
    })
}

/// One `Line` without touching disk, for tests in other modules.
#[cfg(test)]
pub fn line_for_test(path: PathBuf, line: usize, raw: &str) -> Line {
    line_of(&path, line, raw).expect("a non-blank line")
}

/// Indices of the lines containing `query`, in vault order. Every hit, uncapped
/// — indices are cheap; the caller caps how many it renders.
pub fn find(lines: &[Line], query: &str, sensitive: bool) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = if sensitive { query.to_string() } else { query.to_lowercase() };
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| if sensitive { &l.text } else { &l.lower }.contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// Every occurrence of `query` in `text`: the byte range, for painting a picker
/// row, and the char offset, for placing a caret. Occurrences don't overlap —
/// overlapping ranges would double-paint.
///
/// Only called for rows about to be displayed, so a naive char scan is cheap.
/// It re-scans the original text (rather than reusing `Line::lower`) because
/// byte offsets must land in `text`.
pub fn occurrences(text: &str, query: &str, sensitive: bool) -> Vec<(Range<usize>, usize)> {
    let hay: Vec<(usize, char)> =
        text.char_indices().map(|(b, c)| (b, fold(c, sensitive))).collect();
    let needle: Vec<char> = query.chars().map(|c| fold(c, sensitive)).collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()].iter().map(|&(_, c)| c).eq(needle.iter().copied()) {
            let end = hay.get(i + needle.len()).map_or(text.len(), |&(b, _)| b);
            out.push((hay[i].0..end, i));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Count-preserving case fold: a char's first lowercase char. Full
/// `to_lowercase()` can turn one char into several (ß→ss), which would break
/// the char-offset math a caret column rides on.
fn fold(c: char, sensitive: bool) -> char {
    if sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrences_are_byte_ranges_and_char_offsets() {
        // ASCII: byte range and char offset agree, and matches don't overlap.
        assert_eq!(occurrences("aa aa", "aa", true), vec![(0..2, 0), (3..5, 3)]);
        assert_eq!(occurrences("aaa", "aa", true), vec![(0..2, 0)]);
        // Case folding, and a multi-byte char ahead of the hit: the byte range
        // is wider than the char offset, and both must be right or the caret
        // and the highlight land in different places.
        assert_eq!(occurrences("Fée FOO", "foo", false), vec![(5..8, 4)]);
        assert_eq!(occurrences("Fée FOO", "foo", true), Vec::new());
        // A hit spanning a multi-byte char.
        assert_eq!(occurrences("aébc", "ÉB", false), vec![(1..4, 1)]);
        assert!(occurrences("anything", "", false).is_empty());
        assert!(occurrences("ab", "abc", false).is_empty());
    }

    #[test]
    fn snapshot_and_find_over_a_vault() {
        let root = std::env::temp_dir().join("darknotes_grep_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.md"), "# Alpha\n\n  indented Widget\nplain widget\n").unwrap();
        std::fs::write(root.join("sub/b.md"), "nothing here\nWIDGET again\n").unwrap();
        std::fs::write(root.join("c.txt"), "widget in a txt\n").unwrap();

        let vault = Vault::scan(&root);
        let lines = snapshot(&vault);
        // Blank lines dropped, text trimmed, non-`.md` skipped.
        assert!(lines.iter().all(|l| !l.text.is_empty()));
        assert!(lines.iter().all(|l| l.path.extension().unwrap() == "md"));
        let indented = lines.iter().find(|l| l.line == 2).expect("the indented line");
        assert_eq!(indented.text, "indented Widget");
        // The stripped indent, so `indent + occurrence offset` is the source
        // column: `Widget` sits at char 9 of the trimmed text, 11 of the line.
        assert_eq!(indented.indent, 2);
        assert_eq!(occurrences(&indented.text, "widget", false), vec![(9..15, 9)]);

        let hit = |q, sensitive| -> Vec<(String, usize)> {
            find(&lines, q, sensitive)
                .into_iter()
                .map(|i| (lines[i].text.clone(), lines[i].line))
                .collect()
        };
        // Insensitive: all three casings, in vault order — folders sort before
        // files, so `sub/b.md` leads.
        assert_eq!(
            hit("widget", false),
            vec![
                ("WIDGET again".to_string(), 1),
                ("indented Widget".to_string(), 2),
                ("plain widget".to_string(), 3),
            ]
        );
        // Sensitive keeps only the exact case; empty query matches nothing.
        assert_eq!(hit("Widget", true), vec![("indented Widget".to_string(), 2)]);
        assert!(hit("", false).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}


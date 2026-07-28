//! Cross-note jump history: the positions `Ctrl-O` walks back through.
//!
//! A pure ring + cursor, no gpui — the semantics that matter (a new jump drops
//! the forward tail; the first step back records the present so `Ctrl-I` can
//! return) are easy to get subtly wrong, so they're unit-tested here.

use std::path::PathBuf;

/// Vim's default jumplist length.
const CAP: usize = 100;

/// A place in the vault: the file, and where in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pos {
    pub path: PathBuf,
    /// Only for deduping adjacent entries; `offset` is what a jump restores.
    pub line: usize,
    pub offset: usize,
}

/// Positions jumped *from*, oldest first, plus a cursor for walking them.
/// `cursor == list.len()` means "at the present" — no step back taken yet.
// ponytail: `Vec` + index, so the cursor is a plain `usize`; the O(n) evict at
// CAP is 100 elements once per jump.
#[derive(Default)]
pub struct Jumps {
    list: Vec<Pos>,
    cursor: usize,
}

impl Jumps {
    /// Record a jump origin. Drops any forward history (a new jump ends the
    /// walk), and coalesces a second jump from the same line.
    pub fn push(&mut self, pos: Pos) {
        self.list.truncate(self.cursor);
        let same_line =
            self.list.last().is_some_and(|t| t.path == pos.path && t.line == pos.line);
        if !same_line {
            self.list.push(pos);
            if self.list.len() > CAP {
                self.list.remove(0);
            }
        }
        self.cursor = self.list.len();
    }

    /// One entry older, or `None` at the oldest. `here` (the current position)
    /// joins the list on the first step back so `forward` can return to it; a
    /// pathless buffer passes `None` and simply has nothing to return to.
    pub fn back(&mut self, here: Option<Pos>) -> Option<Pos> {
        if self.cursor == 0 {
            return None;
        }
        if self.cursor == self.list.len()
            && let Some(here) = here
        {
            self.list.push(here);
        }
        self.cursor -= 1;
        self.list.get(self.cursor).cloned()
    }

    /// One entry newer, or `None` at the present.
    pub fn forward(&mut self) -> Option<Pos> {
        if self.cursor + 1 >= self.list.len() {
            return None;
        }
        self.cursor += 1;
        self.list.get(self.cursor).cloned()
    }

    /// The position before the latest jump — vim's `'` mark, the target of
    /// `''`/`` `` ``.
    pub fn last(&self) -> Option<&Pos> {
        self.list.get(self.cursor.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(name: &str, line: usize) -> Pos {
        Pos { path: PathBuf::from(name), line, offset: line * 10 }
    }

    #[test]
    fn walks_back_and_forward() {
        let mut j = Jumps::default();
        assert_eq!(j.back(Some(at("a", 1))), None); // empty: nothing older
        j.push(at("a", 1));
        j.push(at("b", 2));
        // Back from the present records it, so forward can return.
        assert_eq!(j.back(Some(at("c", 3))), Some(at("b", 2)));
        assert_eq!(j.back(Some(at("c", 3))), Some(at("a", 1)));
        assert_eq!(j.back(Some(at("c", 3))), None); // oldest
        assert_eq!(j.forward(), Some(at("b", 2)));
        assert_eq!(j.forward(), Some(at("c", 3))); // where the walk began
        assert_eq!(j.forward(), None);
    }

    #[test]
    fn a_new_jump_drops_the_forward_tail() {
        let mut j = Jumps::default();
        j.push(at("a", 1));
        j.push(at("b", 2));
        assert_eq!(j.back(Some(at("c", 3))), Some(at("b", 2)));
        j.push(at("b", 2)); // jumping again from b ends the walk
        assert_eq!(j.forward(), None);
        assert_eq!(j.back(Some(at("d", 4))), Some(at("b", 2)));
    }

    #[test]
    fn same_line_and_cap() {
        let mut j = Jumps::default();
        j.push(at("a", 1));
        j.push(Pos { offset: 99, ..at("a", 1) }); // same line, other column
        assert_eq!(j.back(None), Some(at("a", 1)));
        assert_eq!(j.back(None), None); // one entry, not two

        let mut j = Jumps::default();
        for i in 0..CAP + 10 {
            j.push(at("a", i));
        }
        // Oldest evicted, newest kept.
        assert_eq!(j.last(), Some(&at("a", CAP + 9)));
        for _ in 0..CAP {
            j.back(None);
        }
        assert_eq!(j.back(None), None);
    }
}

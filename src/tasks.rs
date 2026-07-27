//! Vault task scanning: the one engine behind the agenda, the cockpit widgets,
//! and (later) `darknotes query`. A task is a GFM checkbox line anywhere in the
//! vault, with `due:`/`wait:`/`every:` tokens lifted off it.
//!
//! Reads from disk, so a task appears once its file is saved — `:capture`
//! writes disk directly, but a task typed into a dirty buffer waits for `:w`.
//! Preferring an open buffer's own text is a five-line change if that gap
//! starts to bite; it doubles the line-source path, so it stays unbuilt.
//!
//! Scanning is on-demand (agenda open), never per frame.

use std::path::{Path, PathBuf};

use jiff::{civil::Date, ToSpan};

use crate::markdown::task_box;
use crate::vault::Vault;

/// One checkbox line found in the vault.
pub struct Task {
    pub path: PathBuf,
    /// 0-based line index within `path`.
    pub line: usize,
    /// The task text with recognized tokens removed. A token whose value fails
    /// to parse stays in the text, so a typo (`due:tomorrow`) shows up as
    /// itself instead of being silently dropped.
    pub text: String,
    pub done: bool,
    pub due: Option<Date>,
    pub wait: Option<Date>,
}

/// Agenda grouping. Declaration order is the agenda's sort order, so undated
/// inbox items land last.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Bucket {
    Overdue,
    Today,
    Week,
    Later,
    Inbox,
}

impl Bucket {
    /// Column label for an agenda row.
    pub fn label(self) -> &'static str {
        match self {
            Bucket::Overdue => "OVERDUE",
            Bucket::Today => "TODAY",
            Bucket::Week => "WEEK",
            Bucket::Later => "LATER",
            Bucket::Inbox => "INBOX",
        }
    }
}

impl Task {
    /// Which agenda group this falls in, relative to `today`.
    pub fn bucket(&self, today: Date) -> Bucket {
        let Some(due) = self.due else {
            return Bucket::Inbox;
        };
        if due < today {
            Bucket::Overdue
        } else if due == today {
            Bucket::Today
        } else if today.checked_add(7.days()).is_ok_and(|week| due <= week) {
            Bucket::Week
        } else {
            Bucket::Later
        }
    }

    /// Agenda membership: open, not still waiting, and either dated or living in
    /// an agenda source. An undated task in an ordinary note is *local* — it
    /// stays with the prose it was typed into and never surfaces.
    fn in_agenda(&self, root: &Path, today: Date) -> bool {
        if self.done {
            return false;
        }
        // `wait:` only ever hides forward; once the date passes, the task is
        // ordinary again.
        if self.wait.is_some_and(|w| w > today) {
            return false;
        }
        self.due.is_some() || self.wait.is_some() || is_agenda_source(root, &self.path)
    }
}

/// Every task line in the vault, in vault file order. Unfiltered: the agenda
/// and the query language apply their own rules over this.
pub fn scan(vault: &Vault) -> Vec<Task> {
    let t0 = crate::perf::t0();
    let mut out = Vec::new();
    let mut read = 0;
    for path in vault.files.iter().filter(|p| p.extension().is_some_and(|e| e == "md")) {
        // A file that vanished since the vault scan simply contributes nothing.
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        read += 1;
        out.extend(
            text.lines().enumerate().filter_map(|(line, raw)| parse_line(path, line, raw)),
        );
    }
    crate::perf::task_scan_done(t0, read, out.len());
    out
}

/// The agenda list for `today`: surfaced open tasks, grouped-order first, then
/// by date, then by file position so the order is stable between opens.
pub fn agenda(vault: &Vault, today: Date) -> Vec<Task> {
    let mut tasks: Vec<Task> =
        scan(vault).into_iter().filter(|t| t.in_agenda(&vault.root, today)).collect();
    tasks.sort_by(|a, b| {
        (a.bucket(today), a.due, &a.path, a.line).cmp(&(b.bucket(today), b.due, &b.path, b.line))
    });
    tasks
}

/// `inbox.md` and the `daily/` tree — the notes whose undated tasks still count
/// as life-level.
fn is_agenda_source(root: &Path, path: &Path) -> bool {
    path == root.join("inbox.md") || path.starts_with(root.join("daily"))
}

/// Parse one line into a `Task`, or `None` if it holds no checkbox.
fn parse_line(path: &Path, line: usize, raw: &str) -> Option<Task> {
    let (at, done) = task_box(raw)?;
    let (mut due, mut wait) = (None, None);
    let mut words = Vec::new();
    // Tokens can sit anywhere on the line; whatever isn't one is the text.
    // Repeats are last-wins. A well-formed `every:` is recognized so it drops
    // out of the text, but its value goes unstored until completion-rewrite
    // needs the arithmetic.
    for word in raw[at + 3..].split_whitespace() {
        let taken = match word.split_once(':') {
            Some(("due", v)) => set_date(&mut due, v),
            Some(("wait", v)) => set_date(&mut wait, v),
            Some(("every", v)) => valid_every(v),
            _ => false,
        };
        if !taken {
            words.push(word);
        }
    }
    Some(Task { path: path.to_path_buf(), line, text: words.join(" "), done, due, wait })
}

/// Fill `slot` from an ISO `YYYY-MM-DD` value, reporting whether it parsed.
fn set_date(slot: &mut Option<Date>, v: &str) -> bool {
    match v.parse() {
        Ok(d) => {
            *slot = Some(d);
            true
        }
        Err(_) => false,
    }
}

/// `every:` shape check — `{N}{d|w|m|y}` with `N` positive.
fn valid_every(v: &str) -> bool {
    v.strip_suffix(['d', 'w', 'm', 'y']).is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn one(raw: &str) -> Task {
        parse_line(Path::new("n.md"), 0, raw).expect("a task line")
    }

    #[test]
    fn parse_line_lifts_tokens_off_the_text() {
        let t = one("- [ ] renew cert due:2026-07-21 every:1y wait:2026-06-01 #ops");
        assert_eq!(t.text, "renew cert #ops");
        assert_eq!(t.due, Some(date(2026, 7, 21)));
        assert_eq!(t.wait, Some(date(2026, 6, 1)));
        assert!(!t.done);

        // Checked box, indented, no tokens.
        let t = one("  - [x] ship it");
        assert!(t.done);
        assert_eq!(t.text, "ship it");
        assert_eq!(t.due, None);

        // Unparseable values stay visible rather than vanishing.
        let t = one("- [ ] pay rent due:tomorrow every:soon wait:07-04");
        assert_eq!(t.text, "pay rent due:tomorrow every:soon wait:07-04");
        assert_eq!(t.due, None);
        assert_eq!(t.wait, None);

        // Not tasks.
        assert!(parse_line(Path::new("n.md"), 0, "- a bullet").is_none());
        assert!(parse_line(Path::new("n.md"), 0, "just prose").is_none());
    }

    #[test]
    fn valid_every_shapes() {
        assert!(valid_every("1d") && valid_every("2w") && valid_every("18m") && valid_every("1y"));
        assert!(!valid_every("0d")); // a zero-length cycle would never advance
        assert!(!valid_every("d") && !valid_every("1x") && !valid_every("1") && !valid_every("-1d"));
    }

    #[test]
    fn buckets_split_on_today() {
        let today = date(2026, 7, 27);
        let at = |due: Option<Date>| Task {
            path: PathBuf::from("n.md"),
            line: 0,
            text: String::new(),
            done: false,
            due,
            wait: None,
        };
        assert_eq!(at(Some(date(2026, 7, 26))).bucket(today), Bucket::Overdue);
        assert_eq!(at(Some(today)).bucket(today), Bucket::Today);
        assert_eq!(at(Some(date(2026, 8, 3))).bucket(today), Bucket::Week); // today + 7, inclusive
        assert_eq!(at(Some(date(2026, 8, 4))).bucket(today), Bucket::Later);
        assert_eq!(at(None).bucket(today), Bucket::Inbox);
        // Sort order runs overdue → … → inbox.
        assert!(Bucket::Overdue < Bucket::Today && Bucket::Later < Bucket::Inbox);
    }

    #[test]
    fn agenda_surfaces_dated_and_agenda_sources_only() {
        let root = std::env::temp_dir().join("darknotes_agenda_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("daily")).unwrap();
        let today = date(2026, 7, 27);

        std::fs::write(
            root.join("notes.md"),
            "# Work\n- [ ] local, stays put\n- [ ] dated one due:2026-07-28\n- [x] done due:2026-07-20\n",
        )
        .unwrap();
        std::fs::write(root.join("inbox.md"), "- [ ] captured, undated\n").unwrap();
        std::fs::write(root.join("daily/2026-07-27.md"), "- [ ] from the daily note\n").unwrap();
        // Waiting hides forward only.
        std::fs::write(
            root.join("later.md"),
            "- [ ] snoozed wait:2026-08-01\n- [ ] unsnoozed wait:2026-07-01\n",
        )
        .unwrap();

        let vault = Vault::scan(&root);
        let got: Vec<String> = agenda(&vault, today).into_iter().map(|t| t.text).collect();
        // Week before Inbox, then by path: `daily/` < `inbox.md` < `later.md`.
        // The undated local task and the done one never appear; neither does the
        // still-snoozed one. An elapsed `wait:` counts as dated, so it surfaces
        // — with no `due:`, into Inbox.
        assert_eq!(
            got,
            vec!["dated one", "from the daily note", "captured, undated", "unsnoozed"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

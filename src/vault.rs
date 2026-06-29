use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A vault is a root folder plus the markdown files found beneath it. `tree` is
/// the nested folder/file structure for the sidebar; `files` is the same files
/// flattened, for navigation and `:e`/`:w` resolution. Hidden dirs (`.git`,
/// `.obsidian`) are skipped and folders with no `.md` beneath them are omitted.
pub struct Vault {
    pub root: PathBuf,
    pub tree: Vec<Entry>,
    pub files: Vec<PathBuf>,
}

/// A node in the folder tree: a folder with children, or a markdown file.
pub enum Entry {
    Dir {
        name: String,
        path: PathBuf,
        children: Vec<Entry>,
    },
    File {
        name: String,
        path: PathBuf,
    },
}

/// One flattened, currently-visible sidebar row. `depth` drives indentation;
/// `expanded` is meaningful only for folders.
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
}

impl Vault {
    pub fn scan(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let tree = build_dir(&root);
        let mut files = Vec::new();
        collect_files(&tree, &mut files);
        Self { root, tree, files }
    }

    /// Flatten the tree to the rows currently visible: top-level entries always,
    /// a folder's children only when its path is in `expanded`.
    pub fn visible_rows(&self, expanded: &HashSet<PathBuf>) -> Vec<Row> {
        let mut out = Vec::new();
        push_rows(&self.tree, 0, expanded, &mut out);
        out
    }
}

fn push_rows(entries: &[Entry], depth: usize, expanded: &HashSet<PathBuf>, out: &mut Vec<Row>) {
    for e in entries {
        match e {
            Entry::Dir { name, path, children } => {
                let open = expanded.contains(path);
                out.push(Row {
                    depth,
                    name: name.clone(),
                    path: path.clone(),
                    is_dir: true,
                    expanded: open,
                });
                if open {
                    push_rows(children, depth + 1, expanded, out);
                }
            }
            Entry::File { name, path } => out.push(Row {
                depth,
                name: name.clone(),
                path: path.clone(),
                is_dir: false,
                expanded: false,
            }),
        }
    }
}

// ponytail: plain recursion over read_dir — no walkdir dep. Fine for vault-sized
// trees; revisit if someone points it at a huge directory.
//
// Returns `dir`'s children: subfolders first (each recursed into), then files,
// both alphabetical by name. Folders with no `.md` anywhere beneath are dropped
// so the sidebar has no dead, un-openable rows.
fn build_dir(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !is_hidden(&path) {
                let children = build_dir(&path);
                if !children.is_empty() {
                    dirs.push(Entry::Dir { name: file_name(&path), path, children });
                }
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            files.push(Entry::File { name: file_name(&path), path });
        }
    }
    dirs.sort_by(|a, b| entry_name(a).cmp(entry_name(b)));
    files.sort_by(|a, b| entry_name(a).cmp(entry_name(b)));
    dirs.extend(files);
    dirs
}

fn collect_files(entries: &[Entry], out: &mut Vec<PathBuf>) {
    for e in entries {
        match e {
            Entry::Dir { children, .. } => collect_files(children, out),
            Entry::File { path, .. } => out.push(path.clone()),
        }
    }
}

fn entry_name(e: &Entry) -> &str {
    match e {
        Entry::Dir { name, .. } | Entry::File { name, .. } => name,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth_names(rows: &[Row]) -> Vec<(usize, String)> {
        rows.iter().map(|r| (r.depth, r.name.clone())).collect()
    }

    #[test]
    fn scan_finds_md_recursively_skipping_hidden() {
        let root = std::env::temp_dir().join("darknotes_vault_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::write(root.join("sub/c.md"), "c").unwrap();
        std::fs::write(root.join(".hidden/d.md"), "d").unwrap();

        let vault = Vault::scan(&root);
        // Folders sort before files, so the nested file leads the flat list.
        let names: Vec<String> = vault
            .files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, vec!["sub/c.md", "a.md"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn visible_rows_expand_collapse() {
        let root = std::env::temp_dir().join("darknotes_vault_rows_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("sub/c.md"), "c").unwrap();

        let vault = Vault::scan(&root);
        let mut expanded = HashSet::new();

        // Collapsed: the folder and the root file, both at depth 0.
        assert_eq!(
            depth_names(&vault.visible_rows(&expanded)),
            vec![(0, "sub".to_string()), (0, "a.md".to_string())]
        );

        // Expanded: the folder's child appears between, indented one level.
        expanded.insert(root.join("sub"));
        assert_eq!(
            depth_names(&vault.visible_rows(&expanded)),
            vec![
                (0, "sub".to_string()),
                (1, "c.md".to_string()),
                (0, "a.md".to_string()),
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

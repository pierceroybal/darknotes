use std::path::{Path, PathBuf};

/// A vault is a root folder plus the markdown files found beneath it. Recursive,
/// hidden dirs (`.git`, `.obsidian`) skipped, results sorted for stable order.
pub struct Vault {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
}

impl Vault {
    pub fn scan(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut files = Vec::new();
        collect_md(&root, &mut files);
        files.sort();
        Self { root, files }
    }
}

// ponytail: plain recursion over read_dir — no walkdir dep. Fine for vault-sized
// trees; revisit if someone points it at a huge directory.
fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !is_hidden(&path) {
                collect_md(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(names, vec!["a.md", "sub/c.md"]);

        let _ = std::fs::remove_dir_all(&root);
    }
}

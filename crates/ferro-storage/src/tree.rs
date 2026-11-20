//! The analyzer's view of the size tree: nodes from `SizeTree` responses, with child
//! lists loaded lazily as folders are expanded.

use ferro_core::index::flags;
use ferro_ipc::{SizeNode, SizeRest};

/// Which size the views show and sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// Logical file sizes.
    Size,
    /// Bytes allocated on disk.
    OnDisk,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    /// Full path; volume roots end with `\`; empty for "all volumes".
    pub path: String,
    pub size: u64,
    pub alloc_size: u64,
    pub files: u64,
    pub folders: u64,
    pub mtime: u64,
    pub flags: u16,
    pub child_count: u32,
    /// `None` until loaded (the response stopped at its depth limit).
    pub children: Option<Vec<Node>>,
    /// Children the service summed instead of listing.
    pub rest: Option<SizeRest>,
    pub expanded: bool,
    /// An expand request is in flight.
    pub loading: bool,
}

impl Node {
    /// Converts a response node located at `path`.
    pub fn from_ipc(node: SizeNode, path: String) -> Self {
        let loaded = !node.children.is_empty() || node.child_count == 0;
        let children = loaded.then(|| {
            node.children
                .into_iter()
                .map(|child| {
                    let child_path = join(&path, &child.name);
                    Node::from_ipc(child, child_path)
                })
                .collect()
        });
        Self {
            name: node.name,
            path,
            size: node.size,
            alloc_size: node.alloc_size,
            files: node.files,
            folders: node.folders,
            mtime: node.mtime,
            flags: node.flags,
            child_count: node.child_count,
            children,
            rest: node.rest,
            expanded: false,
            loading: false,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    pub fn has_children(&self) -> bool {
        self.child_count > 0
    }

    pub fn measure(&self, measure: Measure) -> u64 {
        match measure {
            Measure::Size => self.size,
            Measure::OnDisk => self.alloc_size,
        }
    }

    /// Replaces the children with those of a freshly loaded node for the same path.
    pub fn set_children(&mut self, loaded: Node) {
        self.children = loaded.children;
        self.rest = loaded.rest;
        self.child_count = loaded.child_count;
        self.loading = false;
    }

    /// The node at `path` (case-insensitive) in this subtree.
    pub fn find_mut(&mut self, path: &str) -> Option<&mut Node> {
        if self.path.eq_ignore_ascii_case(path) {
            return Some(self);
        }
        self.children
            .as_mut()?
            .iter_mut()
            .find(|child| is_within(path, &child.path))?
            .find_mut(path)
    }

    /// Sorts every loaded child list by `measure`, largest first.
    pub fn sort(&mut self, measure: Measure) {
        if let Some(children) = &mut self.children {
            children.sort_by_key(|c| std::cmp::Reverse(c.measure(measure)));
            for child in children {
                child.sort(measure);
            }
        }
    }
}

/// Path of `name` inside `folder` (`""` + `C:\` = `C:\`; `C:\` + `a` = `C:\a`).
pub fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_owned()
    } else if folder.ends_with('\\') {
        format!("{folder}{name}")
    } else {
        format!("{folder}\\{name}")
    }
}

/// True if `path` is `folder` or lies below it (case-insensitive).
pub fn is_within(path: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    match path.get(..folder.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(folder) => {
            folder.ends_with('\\')
                || path.len() == folder.len()
                || path[folder.len()..].starts_with('\\')
        }
        _ => false,
    }
}

/// Parent folder of `path`: `C:\a\b` -> `C:\a`, `C:\a` -> `C:\`, `C:\` -> `""` (all
/// volumes).
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('\\');
    match trimmed.rsplit_once('\\') {
        Some((folder, _)) if folder.ends_with(':') => format!("{folder}\\"),
        Some((folder, _)) => folder.to_owned(),
        None => String::new(),
    }
}

/// Breadcrumb parts: `C:\Users\me` -> `[("C:\", "C:\"), ("Users", "C:\Users"), ("me",
/// "C:\Users\me")]`.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, component) in path.split('\\').filter(|c| !c.is_empty()).enumerate() {
        if i == 0 {
            current = format!("{component}\\");
            parts.push((current.clone(), current.clone()));
        } else {
            current = join(&current, component);
            parts.push((component.to_owned(), current.clone()));
        }
    }
    parts
}

/// Extension of a file name, lower-case, without the dot (`""` if none).
pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(pos) if pos > 0 => name[pos + 1..].to_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipc(name: &str, size: u64, children: Vec<SizeNode>, child_count: u32) -> SizeNode {
        SizeNode {
            name: name.into(),
            size,
            flags: flags::DIR,
            child_count,
            children,
            ..SizeNode::default()
        }
    }

    #[test]
    fn paths_and_lazy_children() {
        let response = ipc(
            "",
            30,
            vec![ipc(
                r"C:\",
                30,
                vec![ipc("Users", 20, vec![], 3), ipc("Empty", 0, vec![], 0)],
                2,
            )],
            1,
        );
        let mut root = Node::from_ipc(response, String::new());
        let users = root.find_mut(r"c:\users").unwrap();
        assert_eq!(users.path, r"C:\Users");
        assert!(users.children.is_none(), "not loaded yet");
        users.set_children(Node::from_ipc(
            ipc("Users", 20, vec![ipc("me", 20, vec![], 0)], 1),
            r"C:\Users".into(),
        ));
        assert_eq!(root.find_mut(r"C:\Users\me").unwrap().path, r"C:\Users\me");
        assert!(root.find_mut(r"C:\Empty").unwrap().children.is_some());
        assert!(root.find_mut(r"C:\Use").is_none());
    }

    #[test]
    fn path_helpers() {
        assert_eq!(join("", r"C:\"), r"C:\");
        assert_eq!(join(r"C:\", "a"), r"C:\a");
        assert_eq!(join(r"C:\a", "b"), r"C:\a\b");
        assert!(is_within(r"C:\a\b", r"c:\A"));
        assert!(is_within(r"C:\a", r"C:\"));
        assert!(!is_within(r"C:\ab", r"C:\a"));
        assert_eq!(parent(r"C:\a\b"), r"C:\a");
        assert_eq!(parent(r"C:\a"), r"C:\");
        assert_eq!(parent(r"C:\"), "");
        let crumbs: Vec<_> = breadcrumbs(r"C:\Users\me")
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(crumbs, [r"C:\", r"C:\Users", r"C:\Users\me"]);
        assert_eq!(extension("Movie.MKV"), "mkv");
        assert_eq!(extension(".gitignore"), "");
    }
}

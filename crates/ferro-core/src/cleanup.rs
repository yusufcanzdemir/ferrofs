//! Developer artifacts for the cleanup scanner (SPEC §5.4): regenerable build output and
//! dependency folders, recognized by name plus a project marker next to them.
//!
//! Folders inside installed software are never reported: shipped applications (Electron
//! apps, editor extensions, global npm packages) also contain `node_modules` and the like,
//! and deleting those breaks them. Only the topmost artifact of a nested chain is reported
//! (a `node_modules` inside another one is already part of its size).

use std::collections::HashSet;

use rayon::prelude::*;

use crate::index::{Index, flags};
use crate::sizes::SizeTotals;

/// Slots per parallel work item.
const CHUNK: usize = 1 << 15;
/// Deepest ancestor chain inspected (guards against corrupt cycles).
const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// `node_modules` next to `package.json` and a lock file.
    NodeModules,
    /// `target` next to `Cargo.toml`.
    RustTarget,
    /// `target` next to `pom.xml`.
    MavenTarget,
    /// Project-level `.gradle` next to a Gradle build file.
    GradleCache,
    /// `build` next to a project file (may be a hand-written folder in some projects).
    Build,
    /// `dist` next to a project file.
    Dist,
    /// `__pycache__`.
    PyCache,
    /// `.venv` / `venv` containing `pyvenv.cfg` (opt-in).
    PythonVenv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub slot: u32,
}

/// Ancestor folder names (folded) that mark installed software or system areas.
const EXCLUDED_ANYWHERE: &[&[u8]] = &[
    b"appdata",
    b"program files",
    b"program files (x86)",
    b"programdata",
    b"$recycle.bin",
    b"system volume information",
    b".vscode",
    b".vscode-insiders",
    b".cursor",
    b".windsurf",
    b"site-packages",
    b".venv",
    b"venv",
];
/// Top-level folder names (folded) excluded as a whole.
const EXCLUDED_TOP: &[&[u8]] = &[b"windows", b"windows.old"];

const NODE_LOCKS: &[&[u8]] = &[
    b"package-lock.json",
    b"yarn.lock",
    b"pnpm-lock.yaml",
    b"bun.lockb",
    b"bun.lock",
    b"npm-shrinkwrap.json",
];
const GRADLE_FILES: &[&[u8]] = &[
    b"build.gradle",
    b"build.gradle.kts",
    b"settings.gradle",
    b"settings.gradle.kts",
    b"gradlew",
];
const BUILD_MARKERS: &[&[u8]] = &[
    b"package.json",
    b"build.gradle",
    b"build.gradle.kts",
    b"cmakelists.txt",
    b"pyproject.toml",
    b"setup.py",
    b"gradlew",
];
const DIST_MARKERS: &[&[u8]] = &[b"package.json", b"pyproject.toml", b"setup.py"];

/// Developer artifacts below `folder` (`None` = whole volume). `totals` must have been
/// built without a filter.
pub fn find_artifacts(
    index: &Index,
    totals: &SizeTotals,
    folder: Option<u32>,
    include_venv: bool,
) -> Vec<Artifact> {
    let below = totals.folder_below(folder);
    let found: Vec<Artifact> = (0..index.slot_count().div_ceil(CHUNK))
        .into_par_iter()
        .flat_map_iter(|chunk| {
            let end = ((chunk + 1) * CHUNK).min(index.slot_count());
            let below = &below;
            (chunk * CHUNK..end).filter_map(move |slot| {
                // Slot counts are bounded by u32 (see Index::alloc_slot).
                #[allow(clippy::cast_possible_truncation)]
                let slot = slot as u32;
                if !index.is_live(slot)
                    || index.flags(slot) & (flags::DIR | flags::REPARSE) != flags::DIR
                {
                    return None;
                }
                let kind = classify(index, totals, slot, include_venv)?;
                (below(slot) && !excluded(index, slot)).then_some(Artifact { kind, slot })
            })
        })
        .collect();
    // Keep only the topmost of nested artifacts.
    let slots: HashSet<u32> = found.iter().map(|a| a.slot).collect();
    found
        .into_iter()
        .filter(|a| !ancestors(index, a.slot).any(|s| slots.contains(&s)))
        .collect()
}

/// The artifact kind of folder `slot`, if its name and surroundings match.
fn classify(
    index: &Index,
    totals: &SizeTotals,
    slot: u32,
    include_venv: bool,
) -> Option<ArtifactKind> {
    let parent = index.parent_of(slot);
    let sibling = |names: &[&[u8]]| totals.has_child_named(index, parent, names);
    match index.folded_name(slot) {
        b"node_modules" => (sibling(&[b"package.json"]) && sibling(NODE_LOCKS))
            .then_some(ArtifactKind::NodeModules),
        b"target" if sibling(&[b"cargo.toml"]) => Some(ArtifactKind::RustTarget),
        b"target" if sibling(&[b"pom.xml"]) => Some(ArtifactKind::MavenTarget),
        b".gradle" => sibling(GRADLE_FILES).then_some(ArtifactKind::GradleCache),
        b"build" => sibling(BUILD_MARKERS).then_some(ArtifactKind::Build),
        b"dist" => sibling(DIST_MARKERS).then_some(ArtifactKind::Dist),
        b"__pycache__" => Some(ArtifactKind::PyCache),
        b".venv" | b"venv" if include_venv => totals
            .has_child_named(index, Some(slot), &[b"pyvenv.cfg"])
            .then_some(ArtifactKind::PythonVenv),
        _ => None,
    }
}

/// Parent folders of `slot`, nearest first.
fn ancestors(index: &Index, slot: u32) -> impl Iterator<Item = u32> + '_ {
    std::iter::successors(index.parent_of(slot), move |&s| index.parent_of(s)).take(MAX_DEPTH)
}

/// True if the folder lies in installed software or a system area.
fn excluded(index: &Index, slot: u32) -> bool {
    let mut top = slot;
    for ancestor in ancestors(index, slot) {
        if EXCLUDED_ANYWHERE.contains(&index.folded_name(ancestor)) {
            return true;
        }
        top = ancestor;
    }
    top != slot && EXCLUDED_TOP.contains(&index.folded_name(top))
}

/// Marker file names (folded) that justified an artifact, for re-checking before deletion.
pub fn markers(kind: ArtifactKind) -> &'static [&'static [u8]] {
    match kind {
        ArtifactKind::NodeModules => &[b"package.json"],
        ArtifactKind::RustTarget => &[b"cargo.toml"],
        ArtifactKind::MavenTarget => &[b"pom.xml"],
        ArtifactKind::GradleCache => GRADLE_FILES,
        ArtifactKind::Build => BUILD_MARKERS,
        ArtifactKind::Dist => DIST_MARKERS,
        ArtifactKind::PyCache => &[],
        ArtifactKind::PythonVenv => &[b"pyvenv.cfg"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FoldOptions;
    use crate::index::{Name, NewEntry};

    struct Tree {
        index: Index,
        next: u64,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                index: Index::new("C:\\", FoldOptions::default()),
                next: 20,
            }
        }

        fn add(&mut self, parent: u64, name: &str, dir: bool) -> u64 {
            let frn = self.next;
            self.next += 1;
            self.index
                .upsert(&NewEntry {
                    frn,
                    parent_frn: parent,
                    name: Name::Utf8(name),
                    attributes: if dir { 0x10 } else { 0x20 },
                    size: if dir { 0 } else { 1000 },
                    alloc_size: 0,
                    mtime: 0,
                })
                .unwrap();
            frn
        }

        fn project(&mut self, parent: u64, name: &str, files: &[&str], dirs: &[&str]) -> u64 {
            let project = self.add(parent, name, true);
            for f in files {
                self.add(project, f, false);
            }
            for d in dirs {
                let dir = self.add(project, d, true);
                self.add(dir, "inner.bin", false);
            }
            project
        }
    }

    fn found(tree: &Tree, include_venv: bool) -> Vec<(ArtifactKind, String)> {
        let totals = SizeTotals::build(&tree.index, None);
        let mut list: Vec<_> = find_artifacts(&tree.index, &totals, None, include_venv)
            .into_iter()
            .map(|a| (a.kind, tree.index.path(a.slot)))
            .collect();
        list.sort_by(|a, b| a.1.cmp(&b.1));
        list
    }

    #[test]
    fn artifacts_need_markers_and_skip_installed_software() {
        let mut t = Tree::new();
        let dev = t.add(5, "dev", true);
        t.project(
            dev,
            "web",
            &["package.json", "package-lock.json"],
            &["node_modules", "dist"],
        );
        t.project(dev, "app", &["Cargo.toml"], &["target"]);
        t.project(
            dev,
            "java",
            &["pom.xml", "build.gradle"],
            &["target", ".gradle", "build"],
        );
        // No lock file / no marker: not artifacts.
        t.project(dev, "shipped", &["package.json"], &["node_modules"]);
        t.project(dev, "docs", &["README.md"], &["build", "target"]);
        // Python: cache folders, venv only when asked, caches inside a venv never.
        let py = t.project(dev, "py", &["pyproject.toml"], &["__pycache__"]);
        let venv = t.add(py, ".venv", true);
        t.add(venv, "pyvenv.cfg", false);
        let lib = t.add(venv, "Lib", true);
        t.project(lib, "pkg", &[], &["__pycache__"]);
        // Installed software.
        let users = t.add(5, "Users", true);
        let me = t.add(users, "me", true);
        let appdata = t.add(me, "AppData", true);
        t.project(
            appdata,
            "tool",
            &["package.json", "yarn.lock"],
            &["node_modules"],
        );
        let vscode = t.add(me, ".vscode", true);
        t.project(
            vscode,
            "ext",
            &["package.json", "package-lock.json"],
            &["node_modules"],
        );
        let windows = t.add(5, "Windows", true);
        t.project(windows, "x", &["Cargo.toml"], &["target"]);
        // Nested: only the outer node_modules.
        let mono = t.project(dev, "mono", &["package.json", "yarn.lock"], &[]);
        let outer = t.add(mono, "node_modules", true);
        t.project(
            outer,
            "dep",
            &["package.json", "yarn.lock"],
            &["node_modules"],
        );

        let kinds = found(&t, false);
        let expected = [
            (ArtifactKind::GradleCache, r"C:\dev\java\.gradle"),
            (ArtifactKind::Build, r"C:\dev\java\build"),
            (ArtifactKind::MavenTarget, r"C:\dev\java\target"),
            (ArtifactKind::NodeModules, r"C:\dev\mono\node_modules"),
            (ArtifactKind::RustTarget, r"C:\dev\app\target"),
            (ArtifactKind::PyCache, r"C:\dev\py\__pycache__"),
            (ArtifactKind::Dist, r"C:\dev\web\dist"),
            (ArtifactKind::NodeModules, r"C:\dev\web\node_modules"),
        ];
        let mut expected: Vec<_> = expected
            .iter()
            .map(|(k, p)| (*k, (*p).to_owned()))
            .collect();
        expected.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(kinds, expected);

        let with_venv = found(&t, true);
        assert!(with_venv.contains(&(ArtifactKind::PythonVenv, r"C:\dev\py\.venv".to_owned())));
        assert_eq!(with_venv.len(), expected.len() + 1);
    }

    #[test]
    fn search_can_be_limited_to_a_folder() {
        let mut t = Tree::new();
        let a = t.add(5, "a", true);
        t.project(a, "one", &["Cargo.toml"], &["target"]);
        let b = t.add(5, "b", true);
        t.project(b, "two", &["Cargo.toml"], &["target"]);
        let totals = SizeTotals::build(&t.index, None);
        let a_slot = totals.resolve(&t.index, "a").unwrap();
        let inside = find_artifacts(&t.index, &totals, a_slot, false);
        assert_eq!(inside.len(), 1);
        assert_eq!(t.index.path(inside[0].slot), r"C:\a\one\target");
        assert_eq!(markers(ArtifactKind::RustTarget), [b"cargo.toml"]);
    }
}

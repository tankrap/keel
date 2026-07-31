//! `.gitignore` / `.keelignore` support — so `keel status`, `diff`, and snapshotting skip exactly
//! what git skips. Without it keel versioned build artifacts git ignores (on the linux kernel that
//! was 329 "changes" vs git's 13).
//!
//! Uses ripgrep's `ignore::gitignore` matcher for real semantics (anchoring, `!` negation, `**`).
//! Rules nest like git's: a directory's own `.gitignore`/`.keelignore` layers on top of its
//! parents, so [`Ignore::descend`] returns a child matcher as the recursive walk enters a
//! subdirectory — and that layer's patterns are matched against paths relative to *that* directory,
//! exactly as git resolves a nested `.gitignore`. Two internals are ALWAYS ignored regardless of
//! any rule: `.git` and keel's own `.keel` store.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
struct Layer {
    prefix: String, // root-relative directory the layer's rules are anchored at ("" = repo root)
    gi: Arc<Gitignore>,
}

/// A stack of gitignore matchers, root-first. Cheap to clone (Arc'd layers) so the recursive walk
/// can branch a child matcher per subdirectory.
#[derive(Clone, Default)]
pub struct Ignore {
    layers: Vec<Layer>,
}

impl Ignore {
    /// Build the root matcher from `<root>/.gitignore` and `<root>/.keelignore` (either may be
    /// absent). `.keelignore` layers after `.gitignore`, so it can add or (with `!`) re-include.
    pub fn load(root: &Path) -> Ignore {
        let mut layers = Vec::new();
        if let Some(gi) = build_layer(root) {
            layers.push(Layer { prefix: String::new(), gi });
        }
        Ignore { layers }
    }

    /// Extend the stack with the ignore files in `abs_dir` (a subdirectory whose root-relative path
    /// is `rel_dir`) as the walk enters it. Returns an unchanged clone if the directory carries no
    /// ignore file.
    pub fn descend(&self, rel_dir: &str, abs_dir: &Path) -> Ignore {
        match build_layer(abs_dir) {
            Some(gi) => {
                let mut layers = self.layers.clone();
                layers.push(Layer { prefix: rel_dir.to_string(), gi });
                Ignore { layers }
            }
            None => self.clone(),
        }
    }

    /// One-shot resolution for a single path — loads the root matcher and descends through every
    /// ancestor directory of `rel` (reading their ignore files) before checking. For fs-watch
    /// events, where there's no pre-descended matcher to carry down a recursive walk.
    pub fn resolve(root: &Path, rel: &str, is_dir: bool) -> bool {
        let mut ig = Ignore::load(root);
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        let mut abs = root.to_path_buf();
        let mut prefix = String::new();
        for comp in comps.iter().take(comps.len().saturating_sub(1)) {
            abs.push(comp);
            prefix = if prefix.is_empty() { (*comp).to_string() } else { format!("{prefix}/{comp}") };
            ig = ig.descend(&prefix, &abs);
        }
        ig.is_ignored(rel, is_dir)
    }

    /// Whether `rel` (path relative to the repo root, `/`-separated) is ignored. `is_dir` matters:
    /// a `build/` rule ignores the directory but not a file named `build`. `.git` and `.keel` are
    /// always ignored. The deepest layer with an opinion wins; within a layer a later `!` re-include
    /// beats an earlier ignore (standard gitignore precedence).
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel.split('/').any(|c| c == ".git" || c == ".keel") {
            return true;
        }
        for layer in self.layers.iter().rev() {
            // Match each layer against the path relative to that layer's own directory.
            let sub = if layer.prefix.is_empty() {
                Some(rel)
            } else {
                rel.strip_prefix(&layer.prefix).and_then(|s| s.strip_prefix('/'))
            };
            if let Some(sub) = sub {
                match layer.gi.matched(sub, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }
        false
    }
}

/// Build one matcher layer from a directory's `.gitignore` + `.keelignore`, or `None` if neither
/// exists. Patterns are anchored at `dir` (so a leading-`/` pattern is relative to it).
fn build_layer(dir: &Path) -> Option<Arc<Gitignore>> {
    let git = dir.join(".gitignore");
    let keel = dir.join(".keelignore");
    if !git.exists() && !keel.exists() {
        return None;
    }
    let mut b = GitignoreBuilder::new(dir);
    let _ = b.add(&git); // add() returns Option<Error>; a missing/broken file contributes nothing
    let _ = b.add(&keel);
    b.build().ok().map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn honors_gitignore_semantics_including_negation_anchoring_and_nesting() {
        let dir = TmpDir::new();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "*.log\nbuild/\n/only-root.txt\n!keep.log\n").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "*.tmp\n").unwrap();

        let ig = Ignore::load(root);
        // basename glob, at any depth
        assert!(ig.is_ignored("a.log", false));
        assert!(ig.is_ignored("sub/b.log", false));
        // negation re-includes
        assert!(!ig.is_ignored("keep.log", false));
        // directory-only rule: ignores the dir (walk won't descend), not a file of that name
        assert!(ig.is_ignored("build", true));
        assert!(!ig.is_ignored("build", false));
        // anchored rule matches only at root
        assert!(ig.is_ignored("only-root.txt", false));
        assert!(!ig.is_ignored("sub/only-root.txt", false));
        // internals always ignored
        assert!(ig.is_ignored(".git", true));
        assert!(ig.is_ignored(".keel", true));
        // a plain source file is kept
        assert!(!ig.is_ignored("src/main.rs", false));

        // nested .gitignore: `*.tmp` applies under sub/ (relative to sub), not at root
        let ig_sub = ig.descend("sub", &root.join("sub"));
        assert!(ig_sub.is_ignored("sub/x.tmp", false));
        assert!(!ig.is_ignored("x.tmp", false), "root has no *.tmp rule");
    }
}

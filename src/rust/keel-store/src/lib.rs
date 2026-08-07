//! keel-store — the native content-addressed object store.
//!
//! Objects (blob / tree / change / session) are content-addressed with **BLAKE3**
//! over an explicit, versioned canonical binary encoding. The address is
//! `id(obj) = BLAKE3(encode(obj))`, and `encode` is deterministic and byte-stable —
//! the same logical object always produces the same bytes and therefore the same id.
//! Tree entries are canonically sorted by name (order is not meaningful); change
//! parents preserve order (first parent = mainline). This is the substrate the whole
//! system rests on (Linear NEW-1074).
//!
//! Layers: [`object`] (model + encoding), [`store`] (LMDB backing + FastCDC dedup),
//! [`snapshot`] (working-tree ⇄ trees/blobs), [`repo`] (change DAG + refs).
//!
//! Addressing note: the prototype used BLAKE2b to stay byte-identical with the Node
//! reference store. This native store is greenfield (git backing is dropped), so we
//! use BLAKE3 — faster on large blobs (SIMD + parallel tree hashing) and its tree
//! structure enables verified incremental streaming.

pub mod chunk;
pub mod delta;
pub mod ignorerules;
pub mod livestatus;
pub mod object;
pub mod repo;
pub mod semdiff;
pub mod sessiontag;
pub mod shred;
pub mod snapshot;
pub mod store;
pub mod textdiff;
pub mod vfs;

pub use livestatus::LiveStatus;
pub use object::{
    Change, DecodeError, Object, ObjectId, Review, Session, Tree, TreeEntry, Verdict, Verification,
};
pub use repo::{ChangeKind, PathChange, Repo};
pub use shred::SecretState;
pub use textdiff::{diff_lines, DiffLine, Hunk, Tag};
pub use semdiff::{summarize as semantic_summarize, AddedLine, Anomaly, ChangeGroup, GroupKind, SemanticSummary};
pub use store::{Store, StoreError};
pub use vfs::{Attr, DirEntry, NodeKind, Stats as VfsStats, Vfs};

/// The daemon's Unix socket path for `root`. Normally `<root>/.keel/daemon.sock`, but Unix socket
/// paths are capped (`sun_path` is 104 bytes on macOS, 108 on Linux); for a deep repo path that
/// limit is easily exceeded, and `bind()` fails so the daemon silently never comes up. When the
/// natural path is too long, fall back to a short, stable path under `/tmp` keyed by a hash of the
/// canonicalized root, so the daemon and every client independently agree on the same socket.
///
/// The daemon and CLI MUST both call this so they compute the identical path.
pub fn daemon_socket_path(root: &std::path::Path) -> std::path::PathBuf {
    let natural = root.join(".keel/daemon.sock");
    // Leave headroom below the smaller (macOS) limit for the trailing NUL.
    const MAX_SUN_PATH: usize = 100;
    if natural.as_os_str().len() <= MAX_SUN_PATH {
        return natural;
    }
    // Hash the canonical root (fall back to the raw path if canonicalize fails) so the short path
    // is stable across processes and unique per repo.
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = blake3::hash(canon.as_os_str().as_encoded_bytes());
    std::path::PathBuf::from(format!("/tmp/keel-{}.sock", &digest.to_hex()[..16]))
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique, self-cleaning temp directory (avoids a tempfile dependency).
    pub struct TmpDir(pub PathBuf);
    impl TmpDir {
        pub fn new() -> TmpDir {
            static C: AtomicU64 = AtomicU64::new(0);
            let n = C.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("keel-test-{}-{}", std::process::id(), n));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

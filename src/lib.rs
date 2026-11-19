//! termdisk: a disk usage analyzer for the terminal. The library scans a folder into a compact
//! tree and shows it in a `View` drawn into a ratatui `Buffer`; it has no terminal code, so a
//! host such as termcode can embed it.
//!
//! Embedding: give `View` the keys and clicks, `draw` it into the host's area (it draws
//! nothing outside it), call `tick` when `next_wake` says, and act on the `Outcome`. The view
//! asks before anything goes to the trash. `trash` itself does every safety check but asks
//! nothing: a host that calls it directly must confirm with the user itself.

mod draw;
mod guard;
mod scan;
mod sys;
mod trash;
pub mod tree;
pub mod treemap;
mod view;

pub use guard::{check, trash, Policy, Refusal, Trashed};
pub use scan::{scan, Progress, Scan};
pub use tree::{Node, NodeId, Tree};
pub use view::{Outcome, View};

/// A size as baobab shows it (GLib's `g_format_size`): SI units, one decimal.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let (mut v, mut u) = (bytes as f64 / 1000.0, 0);
    // 999.95 and up would print as "1000.0"; that is the next unit's "1.0".
    while v >= 999.95 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// A folder of its own under the system temp dir, removed with everything in it when dropped.
/// Every test that writes files works in one of these.
#[cfg(test)]
pub(crate) struct TempDir(pub std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "termdisk-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        );
        let p = std::env::temp_dir().join(name);
        std::fs::create_dir(&p).unwrap();
        TempDir(p)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let ours = self.0.parent() == Some(&std::env::temp_dir())
            && self
                .0
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("termdisk-"));
        if ours {
            let _ = std::fs::remove_dir_all(&self.0); // test-only cleanup
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_size;

    #[test]
    fn fmt_size_is_si_like_baobab() {
        assert_eq!(fmt_size(0), "0 bytes");
        assert_eq!(fmt_size(999), "999 bytes");
        assert_eq!(fmt_size(1000), "1.0 kB");
        assert_eq!(fmt_size(1_500_000), "1.5 MB");
        assert_eq!(fmt_size(999_960), "1.0 MB");
        assert_eq!(fmt_size(u64::MAX), "18.4 EB");
    }
}

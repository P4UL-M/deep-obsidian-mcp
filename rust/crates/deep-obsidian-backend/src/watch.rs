//! Change notification: the vault-watch ignore rules and the stream handle.
//!
//! The ignore rules live here rather than in the server's runtime so there is one
//! definition of "a change worth reindexing". The server's watcher and the
//! backend's [`ChangeStream`] consume the same functions, which is what lets a
//! later slice move the runtime onto [`VaultBackend::changes`](crate::VaultBackend::changes)
//! without any behaviour drift.

use std::path::Path;

use notify::Event;
use tokio::sync::mpsc;

/// Something changed in the vault, or watching itself failed.
///
/// Mirrors the server runtime's internal watch signal one-for-one, so the later
/// migration is a type swap rather than a semantic change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeEvent {
    /// A change worth acting on. `reason` is the human-readable trigger the
    /// runtime records (e.g. `watch:Notes/Home.md`).
    Change(String),
    /// The watcher reported an error.
    Error(String),
}

/// True when a vault-relative path should not trigger a reindex.
///
/// Hidden segments and `node_modules` are always ignored. Beyond that: markdown
/// files always count, other files count only when they look like files (a
/// basename containing a `.`), so directory-level noise is dropped.
pub fn should_ignore_watch_path(relative_path: Option<&str>) -> bool {
    let Some(relative_path) = relative_path else {
        return false;
    };

    let normalized = relative_path.replace('\\', "/");
    let segments = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return false;
    }

    if segments.iter().any(|segment| segment.starts_with('.')) {
        return true;
    }
    if segments.contains(&"node_modules") {
        return true;
    }

    let basename = segments.last().copied().unwrap_or_default();
    if basename.ends_with(".md") {
        return false;
    }

    !basename.contains('.')
}

/// The reason string for a filesystem event, or `None` when every path in it is
/// ignorable.
pub fn watch_reason(vault_path: &Path, event: &Event) -> Option<String> {
    if event.paths.is_empty() {
        return Some("watch:unknown".to_string());
    }

    for path in &event.paths {
        let relative = path
            .strip_prefix(vault_path)
            .ok()
            .map(|value| value.to_string_lossy().replace('\\', "/"));
        if should_ignore_watch_path(relative.as_deref()) {
            continue;
        }
        return Some(match relative {
            Some(value) if !value.is_empty() => format!("watch:{value}"),
            _ => "watch:unknown".to_string(),
        });
    }

    None
}

/// A live subscription to a backend's changes.
///
/// The handle **owns whatever keeps the subscription alive** — for the filesystem
/// backend that is the `notify` watcher, which stops delivering the moment it is
/// dropped. Holding it here means a caller cannot accidentally keep the receiver
/// while letting the watcher die.
///
/// Deliberately an mpsc handle rather than a `Stream`: the only consumer this slice
/// has to serve is the server runtime's `recv()` loop over an unbounded channel.
pub struct ChangeStream {
    receiver: mpsc::UnboundedReceiver<ChangeEvent>,
    /// Kept alive for its `Drop`; never read.
    _subscription: Box<dyn std::any::Any + Send>,
}

impl ChangeStream {
    /// Build a stream from a receiver and the resource that must outlive it.
    pub fn new(
        receiver: mpsc::UnboundedReceiver<ChangeEvent>,
        subscription: impl std::any::Any + Send + 'static,
    ) -> Self {
        Self {
            receiver,
            _subscription: Box::new(subscription),
        }
    }

    /// A stream that yields nothing, for backends that cannot watch.
    ///
    /// The sender is dropped immediately so `recv` reports a closed channel rather
    /// than pending forever — a caller polling a capability-less backend must not
    /// hang.
    pub fn empty() -> Self {
        let (_sender, receiver) = mpsc::unbounded_channel();
        Self::new(receiver, ())
    }

    /// Await the next event. `None` once the subscription has ended.
    pub async fn recv(&mut self) -> Option<ChangeEvent> {
        self.receiver.recv().await
    }
}

impl std::fmt::Debug for ChangeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChangeStream { .. }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_hidden_and_node_modules_segments() {
        assert!(should_ignore_watch_path(Some(".obsidian/workspace.json")));
        assert!(should_ignore_watch_path(Some("Notes/.hidden/file.md")));
        assert!(should_ignore_watch_path(Some("node_modules/pkg/index.js")));
    }

    #[test]
    fn keeps_markdown_and_file_like_paths() {
        assert!(!should_ignore_watch_path(Some("Notes/Home.md")));
        assert!(!should_ignore_watch_path(Some("Assets/logo.png")));
        // A directory-looking basename (no dot) is noise.
        assert!(should_ignore_watch_path(Some("Notes/Subfolder")));
    }

    #[test]
    fn no_relative_path_is_not_ignored() {
        assert!(!should_ignore_watch_path(None));
        assert!(!should_ignore_watch_path(Some("")));
    }

    #[tokio::test]
    async fn empty_stream_terminates() {
        let mut stream = ChangeStream::empty();
        assert_eq!(stream.recv().await, None);
    }
}

//! GOAT-6 — clip connections (FCPX-style) for the magnetic timeline.
//!
//! A *connection* attaches one clip to another so they move together: the
//! `connected` clip "hangs off" the `anchor` clip. When a magnetic move or
//! ripple shifts the anchor by some delta, every connected clip — and the
//! clips connected to THEM, transitively — shifts by the same delta, so a
//! lower-third / B-roll / sound effect stays locked to its parent shot.
//!
//! Connections live on the [`crate::Sequence`] (not the `Clip`, to avoid
//! nested `Arc` churn and cross-track ownership). They form a FOREST: each
//! clip has at most one anchor, and cycles are rejected at edit time.

use serde::{Deserialize, Serialize};

use crate::id::ClipId;

/// One directed attachment: `connected` follows `anchor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// The clip that drives the movement.
    pub anchor: ClipId,
    /// The clip that follows the anchor.
    pub connected: ClipId,
}

impl Connection {
    /// A new attachment of `connected` to `anchor`.
    pub fn new(anchor: ClipId, connected: ClipId) -> Self {
        Connection { anchor, connected }
    }
}

/// The transitive closure of clips reachable from `root` by following
/// `anchor → connected` edges, INCLUDING `root` itself. The returned set
/// is the group that moves together when `root` moves. Robust against a
/// (rejected-but-present) cycle via a visited set.
pub fn connected_subtree(connections: &[Connection], root: ClipId) -> Vec<ClipId> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        for c in connections {
            if c.anchor == cur && !out.contains(&c.connected) {
                out.push(c.connected);
            }
        }
        i += 1;
    }
    out
}

/// Would adding `anchor → connected` create a cycle (or a self-loop) in
/// the existing connection forest? True if `anchor` is already in the
/// subtree rooted at `connected` (so connecting them would close a loop),
/// or `anchor == connected`.
pub fn would_cycle(connections: &[Connection], anchor: ClipId, connected: ClipId) -> bool {
    if anchor == connected {
        return true;
    }
    connected_subtree(connections, connected).contains(&anchor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid() -> ClipId {
        ClipId::new()
    }

    #[test]
    fn subtree_is_transitive_and_includes_root() {
        let (a, b, c) = (cid(), cid(), cid());
        // a → b → c.
        let conns = vec![Connection::new(a, b), Connection::new(b, c)];
        let mut sub = connected_subtree(&conns, a);
        sub.sort();
        let mut expect = vec![a, b, c];
        expect.sort();
        assert_eq!(sub, expect, "a's subtree is {{a,b,c}}");
        assert_eq!(
            connected_subtree(&conns, c),
            vec![c],
            "leaf subtree is itself"
        );
    }

    #[test]
    fn cycle_detection() {
        let (a, b) = (cid(), cid());
        let conns = vec![Connection::new(a, b)];
        assert!(would_cycle(&conns, a, a), "self-loop");
        assert!(would_cycle(&conns, b, a), "b→a closes a→b→a");
        assert!(!would_cycle(&conns, a, cid()), "fresh clip is fine");
    }

    #[test]
    fn subtree_survives_a_cycle_without_looping() {
        let (a, b) = (cid(), cid());
        // Pathological a↔b (edit layer rejects this; be defensive anyway).
        let conns = vec![Connection::new(a, b), Connection::new(b, a)];
        let sub = connected_subtree(&conns, a);
        assert_eq!(sub.len(), 2, "visited set prevents an infinite loop");
    }
}

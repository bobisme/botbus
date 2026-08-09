//! Thread structure derived from [`Message::reply_to`].
//!
//! A message carries one anchor: the id of the message it answers. Everything
//! else — the root, the sibling set, the reply count — is derived by walking
//! those anchors. Nothing is stored twice, so two machines that sync the same
//! channel derive the same threads without agreeing on anything beyond the
//! append-only records themselves.
//!
//! Every walk in this module is total. It terminates and returns a usable
//! answer for any input, including inputs no correct rite writes:
//!
//! | input                                   | result                                          |
//! |-----------------------------------------|-------------------------------------------------|
//! | no anchor                               | the message is its own root                     |
//! | anchor present in the set               | the walk climbs to the topmost ancestor         |
//! | anchor absent (unsynced or tombstoned)  | root is the topmost *present* ancestor, and the missing id is named |
//! | anchor points at the message itself     | the message is its own root, flagged            |
//! | two anchors forming a cycle             | the walk stops at the revisit, flagged          |
//! | a chain longer than [`MAX_THREAD_DEPTH`]| the walk stops at the cap, flagged              |
//!
//! The last four are never rendered as ordinary roots: [`Anchor::kind`] says
//! which case produced the root, so a caller can show a dangling child as
//! dangling instead of silently promoting it.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use ulid::Ulid;

use crate::core::message::Message;

/// Hard ceiling on how far a parent walk climbs.
///
/// The visited set already makes a walk terminate. This is a second bound on
/// *cost*: a pathological chain synced from elsewhere cannot make an
/// interactive command walk an unbounded number of hops.
pub const MAX_THREAD_DEPTH: usize = 256;

/// How a root was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// The message has no anchor. It is a top-level message.
    Root,
    /// The walk climbed to an ancestor that has no anchor.
    Resolved,
    /// The chain ends at an anchor that is not in the set: not yet synced,
    /// deleted, or written by a machine this one has not pulled from.
    MissingParent,
    /// The chain ends at a message that anchors to itself.
    SelfReference,
    /// The chain revisits a message it already passed.
    Cycle,
    /// The chain is longer than [`MAX_THREAD_DEPTH`].
    DepthLimit,
}

impl RootKind {
    /// True when the whole chain up to the root is present and well formed.
    ///
    /// False means the root is the best available answer, not the real one.
    pub fn is_complete(self) -> bool {
        matches!(self, RootKind::Root | RootKind::Resolved)
    }
}

/// The result of walking a message's anchors up to its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// Topmost ancestor reachable in the set. The message itself when it has
    /// no usable anchor.
    pub root: Ulid,
    /// Which case stopped the walk.
    pub kind: RootKind,
    /// Hops taken from the message to [`Anchor::root`].
    pub depth: usize,
    /// The anchor that could not be followed, when `kind` is
    /// [`RootKind::MissingParent`].
    pub missing_parent: Option<Ulid>,
}

/// Walk `start` up to its root inside `by_id`.
///
/// `by_id` is the set of messages the caller can see — normally one channel
/// after deletions are filtered out. A message outside that set is treated as
/// missing, which is exactly right for both an unsynced parent and a tombstoned
/// one.
pub fn resolve_anchor(start: Ulid, by_id: &HashMap<Ulid, &Message>) -> Anchor {
    let mut seen: HashSet<Ulid> = HashSet::new();
    let mut current = start;
    let mut depth = 0usize;

    loop {
        let Some(message) = by_id.get(&current) else {
            // Only reachable on the first hop: every later `current` is checked
            // for membership before the walk advances to it.
            return Anchor {
                root: current,
                kind: RootKind::MissingParent,
                depth,
                missing_parent: Some(current),
            };
        };
        seen.insert(current);

        // `reply_to` is read raw here so a self-edge is reported rather than
        // quietly smoothed over by `Message::parent_id`.
        let Some(parent) = message.reply_to else {
            let kind = if depth == 0 {
                RootKind::Root
            } else {
                RootKind::Resolved
            };
            return Anchor {
                root: current,
                kind,
                depth,
                missing_parent: None,
            };
        };

        if parent == current {
            return Anchor {
                root: current,
                kind: RootKind::SelfReference,
                depth,
                missing_parent: None,
            };
        }

        if seen.contains(&parent) {
            return Anchor {
                root: current,
                kind: RootKind::Cycle,
                depth,
                missing_parent: None,
            };
        }

        if depth >= MAX_THREAD_DEPTH {
            return Anchor {
                root: current,
                kind: RootKind::DepthLimit,
                depth,
                missing_parent: None,
            };
        }

        if !by_id.contains_key(&parent) {
            return Anchor {
                root: current,
                kind: RootKind::MissingParent,
                depth,
                missing_parent: Some(parent),
            };
        }

        current = parent;
        depth += 1;
    }
}

/// One message inside a [`Thread`], with its distance from the root.
#[derive(Debug, Clone)]
pub struct ThreadMessage {
    pub message: Message,
    /// Hops from the thread root. The root itself is 0.
    pub depth: usize,
}

/// A thread: one root and everything that hangs off it.
#[derive(Debug, Clone)]
pub struct Thread {
    /// The message the caller asked about.
    pub anchor: Ulid,
    /// Topmost ancestor reachable from the anchor.
    pub root: Ulid,
    /// Which case produced [`Thread::root`].
    pub kind: RootKind,
    /// An anchor referenced by the root but absent from the set. When this is
    /// set the thread is a fragment, not a whole conversation.
    pub missing_parent: Option<Ulid>,
    /// Root first, then every descendant, chronological by ULID within the
    /// whole thread.
    pub messages: Vec<ThreadMessage>,
}

impl Thread {
    /// Number of messages in the thread, root included.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Always false: a thread contains at least its root.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Collect the thread that `anchor` belongs to.
///
/// Returns `None` when `anchor` is not in `messages`; the caller decides
/// whether that is "wrong channel" or "not synced yet".
///
/// Ordering is by ULID ascending, which is creation order and is stable across
/// machines. Wall-clock `ts` is not used: two machines with skewed clocks would
/// order the same thread differently, and the rest of rite (index rebuild,
/// JSONL append order) already sorts by ULID.
pub fn collect_thread(messages: &[Message], anchor: Ulid) -> Option<Thread> {
    let by_id: HashMap<Ulid, &Message> = messages.iter().map(|m| (m.id, m)).collect();
    if !by_id.contains_key(&anchor) {
        return None;
    }

    let resolved = resolve_anchor(anchor, &by_id);

    // Children map. `parent_id` drops self-edges, so no node is its own child
    // and the descent below cannot spin on one.
    let mut children: HashMap<Ulid, Vec<Ulid>> = HashMap::new();
    for message in messages {
        if let Some(parent) = message.parent_id() {
            children.entry(parent).or_default().push(message.id);
        }
    }

    // Descend from the root. The visited set makes a synced-in cycle finite:
    // an edge back to an ancestor is simply not followed a second time.
    let mut depths: HashMap<Ulid, usize> = HashMap::new();
    let mut visited: HashSet<Ulid> = HashSet::new();
    let mut queue: Vec<(Ulid, usize)> = vec![(resolved.root, 0)];
    visited.insert(resolved.root);

    while let Some((id, depth)) = queue.pop() {
        depths.insert(id, depth);
        if let Some(kids) = children.get(&id) {
            for kid in kids {
                if visited.insert(*kid) {
                    queue.push((*kid, depth + 1));
                }
            }
        }
    }

    let mut collected: Vec<ThreadMessage> = messages
        .iter()
        .filter(|m| visited.contains(&m.id))
        .map(|m| ThreadMessage {
            message: m.clone(),
            depth: depths.get(&m.id).copied().unwrap_or(0),
        })
        .collect();
    collected.sort_by_key(|t| t.message.id);

    Some(Thread {
        anchor,
        root: resolved.root,
        kind: resolved.kind,
        missing_parent: resolved.missing_parent,
        messages: collected,
    })
}

/// Direct replies to `parent`, chronological by ULID.
///
/// This is the primitive behind "has anyone answered me yet": one pass, no
/// walking, no allocation per candidate.
pub fn direct_replies(messages: &[Message], parent: Ulid) -> Vec<&Message> {
    let mut replies: Vec<&Message> = messages
        .iter()
        .filter(|m| m.parent_id() == Some(parent))
        .collect();
    replies.sort_by_key(|m| m.id);
    replies
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Strictly increasing ids.
    ///
    /// `Ulid::new()` is only sortable to the millisecond: two ids minted in the
    /// same millisecond order by their random half, which is arbitrary. Real
    /// replies are not written a microsecond apart, but tests are, so ids that
    /// must compare in a known order are minted here instead.
    fn next_id() -> Ulid {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        Ulid::from_parts(1_700_000_000_000 + n, n as u128)
    }

    fn message(body: &str) -> Message {
        let mut msg = Message::new("agent", "general", body);
        msg.id = next_id();
        msg
    }

    /// A message answering `parent`, with an id that sorts after it.
    fn reply_to(parent: &Message, body: &str) -> Message {
        message(body).with_reply_to(parent.id)
    }

    fn index(messages: &[Message]) -> HashMap<Ulid, &Message> {
        messages.iter().map(|m| (m.id, m)).collect()
    }

    #[test]
    fn top_level_message_is_its_own_root() {
        let root = message("question");
        let messages = vec![root.clone()];

        let anchor = resolve_anchor(root.id, &index(&messages));
        assert_eq!(anchor.root, root.id);
        assert_eq!(anchor.kind, RootKind::Root);
        assert_eq!(anchor.depth, 0);
        assert!(anchor.kind.is_complete());
    }

    #[test]
    fn walk_climbs_to_the_root() {
        let root = message("question");
        let first = reply_to(&root, "answer");
        let second = reply_to(&first, "follow-up");
        let messages = vec![root.clone(), first, second.clone()];

        let anchor = resolve_anchor(second.id, &index(&messages));
        assert_eq!(anchor.root, root.id);
        assert_eq!(anchor.kind, RootKind::Resolved);
        assert_eq!(anchor.depth, 2);
    }

    #[test]
    fn missing_parent_stops_at_the_topmost_present_ancestor() {
        let unsynced = message("never arrived");
        let orphan = reply_to(&unsynced, "answer");
        let child = reply_to(&orphan, "follow-up");
        // `unsynced` is deliberately absent from the set.
        let messages = vec![orphan.clone(), child.clone()];

        let anchor = resolve_anchor(child.id, &index(&messages));
        assert_eq!(
            anchor.root, orphan.id,
            "root is the highest visible message"
        );
        assert_eq!(anchor.kind, RootKind::MissingParent);
        assert_eq!(anchor.missing_parent, Some(unsynced.id));
        assert!(
            !anchor.kind.is_complete(),
            "a fragment must never claim to be a whole thread"
        );
    }

    #[test]
    fn self_reference_is_flagged_and_terminates() {
        let mut looped = message("me");
        looped.reply_to = Some(looped.id);
        let messages = vec![looped.clone()];

        let anchor = resolve_anchor(looped.id, &index(&messages));
        assert_eq!(anchor.root, looped.id);
        assert_eq!(anchor.kind, RootKind::SelfReference);
        assert!(!anchor.kind.is_complete());
        assert_eq!(looped.parent_id(), None, "a self-edge is not a parent");
    }

    #[test]
    fn cycle_terminates() {
        // Two machines each anchor their message to the other's.
        let mut a = message("a");
        let mut b = message("b");
        a.reply_to = Some(b.id);
        b.reply_to = Some(a.id);
        let messages = vec![a.clone(), b.clone()];

        let anchor = resolve_anchor(a.id, &index(&messages));
        assert_eq!(anchor.kind, RootKind::Cycle);
        assert!(!anchor.kind.is_complete());

        // And the collection side terminates too, with both members present.
        let thread = collect_thread(&messages, a.id).expect("anchor is in the set");
        assert_eq!(thread.kind, RootKind::Cycle);
        assert_eq!(thread.len(), 2);
    }

    #[test]
    fn depth_limit_bounds_the_walk() {
        let mut messages = vec![message("root")];
        for i in 0..(MAX_THREAD_DEPTH + 10) {
            let parent = messages.last().unwrap().clone();
            messages.push(reply_to(&parent, &format!("reply {i}")));
        }
        let deepest = messages.last().unwrap().id;

        let anchor = resolve_anchor(deepest, &index(&messages));
        assert_eq!(anchor.kind, RootKind::DepthLimit);
        assert_eq!(anchor.depth, MAX_THREAD_DEPTH);
    }

    #[test]
    fn collect_thread_returns_the_whole_tree_in_ulid_order() {
        let root = message("question");
        let first = reply_to(&root, "answer one");
        let second = reply_to(&root, "answer two");
        let nested = reply_to(&first, "follow-up");
        let unrelated = message("different topic");

        let messages = vec![
            root.clone(),
            first.clone(),
            second.clone(),
            nested.clone(),
            unrelated.clone(),
        ];

        let thread = collect_thread(&messages, nested.id).expect("anchor is in the set");
        assert_eq!(thread.root, root.id);
        assert_eq!(thread.kind, RootKind::Resolved);
        assert_eq!(thread.len(), 4, "the unrelated message must stay out");

        let ids: Vec<Ulid> = thread.messages.iter().map(|t| t.message.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "thread order is ULID ascending");

        let depths: HashMap<Ulid, usize> = thread
            .messages
            .iter()
            .map(|t| (t.message.id, t.depth))
            .collect();
        assert_eq!(depths[&root.id], 0);
        assert_eq!(depths[&first.id], 1);
        assert_eq!(depths[&second.id], 1);
        assert_eq!(depths[&nested.id], 2);
    }

    #[test]
    fn collect_thread_rejects_an_unknown_anchor() {
        let messages = vec![message("only")];
        assert!(collect_thread(&messages, Ulid::new()).is_none());
    }

    #[test]
    fn direct_replies_finds_children_only() {
        let root = message("question");
        let first = reply_to(&root, "answer one");
        let second = reply_to(&root, "answer two");
        let nested = reply_to(&first, "follow-up");
        let messages = vec![root.clone(), first.clone(), second.clone(), nested];

        let replies = direct_replies(&messages, root.id);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].id, first.id);
        assert_eq!(replies[1].id, second.id);

        assert!(direct_replies(&messages, Ulid::new()).is_empty());
    }

    #[test]
    fn a_self_referencing_message_is_never_its_own_reply() {
        let mut looped = message("me");
        looped.reply_to = Some(looped.id);
        let messages = vec![looped.clone()];

        assert!(direct_replies(&messages, looped.id).is_empty());

        // And it collects as a one-message thread rather than looping.
        let thread = collect_thread(&messages, looped.id).unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread.kind, RootKind::SelfReference);
    }
}

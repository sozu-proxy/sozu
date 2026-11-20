//! Newest-first unwinding of the changes the main process committed before any
//! worker answered.
//!
//! The main process applies an order to its own [`ConfigState`] first and
//! fans it out afterwards (`worker_request`, `load_state`,
//! `load_static_config`). When no worker acknowledges the order, the change is
//! reverted with the inverse captured at admission (`should_rollback_fanout`).
//! That inverse is only the exact undo of the order while the order is the
//! newest change to its object: it was captured against the state as it stood
//! when the order arrived, which includes the effect of every order still in
//! flight before it. Two orders on one object that both end unacknowledged and
//! are reverted in the order their deadlines expire, oldest first, undo each
//! other's work. An add and the removal that follows it leave a removal that
//! finds nothing to remove, and then the inverse of the removal puts back an
//! entry that neither client was told exists.
//!
//! The ledger holds one stack per object (see
//! [`sozu_command_lib::state::rollback_key`]), oldest order first. An order
//! that must be reverted is undone as soon as every newer order on its object
//! has a verdict, and the stack is then unwound from the newest end:
//!
//! - a newer order still in flight holds the revert back: its own inverse has
//!   to run first, and may itself be reverted or kept;
//! - a newer order that was kept (at least one worker acknowledged it)
//!   supersedes everything older on that object, so the reverts it holds are
//!   dropped: the workers moved on from the state they would restore;
//! - an object that has no other order in flight is undone at once, as it
//!   always was.
//!
//! Every order that registers here MUST settle, whether it is reverted or
//! kept, or it holds back the reverts of every older order on its object for
//! the life of the process. A task that is dropped without finishing releases
//! its claims through [`RollbackLedger::settle`] with `revert = false`. The
//! ledger is never part of an upgrade snapshot: a task restored by a main
//! process upgrade holds no claim and reverts on its own, as it did before the
//! ledger existed. A revert that was parked in the ledger when the upgrade
//! began, because a newer order on its object was still in flight, is lost with
//! the old process: its task has already ended, so nothing restores it.

use std::collections::HashMap;

use sozu_command_lib::{proto::command::Request, state::rollback_key};

/// The claim of one order on the ledger. Unique for the life of the process,
/// so two orders with equal inverses (add, remove, add) never share one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RollbackTicket(u64);

/// What the fan-out of an order decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// the fan-out is still in flight
    Pending,
    /// at least one worker acknowledged the order, or nothing was owed
    Keep,
    /// no worker acknowledged the order
    Revert,
}

#[derive(Debug)]
struct Owed {
    ticket: RollbackTicket,
    inverse: Request,
    verdict: Verdict,
}

/// An inverse [`RollbackLedger::settle`] released, to be dispatched to the
/// main process state in the order returned.
#[derive(Debug)]
pub(crate) struct Unwind {
    pub(crate) inverse: Request,
    /// `true` when the inverse belongs to an older order than the one that
    /// settled, which had to wait for it
    pub(crate) deferred: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RollbackLedger {
    next_ticket: u64,
    /// per object, oldest order first
    stacks: HashMap<String, Vec<Owed>>,
    /// the object each live ticket is stacked under
    keys: HashMap<RollbackTicket, String>,
}

impl RollbackLedger {
    /// Register an order the main process state has just accepted, with the
    /// inverse that restores the state as it was before the order.
    ///
    /// Call this in the order the orders are applied to the state: the stack
    /// of an object is its history. `None` when the inverse names no object
    /// the ledger stacks; such an order reverts on its own.
    pub(crate) fn owe(&mut self, inverse: Request) -> Option<RollbackTicket> {
        let key = rollback_key(&inverse)?;
        let ticket = RollbackTicket(self.next_ticket);
        self.next_ticket += 1;
        self.keys.insert(ticket, key.clone());
        self.stacks.entry(key).or_default().push(Owed {
            ticket,
            inverse,
            verdict: Verdict::Pending,
        });
        debug_assert_eq!(
            self.keys.len(),
            self.stacks.values().map(Vec::len).sum::<usize>(),
            "every live ticket sits in exactly one stack"
        );
        Some(ticket)
    }

    /// Record the verdict of an order's fan-out and return the inverses that
    /// may now run, newest first. `revert` is `should_rollback_fanout`.
    ///
    /// An unknown ticket (already released) releases nothing.
    pub(crate) fn settle(&mut self, ticket: RollbackTicket, revert: bool) -> Vec<Unwind> {
        let Some(key) = self.keys.get(&ticket).cloned() else {
            debug_assert!(false, "a rollback ticket settles exactly once");
            return Vec::new();
        };
        let Some(stack) = self.stacks.get_mut(&key) else {
            debug_assert!(false, "a live rollback ticket always has its stack");
            return Vec::new();
        };
        let Some(settled) = stack.iter_mut().find(|owed| owed.ticket == ticket) else {
            debug_assert!(false, "a live rollback ticket is always in its stack");
            return Vec::new();
        };
        debug_assert_eq!(
            settled.verdict,
            Verdict::Pending,
            "a rollback ticket settles exactly once"
        );
        settled.verdict = if revert {
            Verdict::Revert
        } else {
            Verdict::Keep
        };

        // Walk from the newest order to the oldest. `blocked`: a newer order
        // is still in flight. `superseded`: a newer order was kept.
        let mut unwind = Vec::new();
        let mut resolved = vec![false; stack.len()];
        let mut blocked = false;
        let mut superseded = false;
        for (index, owed) in stack.iter().enumerate().rev() {
            match owed.verdict {
                Verdict::Pending => blocked = true,
                Verdict::Keep => superseded = true,
                Verdict::Revert if superseded => resolved[index] = true,
                Verdict::Revert if blocked => {}
                Verdict::Revert => {
                    resolved[index] = true;
                    unwind.push(Unwind {
                        inverse: owed.inverse.clone(),
                        deferred: owed.ticket != ticket,
                    });
                }
            }
        }

        // What stays: every order in flight, every revert still waiting for a
        // newer order, and a kept order for as long as an older one is in
        // flight (that one is superseded when it ends).
        let mut older_in_flight = false;
        let mut retained = Vec::with_capacity(stack.len());
        for (index, owed) in stack.drain(..).enumerate() {
            let retain = match owed.verdict {
                Verdict::Pending => {
                    older_in_flight = true;
                    true
                }
                Verdict::Keep => older_in_flight,
                Verdict::Revert => !resolved[index],
            };
            if retain {
                retained.push(owed);
            } else {
                self.keys.remove(&owed.ticket);
            }
        }
        if retained.is_empty() {
            self.stacks.remove(&key);
        } else {
            *stack = retained;
        }
        debug_assert_eq!(
            self.keys.len(),
            self.stacks.values().map(Vec::len).sum::<usize>(),
            "every live ticket sits in exactly one stack"
        );
        // An order that is kept supersedes the older reverts, it never frees
        // one: only a revert can release an inverse.
        debug_assert!(
            revert || unwind.is_empty(),
            "a kept order must not release an inverse"
        );
        unwind
    }

    /// The orders still holding a claim, for the tests.
    #[cfg(test)]
    pub(crate) fn live_tickets(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod tests {
    use sozu_command_lib::proto::command::{
        PathRule, RequestHttpFrontend, RulePosition, SocketAddress, request::RequestType,
    };

    use super::*;

    fn frontend(hostname: &str) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some("cluster_1".to_owned()),
            hostname: hostname.to_owned(),
            path: PathRule::prefix("/".to_owned()),
            address: SocketAddress::new_v4(0, 0, 0, 0, 8080),
            position: RulePosition::Tree.into(),
            ..Default::default()
        }
    }

    /// The inverse of an add: the removal of what it added.
    fn undo_add(hostname: &str) -> Request {
        RequestType::remove_http_frontend(frontend(hostname)).into()
    }

    /// The inverse of a removal: the add of what it removed.
    fn undo_remove(hostname: &str) -> Request {
        RequestType::add_http_frontend(frontend(hostname)).into()
    }

    fn released(unwind: &[Unwind]) -> Vec<&Request> {
        unwind.iter().map(|unwound| &unwound.inverse).collect()
    }

    /// An order alone on its object is undone at once and leaves no claim
    /// behind: the behaviour before the ledger existed.
    ///
    /// TO SEE THIS RED: delete the `unwind.push(Unwind { .. })` statement of
    /// the last `Verdict::Revert` arm of `RollbackLedger::settle`: the
    /// assertion on the released inverse finds `[]`.
    #[test]
    fn an_order_alone_on_its_object_reverts_at_once() {
        let mut ledger = RollbackLedger::default();
        let ticket = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let unwind = ledger.settle(ticket, true);
        assert_eq!(released(&unwind), vec![&undo_add("a.example.com")]);
        assert!(!unwind[0].deferred);
        assert_eq!(ledger.live_tickets(), 0, "a settled order holds no claim");
    }

    /// A kept order releases nothing, and leaves no claim either.
    ///
    /// TO SEE THIS RED: record the verdict as `Verdict::Revert` whatever
    /// `revert` says (`settled.verdict = Verdict::Revert;` in
    /// `RollbackLedger::settle`): the kept order releases its inverse and the
    /// debug assertion `a kept order must not release an inverse` fires.
    #[test]
    fn a_kept_order_releases_nothing() {
        let mut ledger = RollbackLedger::default();
        let ticket = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        assert!(ledger.settle(ticket, false).is_empty());
        assert_eq!(ledger.live_tickets(), 0);
    }

    /// Add, then remove, both unacknowledged and ending oldest first: the
    /// older revert waits for the newer one, and the two run newest first.
    ///
    /// TO SEE THIS RED: delete the `Verdict::Revert if blocked => {}` arm of
    /// `RollbackLedger::settle`: the first settle releases the older inverse
    /// at once and `the add cannot be undone while the removal after it is in
    /// flight` fails.
    #[test]
    fn an_older_revert_waits_for_the_newer_order_on_its_object() {
        let mut ledger = RollbackLedger::default();
        let add = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let remove = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");

        assert!(
            ledger.settle(add, true).is_empty(),
            "the add cannot be undone while the removal after it is in flight"
        );
        let unwind = ledger.settle(remove, true);
        assert_eq!(
            released(&unwind),
            vec![&undo_remove("a.example.com"), &undo_add("a.example.com")],
            "newest first"
        );
        assert_eq!(
            unwind
                .iter()
                .map(|unwound| unwound.deferred)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
        assert_eq!(ledger.live_tickets(), 0);
    }

    /// The mirror direction: remove, then add, ending oldest first.
    ///
    /// TO SEE THIS RED: the same mutation as for the test above, the
    /// `Verdict::Revert if blocked => {}` arm deleted:
    /// `ledger.settle(remove, true).is_empty()` fails.
    #[test]
    fn a_removal_and_the_add_after_it_unwind_newest_first() {
        let mut ledger = RollbackLedger::default();
        let remove = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");
        let add = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");

        assert!(ledger.settle(remove, true).is_empty());
        let unwind = ledger.settle(add, true);
        assert_eq!(
            released(&unwind),
            vec![&undo_add("a.example.com"), &undo_remove("a.example.com")]
        );
    }

    /// Add, remove, add: the first and the third order carry equal inverses,
    /// and each ticket still settles its own entry.
    ///
    /// TO SEE THIS RED: delete `self.next_ticket += 1;` from
    /// `RollbackLedger::owe`, so that every order gets the same ticket: the
    /// debug assertion `every live ticket sits in exactly one stack` fires
    /// (`left: 1, right: 2`).
    #[test]
    fn orders_with_equal_inverses_settle_independently() {
        let mut ledger = RollbackLedger::default();
        let first = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let second = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");
        let third = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        assert_ne!(first, third);

        assert!(ledger.settle(third, false).is_empty());
        assert!(
            ledger.settle(first, true).is_empty(),
            "the third order was kept, the second is still in flight"
        );
        let unwind = ledger.settle(second, true);
        assert!(
            unwind.is_empty(),
            "a kept newer order supersedes both older reverts: {unwind:?}"
        );
        assert_eq!(ledger.live_tickets(), 0);
    }

    /// An order that was kept supersedes what is older on its object.
    ///
    /// TO SEE THIS RED: delete the `Verdict::Revert if superseded =>
    /// resolved[index] = true,` arm of `RollbackLedger::settle`: the older
    /// revert is released underneath the kept order and the debug assertion
    /// `a kept order must not release an inverse` fires.
    #[test]
    fn a_kept_newer_order_drops_the_older_revert() {
        let mut ledger = RollbackLedger::default();
        let add = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let remove = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");

        assert!(ledger.settle(add, true).is_empty());
        assert!(
            ledger.settle(remove, false).is_empty(),
            "a worker acknowledged the removal: restoring the state before the add would undo it"
        );
        assert_eq!(ledger.live_tickets(), 0);
    }

    /// A kept order stays on the stack while an older order is in flight, so
    /// that order is superseded too when it ends unacknowledged.
    ///
    /// TO SEE THIS RED: replace `Verdict::Keep => older_in_flight,` with
    /// `Verdict::Keep => false,` in the retain loop of
    /// `RollbackLedger::settle`: the kept order leaves the stack at once and
    /// `the kept order marks the stack` fails (`left: 1, right: 2`).
    #[test]
    fn a_kept_order_supersedes_an_older_order_that_ends_later() {
        let mut ledger = RollbackLedger::default();
        let older = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let newer = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");

        assert!(ledger.settle(newer, false).is_empty());
        assert_eq!(ledger.live_tickets(), 2, "the kept order marks the stack");
        assert!(ledger.settle(older, true).is_empty());
        assert_eq!(ledger.live_tickets(), 0);
    }

    /// A newer revert runs even while an older order is in flight: the older
    /// order's effect is what the state must go back to.
    ///
    /// TO SEE THIS RED: start the walk of `RollbackLedger::settle` with
    /// `let mut blocked = stack.iter().any(|owed| owed.verdict ==
    /// Verdict::Pending);`: the newer revert waits for the older order and
    /// the first assertion finds `[]` where the inverse of the removal is due.
    #[test]
    fn a_newer_revert_does_not_wait_for_an_older_order() {
        let mut ledger = RollbackLedger::default();
        let older = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let newer = ledger
            .owe(undo_remove("a.example.com"))
            .expect("a keyed inverse");

        let unwind = ledger.settle(newer, true);
        assert_eq!(released(&unwind), vec![&undo_remove("a.example.com")]);
        assert_eq!(ledger.live_tickets(), 1);
        let unwind = ledger.settle(older, true);
        assert_eq!(released(&unwind), vec![&undo_add("a.example.com")]);
    }

    /// Orders on different objects never hold each other back.
    ///
    /// TO SEE THIS RED: key every order the same in `RollbackLedger::owe`
    /// (`let key = rollback_key(&inverse).map(|_| String::new())?;`): the
    /// revert of the first order waits for the second and `an order in flight
    /// on another object does not hold the revert back` fails.
    #[test]
    fn orders_on_different_objects_do_not_wait_for_each_other() {
        let mut ledger = RollbackLedger::default();
        let a = ledger
            .owe(undo_add("a.example.com"))
            .expect("a keyed inverse");
        let _b = ledger
            .owe(undo_add("b.example.com"))
            .expect("a keyed inverse");

        let unwind = ledger.settle(a, true);
        assert_eq!(
            released(&unwind),
            vec![&undo_add("a.example.com")],
            "an order in flight on another object does not hold the revert back"
        );
    }

    /// An inverse that names no object is not stacked: the order reverts on
    /// its own.
    ///
    /// TO SEE THIS RED: replace the `?` of `let key = rollback_key(&inverse)?;`
    /// in `RollbackLedger::owe` with `.unwrap_or_default()`: the inverse of a
    /// `Logging` request gets a ticket and `ledger.owe(..).is_none()` fails.
    #[test]
    fn an_inverse_without_an_object_is_not_stacked() {
        let mut ledger = RollbackLedger::default();
        assert!(
            ledger
                .owe(RequestType::Logging("info".to_owned()).into())
                .is_none()
        );
        assert_eq!(ledger.live_tickets(), 0);
    }
}

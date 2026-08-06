//! The sweep that keeps `workspaces/` honest: a working tree is there iff
//! its chat is open (ADR-0002 rules 1 and 4).

use std::collections::HashSet;
use std::hash::BuildHasher;

/// What one pass over `workspaces/` found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sweep {
    /// Working trees no open chat claims, which are the sweep's to delete.
    pub orphaned: Vec<String>,
    /// Orphans whose container is somehow still up. Deleting one would pull
    /// the floor out from under a running agent, so the sweep says so and
    /// leaves it.
    pub held: Vec<String>,
}

/// Read `workspaces` against the chats that claim one.
///
/// The trees `claimed` names are being built right now and are left alone: a
/// revival writes a working tree before it writes the state that claims one,
/// and a sweep passing through that gap would delete the clone out from under
/// itself.
#[must_use]
pub fn reconcile<S: BuildHasher>(
    workspaces: &[String],
    open: &HashSet<String, S>,
    live: &HashSet<String, S>,
    claimed: &HashSet<String, S>,
) -> Sweep {
    let mut sweep = Sweep::default();
    for orphan in workspaces
        .iter()
        .filter(|dir| !open.contains(*dir) && !claimed.contains(*dir))
    {
        let landing = if live.contains(orphan) {
            &mut sweep.held
        } else {
            &mut sweep.orphaned
        };
        landing.push(orphan.clone());
    }
    sweep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(chat_ids: &[&str]) -> HashSet<String> {
        chat_ids.iter().map(|&id| id.to_owned()).collect()
    }

    fn dirs(chat_ids: &[&str]) -> Vec<String> {
        chat_ids.iter().map(|&id| id.to_owned()).collect()
    }

    #[test]
    fn a_working_tree_no_open_chat_claims_is_swept() {
        let sweep = reconcile(
            &dirs(&["archived", "open"]),
            &ids(&["open"]),
            &ids(&["open"]),
            &ids(&[]),
        );

        assert_eq!(sweep.orphaned, ["archived"]);
        assert!(sweep.held.is_empty());
    }

    #[test]
    fn an_open_chats_working_tree_is_left_where_it_is() {
        let sweep = reconcile(&dirs(&["open"]), &ids(&["open"]), &ids(&[]), &ids(&[]));

        assert_eq!(sweep, Sweep::default());
    }

    /// A revival writes a working tree before it writes the state that claims
    /// one, so a sweep passing through the gap would delete a chat's files out
    /// from under the clone that is still fetching them.
    #[test]
    fn a_working_tree_being_built_right_now_is_nobodys_to_delete() {
        let sweep = reconcile(
            &dirs(&["reviving"]),
            &ids(&[]),
            &ids(&[]),
            &ids(&["reviving"]),
        );

        assert_eq!(sweep, Sweep::default());
    }

    #[test]
    fn an_orphan_whose_container_is_up_is_nobodys_to_delete() {
        let sweep = reconcile(
            &dirs(&["archived"]),
            &ids(&[]),
            &ids(&["archived"]),
            &ids(&[]),
        );

        assert_eq!(sweep.held, ["archived"]);
        assert!(
            sweep.orphaned.is_empty(),
            "the floor was pulled out from under a running agent: {sweep:?}"
        );
    }
}

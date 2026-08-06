//! The warm pool: which chats keep a container and which give one up
//! (ADR-0002 rule 2).

use std::cmp::Reverse;
use std::collections::HashSet;
use std::hash::BuildHasher;

use crate::store::{ChatState, Manifest};

/// The live chats that must give their container up for the pool to hold
/// `cap` of them: the least recently active first out, and every archived
/// chat that somehow still holds one.
///
/// Order among `chats` is not read; `last_active_at` is the whole of the
/// order (ADR-0006).
#[must_use]
pub fn beyond_the_pool<S: BuildHasher>(
    chats: &[Manifest],
    live: &HashSet<String, S>,
    cap: usize,
) -> Vec<String> {
    let mut holders: Vec<&Manifest> = chats
        .iter()
        .filter(|manifest| live.contains(&manifest.chat_id))
        .collect();
    holders.sort_by_key(|manifest| Reverse(manifest.last_active_at));
    let mut kept = 0;
    holders
        .into_iter()
        .filter(|manifest| {
            let keeps = manifest.state == ChatState::Open && kept < cap;
            kept += usize::from(keeps);
            !keeps
        })
        .map(|manifest| manifest.chat_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::{Duration, Utc};

    use crate::store::{ChatState, Manifest, NewChat};

    use super::*;

    /// One open chat, last touched `minutes_ago`.
    fn chat(title: &str, minutes_ago: i64) -> Manifest {
        let mut manifest = Manifest::open(NewChat {
            title: title.to_owned(),
            repo: "CorVous/CorCode".to_owned(),
            branch: format!("chat/{title}"),
            base_branch: "main".to_owned(),
        });
        manifest.last_active_at = Utc::now() - Duration::minutes(minutes_ago);
        manifest
    }

    /// The chats holding a container right now.
    fn live(chats: &[Manifest]) -> HashSet<String> {
        chats
            .iter()
            .map(|manifest| manifest.chat_id.clone())
            .collect()
    }

    #[test]
    fn the_least_recently_active_container_is_the_one_that_goes() {
        let pool = [chat("stale", 60), chat("fresh", 1), chat("middling", 10)];
        let stale = pool[0].chat_id.clone();

        let parking = beyond_the_pool(&pool, &live(&pool), 2);

        assert_eq!(parking, [stale]);
    }

    #[test]
    fn a_pool_inside_its_cap_parks_nothing() {
        let pool = [chat("fresh", 1), chat("stale", 60)];

        assert!(beyond_the_pool(&pool, &live(&pool), 2).is_empty());
    }

    #[test]
    fn a_chat_that_holds_no_container_is_never_parked_again() {
        let pool = [chat("fresh", 1), chat("middling", 10), chat("stale", 60)];

        let parking = beyond_the_pool(&pool, &live(&pool[2..]), 2);

        assert!(
            parking.is_empty(),
            "a pool of one was cut down to nothing: {parking:?}"
        );
    }

    #[test]
    fn an_archived_chat_that_still_holds_a_container_gives_it_up_and_holds_no_slot() {
        let mut pool = [chat("archived", 0), chat("fresh", 1), chat("middling", 10)];
        pool[0].state = ChatState::Archived;
        let archived = pool[0].chat_id.clone();

        let parking = beyond_the_pool(&pool, &live(&pool), 2);

        assert_eq!(parking, [archived]);
    }
}

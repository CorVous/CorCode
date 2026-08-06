//! ADR-0007's reconnect ladder as a rule, apart from anything it is climbed
//! over: which rung comes next, and what the chat's log is told when the climb
//! costs the agent its memory.

/// One rung of the ladder: what to ask the adapter for next (ADR-0007 rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// The adapter's own memory of the session back, with nothing replayed.
    Resume,
    /// The transcript replayed, so the agent remembers by rereading it.
    Load,
    /// A session under a new id, which remembers nothing.
    Fresh,
}

/// Where a climb starts.
pub const FIRST: Rung = Rung::Resume;

/// What climbing one rung came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// The adapter did it.
    Restored,
    /// The adapter answered, and the answer was no: it does not know the
    /// method, or it would not do it. The channel is still there to ask over.
    Refused,
    /// The adapter did not answer at all, so there is nothing left to ask
    /// over.
    Broken,
}

/// What to do once a rung has been climbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Take the turn: the agent remembers this chat.
    Prompt,
    /// Take the turn, but tell the chat first that its agent remembers
    /// nothing above this line (ADR-0007 rule 3).
    PromptWithoutMemory,
    /// Try the next rung over the same connection.
    Climb(Rung),
    /// Nothing here can be resumed (ADR-0007 rule 5).
    GiveUp,
}

/// What `attempt` at `rung` leaves to be done.
#[must_use]
pub const fn after(rung: Rung, attempt: Attempt) -> Step {
    match (rung, attempt) {
        (_, Attempt::Broken) | (Rung::Fresh, Attempt::Refused) => Step::GiveUp,
        (Rung::Resume | Rung::Load, Attempt::Restored) => Step::Prompt,
        (Rung::Fresh, Attempt::Restored) => Step::PromptWithoutMemory,
        (Rung::Resume, Attempt::Refused) => Step::Climb(Rung::Load),
        (Rung::Load, Attempt::Refused) => Step::Climb(Rung::Fresh),
    }
}

/// What a chat is told when its agent came back remembering nothing
/// (ADR-0006's `reset_notice`, ADR-0007 rule 3).
pub const MEMORY_RESET: &str = "Agent memory could not be restored, so the agent starts again here. \
     The log above is the whole record of this chat.";

/// What a chat is told when its workspace came back as a fresh clone
/// (ADR-0002 rule 5).
///
/// A clone standing anywhere but the commit the chat last pushed says so: the
/// remote is the truth, and a commit it no longer carries is gone (ADR-0007
/// rule 4).
#[must_use]
pub fn workspace_reset(branch: &str, standing_at: &str, last_pushed: &str) -> String {
    let drift = if standing_at == last_pushed {
        String::new()
    } else {
        format!(
            " The commit this chat last pushed ({last_pushed}) is not on {branch} any more, \
             so this is the branch tip instead."
        )
    };
    format!(
        "The workspace was reset to a fresh clone at {branch}@{standing_at}.{drift} \
         Anything that was never committed — untracked and gitignored files — is gone."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rung against every outcome: the whole of the ladder, and the
    /// only place it is decided.
    #[test]
    fn the_ladder_climbs_down_one_rung_per_refusal_and_stops_on_a_broken_channel() {
        for (rung, attempt, step) in [
            (Rung::Resume, Attempt::Restored, Step::Prompt),
            (Rung::Resume, Attempt::Refused, Step::Climb(Rung::Load)),
            (Rung::Resume, Attempt::Broken, Step::GiveUp),
            (Rung::Load, Attempt::Restored, Step::Prompt),
            (Rung::Load, Attempt::Refused, Step::Climb(Rung::Fresh)),
            (Rung::Load, Attempt::Broken, Step::GiveUp),
            (Rung::Fresh, Attempt::Restored, Step::PromptWithoutMemory),
            (Rung::Fresh, Attempt::Refused, Step::GiveUp),
            (Rung::Fresh, Attempt::Broken, Step::GiveUp),
        ] {
            assert_eq!(
                after(rung, attempt),
                step,
                "{rung:?} that came to {attempt:?} should lead to {step:?}"
            );
        }
    }

    #[test]
    fn the_climb_starts_at_the_rung_that_costs_the_least() {
        assert_eq!(FIRST, Rung::Resume);
    }

    const BRANCH: &str = "chat/2026-08-05-resume-ladder";
    const PUSHED: &str = "9a1b2c3d4e5f60718293a4b5c6d7e8f901234567";
    const TIP: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_revived_workspace_names_the_branch_and_commit_it_came_back_at() {
        let notice = workspace_reset(BRANCH, PUSHED, PUSHED);

        assert!(
            notice.contains(&format!("{BRANCH}@{PUSHED}")),
            "the notice does not say where the workspace stands: {notice}"
        );
        assert!(
            notice.contains("untracked"),
            "the notice does not say what was lost with the old workspace: {notice}"
        );
    }

    #[test]
    fn a_workspace_that_could_not_come_back_at_the_pushed_commit_says_which_one_it_did() {
        let notice = workspace_reset(BRANCH, TIP, PUSHED);

        assert!(
            notice.contains(&format!("{BRANCH}@{TIP}")),
            "the notice does not say where the workspace stands: {notice}"
        );
        assert!(
            notice.contains(PUSHED) && notice.contains("tip"),
            "the notice hides that the commit this chat pushed is gone: {notice}"
        );
    }
}

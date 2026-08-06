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
    panic!("the ladder is not written yet")
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
}

//! The lines this process logged, kept where a test can read the level each
//! one went out at.
//!
//! A log line's level is what decides whether a deployment ever sees it: the
//! core defaults to WARN, so a site quietly demoted to `debug!` goes silent in
//! production while every test of its wording stays green (issue #84). The
//! wording is a pure builder's to pin; the level is only observable from the
//! outside, through the logger itself.
//!
//! A process has one logger and it is set once, so this capture is a static
//! one that every test in a binary shares. Nothing is drained: a site logs its
//! own words, so tests read past each other's lines rather than racing over
//! them, and none of them need serialising.

use std::sync::{Mutex, Once};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Every line logged since the capture was installed, with its level.
pub struct LoggedLines {
    lines: Mutex<Vec<(Level, String)>>,
}

impl LoggedLines {
    const fn new() -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
        }
    }

    /// The quietest level anything saying `saying` went out at, or nothing
    /// where no line said it. The quietest, because a pin asks whether the
    /// site is loud enough for a deployment to hear: one demoted line is a
    /// silent site, whatever some louder line saying the same thing does.
    #[must_use]
    pub fn quietest_level_of(&self, saying: &str) -> Option<Level> {
        self.held()
            .iter()
            .filter(|(_, line)| line.contains(saying))
            .map(|(level, _)| *level)
            .max()
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<(Level, String)>> {
        self.lines.lock().expect("no holder of the lock panics")
    }
}

impl Log for LoggedLines {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        self.held()
            .push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

static CAPTURED: LoggedLines = LoggedLines::new();
static INSTALLED: Once = Once::new();

/// The lines this process logs, capturing them from the first call onwards.
pub fn capturing_lines() -> &'static LoggedLines {
    INSTALLED.call_once(|| {
        log::set_logger(&CAPTURED).expect("nothing else logs in a process that captures");
        log::set_max_level(LevelFilter::Trace);
    });
    &CAPTURED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_read_back_at_the_level_it_was_logged_at() {
        let capture = capturing_lines();

        log::warn!("the capture reads this one back");

        assert_eq!(
            capture.quietest_level_of("the capture reads this one back"),
            Some(Level::Warn)
        );
    }

    /// A pin reads whichever line is quietest, so a site demoted behind a
    /// louder line that says the same thing is still caught.
    #[test]
    fn a_demoted_line_is_not_masked_by_a_louder_one_saying_the_same_thing() {
        let capture = capturing_lines();

        log::warn!("two sites say this one");
        log::debug!("two sites say this one");

        assert_eq!(
            capture.quietest_level_of("two sites say this one"),
            Some(Level::Debug)
        );
    }

    /// A pin on wording reads the line itself: what a site said is as much
    /// the pin's business as how loud it said it.
    #[test]
    fn a_line_is_read_back_whole_so_a_caller_can_take_what_it_said_out_of_it() {
        let capture = capturing_lines();

        log::info!("session 41 is in permission mode acceptEdits");

        assert_eq!(
            capture.lines_saying("session 41"),
            vec![(
                Level::Info,
                "session 41 is in permission mode acceptEdits".to_owned()
            )]
        );
    }

    #[test]
    fn a_line_nobody_logged_has_no_level_at_all() {
        assert_eq!(
            capturing_lines().quietest_level_of("nothing says this"),
            None
        );
    }
}

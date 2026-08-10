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

    /// The level of the first line that says `saying`, or nothing where no
    /// line said it.
    #[must_use]
    pub fn level_of(&self, saying: &str) -> Option<Level> {
        self.held()
            .iter()
            .find(|(_, line)| line.contains(saying))
            .map(|(level, _)| *level)
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
pub fn logged_lines() -> &'static LoggedLines {
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
        let logged = logged_lines();

        log::warn!("the capture reads this one back");

        assert_eq!(
            logged.level_of("the capture reads this one back"),
            Some(Level::Warn)
        );
    }

    #[test]
    fn a_line_nobody_logged_has_no_level_at_all() {
        assert_eq!(logged_lines().level_of("nothing says this"), None);
    }
}

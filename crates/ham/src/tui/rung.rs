//! Which questions have rung the bell. A question rings once, whichever screen shows it first: the Runs list or the run's own screen.
//! The list keeps its alerts by run (`ham_core::alert::Alerts`), and the run screen used to keep the questions it rang for by request id, so
//! the two could not know of each other: a question that rang on the list rang again when the run was opened, and one that rang on the run's
//! screen rang again when the person went back to the list. [`Rung`] is the one record both ask. It keys a question by the run and the
//! request id, so a repeated id never silences another run.
//!
//! Neither record is forgotten while the process lives, and each keeps the newest [`KEEP`] questions. The list's own repeat rule is not
//! here: `Alerts` decides when a run begins to wait (a run that is answered and waits again, or that asks again with a new request id, rings
//! again on the list); [`Rung::list_rings`] only asks whether the run's screen has rung for that question already.

use std::collections::VecDeque;

/// How many questions each record keeps.
const KEEP: usize = 64;

/// A question: the run and the request id.
type Question = (String, String);

/// Appends `item`, dropping the oldest when the record is full.
fn remember(items: &mut VecDeque<Question>, item: Question) {
    items.push_back(item);
    while items.len() > KEEP {
        items.pop_front();
    }
}

fn holds(items: &VecDeque<Question>, run: &str, request: &str) -> bool {
    items.iter().any(|(u, r)| u == run && r == request)
}

#[derive(Debug, Default)]
pub(crate) struct Rung {
    /// The questions that rang on a run's own screen, or that its screen found to have rung on the list.
    screen: VecDeque<Question>,
    /// The questions that rang on the Runs list.
    list: VecDeque<Question>,
}

impl Rung {
    /// The run's screen shows the open question `request` of `run`: whether the bell rings. It does unless the question rang before, on the
    /// screen or on the list. Either way the screen remembers it.
    pub(crate) fn screen_rings(&mut self, run: &str, request: &str) -> bool {
        if holds(&self.screen, run, request) {
            return false;
        }
        remember(&mut self.screen, (run.to_string(), request.to_string()));
        !holds(&self.list, run, request)
    }

    /// The Runs list reads `run` as newly waiting for `request`: whether the alert goes out. It does not when the question rang on the run's
    /// own screen. Otherwise the list remembers it, so the screen stays quiet when the run is opened.
    pub(crate) fn list_rings(&mut self, run: &str, request: &str) -> bool {
        if holds(&self.screen, run, request) {
            return false;
        }
        if !holds(&self.list, run, request) {
            remember(&mut self.list, (run.to_string(), request.to_string()));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_rings_once_on_the_screen() {
        let mut r = Rung::default();
        assert!(r.screen_rings("run", "q1"));
        assert!(!r.screen_rings("run", "q1"));
        assert!(r.screen_rings("run", "q2"));
    }

    #[test]
    fn a_question_that_rang_on_the_list_does_not_ring_on_the_screen() {
        let mut r = Rung::default();
        assert!(r.list_rings("run", "q1"));
        assert!(!r.screen_rings("run", "q1"));
        // The screen remembers it, and another question still rings there.
        assert!(!r.screen_rings("run", "q1"));
        assert!(r.screen_rings("run", "q2"));
    }

    #[test]
    fn a_question_that_rang_on_the_screen_does_not_ring_on_the_list() {
        let mut r = Rung::default();
        assert!(r.screen_rings("run", "q1"));
        assert!(!r.list_rings("run", "q1"));
        assert!(!r.list_rings("run", "q1"));
    }

    #[test]
    fn the_list_leaves_its_own_repeat_rule_to_alerts() {
        // `Alerts` says when a run begins to wait; asked again for the same question the record does not hold the list back.
        let mut r = Rung::default();
        assert!(r.list_rings("run", "q1"));
        assert!(r.list_rings("run", "q1"));
    }

    #[test]
    fn a_question_is_kept_per_run() {
        let mut r = Rung::default();
        assert!(r.list_rings("a", "q1"));
        assert!(r.screen_rings("b", "q1"));
        assert!(r.list_rings("c", "q1"));
        assert!(!r.screen_rings("a", "q1"));
    }

    #[test]
    fn each_record_keeps_the_newest_questions_only() {
        let mut r = Rung::default();
        for n in 0..=KEEP {
            assert!(r.screen_rings("run", &format!("q{n}")));
        }
        // q0 fell out of the record, so it rings again; the newest is still known.
        assert!(r.screen_rings("run", "q0"));
        assert!(!r.screen_rings("run", &format!("q{KEEP}")));
        for n in 0..=KEEP {
            assert!(r.list_rings("other", &format!("l{n}")));
        }
        assert_eq!(r.list.len(), KEEP);
    }
}

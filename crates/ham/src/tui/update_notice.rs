//! The update notice. A child module of `app`.
//!
//! The terminal app looks at the update source for a newer release and shows a notice beside the version tag when `ham update` has
//! something to install. The notice only indicates the release (`v5.25 available`). A click on it opens the page of the update source in
//! the browser, through the opener the app uses for every link, and the app stays open; the update itself is `ham update` typed in a
//! shell. The notice has no key.
//!
//! - **The look.** [`Effect::LookForUpdate`] is one git call on a blocking thread (`crate::update::Git::look`), answered by
//!   [`Msg::UpdateLook`]. `ham_core::update::LookSchedule` says when: at the start, a day after a look that ended, an hour after one that
//!   failed, and after the Settings screen wrote `update_source` or `ui.update_notice`. At most one look is in flight, and the result of
//!   a look of earlier settings is dropped. Per tick the app compares two numbers; no key and no draw waits for a look. The app is told
//!   the git to run by [`App::set_update_look`]; without it (every app a test builds) there is no look and no notice. Nothing of it is
//!   stored outside the process.
//! - **The notice.** It is computed at every draw from the client's version, the backend's version and the newest version the last look
//!   named (`ham_core::update::notice_for`), so a backend version that arrives or changes shows without another look.
//! - **The click.** [`App::update_click`]: with a web page for the source the last look read (`ham_core::update::web_address`), the page
//!   is handed to the opener ([`Effect::OpenUpdatePage`]) and its answer ([`Msg::UpdatePage`]) says one sentence: the address and how
//!   to update, or why the page could not be opened and how to update. A source without a web page (a local path) opens nothing and
//!   the sentence names the release and how to update.
//! - **The sentence.** It is the same on every screen and over both overlays: `App::draw` gives it rows of its own under the screen,
//!   wrapped to the width ([`App::update_said`]), so it is whole on a terminal of 80 columns and hides nothing. It stays until the next
//!   key, press of a mouse button or paste ([`App::update_said_ends`]). It is no notice or error of the screen it shows on.

use super::*;
use crate::update::LookOutcome;
use ham_core::update::{LookSchedule, UpdateNotice};
use std::path::PathBuf;

/// What the app keeps for the update notice. Nothing of it is stored outside the process.
#[derive(Debug, Default)]
pub(crate) struct UpdateState {
    /// The program the look runs as git; `None` for no look and no notice.
    git: Option<PathBuf>,
    schedule: LookSchedule,
    /// The newest release the last look that applied named; `None` before the first one ended, after a look that found none or was
    /// switched off, and after a Settings write.
    newest: Option<String>,
    /// The web page of the update source the last look read; `None` for a source that has none.
    page: Option<String>,
    /// The sentence of the last click on the notice, while it shows: until the next key, press of a mouse button or paste.
    said: Option<String>,
}

impl App {
    /// Tells the app which git its look for a newer release runs (`crate::update::look_program`); `None` for no look and no notice.
    pub fn set_update_look(&mut self, program: Option<PathBuf>) {
        self.update.git = program;
    }

    /// The look that is due when the app starts, for the first effects of the session: the first look does not wait for the first tick.
    pub fn update_look_first(&mut self) -> Vec<Effect> {
        self.look_if_due()
    }

    /// Starts a look when the schedule says one is due. The wall clock is 0 until the loop's first draw; the first look needs no clock.
    pub(super) fn look_if_due(&mut self) -> Vec<Effect> {
        let now = self.now.unwrap_or(0);
        let Some(program) = self.update.git.as_ref() else {
            return vec![];
        };
        if !self.update.schedule.due(now) {
            return vec![];
        }
        let program = program.clone();
        let generation = self.update.schedule.started();
        vec![Effect::LookForUpdate {
            generation,
            program,
        }]
    }

    /// The answer of a look: what it found and the update source it read (`None` when no look was made).
    pub(super) fn update_looked(
        &mut self,
        generation: u64,
        outcome: LookOutcome,
        source: Option<String>,
    ) -> Vec<Effect> {
        let failed = outcome == LookOutcome::Failed;
        if !self
            .update
            .schedule
            .ended(generation, failed, self.now.unwrap_or(0))
        {
            // A look of earlier settings: the next tick starts the look the Settings write asked for.
            return vec![];
        }
        match outcome {
            LookOutcome::Newest(version) => {
                self.update.newest = Some(version);
                self.update.page = source.as_deref().and_then(ham_core::update::web_address);
            }
            LookOutcome::Off | LookOutcome::NoRelease => {
                self.update.newest = None;
                self.update.page = None;
            }
            // A look that failed keeps what the last look found.
            LookOutcome::Failed => {}
        }
        vec![]
    }

    /// The Settings screen wrote `update_source` or `ui.update_notice`: the notice goes at once, and a look starts as soon as none is in
    /// flight. A look that finds the notice switched off answers `Off` and starts no git.
    pub(super) fn update_settings_changed(&mut self) -> Vec<Effect> {
        self.update.schedule.reset();
        self.update.newest = None;
        self.update.page = None;
        self.look_if_due()
    }

    /// The notice that is due, or `None`: while the app has no git for a look, before the first look ended, and whenever the newest
    /// release the last look named is nothing `ham update` would install. Per draw it plans on three short strings.
    pub(crate) fn update_notice(&self) -> Option<UpdateNotice> {
        self.update.git.as_ref()?;
        ham_core::update::notice_for(
            crate::version::CLIENT,
            crate::version::backend().as_deref(),
            self.update.newest.as_deref(),
        )
    }

    /// The words of the notice for this draw: the long and the short text.
    pub(crate) fn update_notice_words(&self) -> Option<crate::tui::version_tag::NoticeWords> {
        let notice = self.update_notice()?;
        let (long, short) = ham_core::update::notice_words(&notice);
        Some(crate::tui::version_tag::NoticeWords { long, short })
    }

    /// A click on the notice (the button went down and came up on it), on every screen. The page of the update source is handed to the
    /// opener and the app stays open; a source without a web page opens nothing, and the sentence says what is available and how to
    /// update.
    pub(super) fn update_click(&mut self) -> Vec<Effect> {
        let Some(notice) = self.update_notice() else {
            return vec![];
        };
        match self.update.page.clone() {
            Some(url) => vec![Effect::OpenUpdatePage {
                program: crate::open::program(self.open_command.as_deref()),
                url,
            }],
            None => {
                self.update.said = Some(ham_core::update::no_page_words(&notice.version));
                vec![]
            }
        }
    }

    /// The answer of the opener to a click on the notice: the sentence of the click.
    pub(super) fn update_page_opened(
        &mut self,
        url: &str,
        result: Result<(), String>,
    ) -> Vec<Effect> {
        self.update.said = Some(match result {
            Ok(()) => ham_core::update::page_opened_words(url),
            Err(e) => ham_core::update::page_failed_words(&open_failed_words(&e)),
        });
        vec![]
    }

    /// The sentence of the last click on the notice, while it shows. It goes through no screen's own line (`App::say`): the draw gives
    /// it rows of its own under whatever is up, so it is whole and in the same place on every screen and over both overlays.
    pub(crate) fn update_said(&self) -> Option<&str> {
        self.update.said.as_deref()
    }

    /// A key, a press of a mouse button or a paste: the sentence of the last click on the notice has been read.
    pub(super) fn update_said_ends(&mut self) {
        self.update.said = None;
    }
}

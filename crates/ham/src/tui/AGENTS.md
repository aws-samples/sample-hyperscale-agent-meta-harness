# crates/ham/src/tui

## Purpose

The Ratatui TUI of `ham`. `app.rs` holds the screens and turns keys and API answers into effects, `ui.rs` draws them, and
`mod.rs` owns the terminal session and the event loop. The chat client adds one module per concern, so that features
can be built in parallel without editing the three shared files. This file lists those modules, the names other modules may rely on, and
the rules every change here follows.

## Ownership

Owned by `crates/ham/AGENTS.md`. The three shared files (`app.rs`, `ui.rs`, `mod.rs`) have one owner at a time: the TUI-core owner.
A module branch changes only its own module, its own tests and its own section below; the TUI-core owner wires modules into the shared
files. `Cargo.toml` and `Cargo.lock` change only through the dependency-freeze issue, because both files are part of the worker
image key (`scripts/image-key-guard.sh`).

## Local Contracts

- Modules (each file starts as doc comments only; the issue that fills it is named):
  - `session.rs`: `Modes`, `write_start`, `write_enter`, `write_restore` (pure, over `impl Write`), `wants_mouse`, `TerminalSession`
    (enter, restore, Drop), `wait_for_signal`, `forward_continue` (SIGCONT as `Event::FocusGained`, below), `write_mouse` and `set_mouse` (`/mouse on|off`: the three mouse modes on or off for the running session, the recorded mode set following), `selection_hint` (the one-line hint on selecting text natively: Fn in Terminal.app, Option in iTerm2, Shift elsewhere) and `mouse_forced_off` (`HAMH_NO_MOUSE`). It owns every terminal mode the TUI turns on. Mouse reports are on by default.
    Debug builds only: `HAMH_TEST_PANIC_AFTER_KEYS=<n>` panics at the nth key so the PTY
    tests can prove that a panic restores the terminal.
  - `input.rs`: `ChatBox`, below. The only file that names `ratatui_textarea`.
  - `keys.rs`: `Mode` (typing, browse), `Action`, the key table `TABLE`, `lookup(mode, key)`, `alt_as_browse(key)`, `opens_anything(key)`, `typing_footer`, `EDITOR_KEYS_OVERRIDDEN` and `EDITOR_ALT_LETTERS`. No import of `App`. The typing footer of a run that waits for nobody names the hotkeys that follow Esc: `Esc browse, then o open PR, d download, a artifacts, c c cancel` (the first two once a link exists) when the width allows, else `Esc browse, then c c cancel`, else `Esc browse (c c cancel)`, else `typing_footer` unchanged; the hotkeys share the one part of the line that the Esc chip covers, so no chip for a letter types it into the box. A blocked run has ended and has nothing to cancel, so its wording leaves out `c c cancel` (`Esc browse, then o open PR, d download, a artifacts`, else `Esc browse`) and `send_footer` ends the line with `Esc, then r retry`. A first `c` in the box is a letter. The message keys have rows
    with no footer label: `w` (`Action::Withdraw`), `x` (`Action::ConfirmHeld`) and `h` (`Action::ChooseHeld`) in browse mode, and Ctrl-O (`Action::OpenAnything`) in both modes.
  - `burst.rs`: `Burst`, a pure state machine over injected instants (`key`, `paste`, `preview_drawn`, `confirm`) and the timings
    `KEY_GAP` (30 ms), `AFTER_BURST` (150 ms) and `AFTER_PREVIEW` (300 ms).
  - `rung.rs`: `Rung`, the one record of which questions have rung the bell, asked by
    the run screen (`track_input`: `screen_rings(run, request)`) and by the Runs list (`alert_effects`: `list_rings(run, request)`). A question (a run and a request id)
    rings once, whichever screen shows it first. Neither record is forgotten while the process lives; each keeps the newest 64 questions. The list's own repeat rule is
    not in it: `Alerts` decides when a run begins to wait. A private module with no import of `App`.
  - `commands.rs`: the parts of the message box's command flow that hold no `App`: `Armed`,
    `Panel` (the
    result panel: sanitized lines, scroll (PgUp and PgDn in both modes, the arrow keys in browse mode only: `App::typing_key` hands the page keys to `panel_key` when the panel overflows, and the border names only the keys that work), the exit code in the title, a `Bearer` link kept apart and never drawn or printed), `config_revision`.
  - `handoff.rs`: `run_child_with(program, argv, dir, run)`, the hand-over of the terminal to a command typed in the chat box (`push`, `rules edit`, `continue`, with or without a prompt). After the second
    Enter the app emits `Effect::RunTerminal { argv }`; `run_edits` in `mod.rs` runs it on the loop's thread as a child `ham` with the same arguments (`std::env::current_exe`), through `reader::suspend_for`
    (the reader parks, the TUI modes go off, git's and ssh's prompts and the editor reach the person, the modes come back), clears the screen (`clear_screen`) and answers `Msg::CommandDone`. The child's
    standard output and error go to one file opened for appending in a private 0700 directory under the temp directory (removed when the call ends), so the panel shows them in the order written; the last
    256 KiB are kept, cut at a line start, behind the line `(earlier output was cut)`. A child that cannot start, a file that cannot be made and a child ended by a signal give exit code 1 and a line that
    says why. The editor of such a child draws on the controlling terminal (`crate::tty`).
  - `reader.rs`: the key-reader thread and `suspend_for`. Nothing else reads the terminal or hands the tty to a child.
  - `chat.rs` and `transcript.rs`: the run chat view and its transcript widget.
  - `chat_search.rs`: search inside one chat, joined to the transcript (`ChatSearchUi`, `SearchBar`, `mark_of`); the matcher and the state
    machine are `ham_core::search`.
  - `mouse.rs`, `select.rs` and `copy.rs`, `markdown.rs` and `export.rs`: the pure parts of the mouse, text selection, the clipboard, Markdown and diff
    rendering and the Markdown export, each with the names the wiring may rely on in the sections below.
  - The engine rows of `settings.rs`: between the run rows and the interface rows the screen draws a group named by `engines::settings_group_label` (`Cline engine`) with one row for each key of the engine's table, in the table's order, with the words of `engines::setting_texts` and the origin `file` or `default`. Enter writes the next level of a choice (the levels of the key table: for `reasoning.effort` default, off, low, medium, high, xhigh and round again), the next subset of a list of tools (none, the first, the second, both), or opens a one-line box for a number (`BoxTarget::Engine`); `u` removes the key; `e` on an engine row hands the terminal to `ham engine <name> edit`. Each change is planned on the snapshot's text by `engine_cmd::plan_set` or `plan_unset` (the patcher and the shared check): a value the check refuses stays in the box with the first line of its words, a no-op writes nothing, and a write goes through `Job::ApplyEngine` to `engine_cmd::apply_set` or `apply_unset` (a SHA-256 compare before the rename). The engine file is read-only on the screen when it cannot be read as TOML or is written for a newer `ham` (only `e` and Esc work); a refused key shows as a finding, and the rows still show the values the file holds. The writes of the engine rows never touch `config.toml`. The "Locked by the harness" texts are the protocol's table (`settings::LOCKED` is built from `harness_protocol::direct::DENIED`).
  - `ext.rs`: `Job` (the request of a chat-client feature: a page of the stored conversation, the run's messages, a send, a withdrawal, the search across runs, the delivered head of a finished run, the follow-up's look and start, and `EngineSettings`: the settings hash of the engine file as it is now, one file read and no network),
    `Done` (its answer), `SendFailure` (the server's status, code and words, or status 0 for no answer) and `perform`. The app asks with one effect, `Effect::Ext(Job)`, and
    reads one message, `Msg::Ext(Done)`; nothing here keeps state.
  - `history.rs`, `earlier.rs`, `send.rs`, `openbox.rs`, `engine_ui.rs` and `tags.rs` are **children of `app`** (`#[path = "..."] mod ...;` in `app.rs`), so they use its private state; `app.rs` only calls
    them. Their state sits in six fields of `App` (`hist`, `earlier`, `send`, `find`, `caps`, `tags`). The hooks in the shared files: `open_run` (`hist_open`, `send_open`, `earlier_feed`), `tick` (`hist_tick`,
    `earlier_tick`, `send_tick`, `openbox_tick`), `set_runs` (`earlier_feed`), `send_box` (`earlier_blocks_send`), `apply_recall` (`earlier_wait`), `refresh` (`caps_refresh`, `send_context_changed`), `rebuild_chat` (`hist_model`, which calls `send_with_user_items` for a conversation without a stored store), `scroll_chat` (`hist_scrolled`), `key_at` (`ext_key`, before the
    screen's own keys; Ctrl-O is found through `keys::opens_anything`), `answer_with` (`send_enter`, `send_answer_started`), `answered` (`send_answer_done`), `browse_action` (`send_withdraw_action`, `send_confirm_action`, `send_choose_action`, `openbox_open`), `route_line` (`forward_engine_command`), `ui_word` (`open`, `engines`, `sessions`, `now`), `popup_entries` (`popup_for`), the
    update of `Msg::Ext` (`ext_done`), and in `ui.rs` the box title (`send_title`), the pending rows (`pending_rows`), the footer (`send_footer`), the header's state (`state_with_caps`) and the overlay
    (`draw_openbox`).
  - `mascot.rs` (the ham mascot; tests: the unit tests of the module, `crates/ham/tests/mascot_art.rs`, `tui_mascot_screen.rs`, `mascot_svg.rs` and `mascot_words.rs`): the small orange character of the terminal app, an original pixel drawing with no logo or text:
    two big eyes that look up, a dark shirt with small arms, light shorts, no mouth, and, in the large form and in the head of a tall terminal, a short rocket flame with grey smoke below it. Pure, no `App`: `Form` (`Head`, 8 columns by 5 rows, `HEAD_MAX = (5, 8)`: the head and shirt, a flame under the legs and a puff of smoke; `HeadShort`, 8 by 3, `HEAD_SHORT_MAX = (3, 8)`: the character alone, for the 31 and 32-row band; `Large`, 14 by 12, `LARGE_MAX = (12, 14)`), `Look` (`Blocks`: only space and `▀ ▄ █`, each one cell wide, in twelve colours;
    `Ascii`: plain ASCII with no style, the same size and the same cells; the eyes are `@`, and no look draws a letter or a digit), `FRAMES` (3: the large form and the head have three frames, frame 0 is the still picture of the README; the character and the flames never change and only the smoke moves, four to six pixels by one cell from one frame to the next, the first three smoke rows next to the flame stay and the two plumes never touch; the head's smoke is two pixel rows, the first stays and the last changes four to eight places from one frame to the next, with the plumes apart in columns 3 and 4; the ASCII look moves the same smoke through a density ramp; `HeadShort` has one frame; a frame number past the last wraps), `lines_at(form, look, frame)`, `plain_at`, `lines` and `plain` (frame 0), `size`, `look_from_env` (`NO_COLOR` set and not empty, `TERM=dumb`, or a first non-empty `LC_ALL`, `LC_CTYPE` or `LANG` that does not
    name UTF-8, or none set, gives `Ascii`), (the look reaches a draw as a value of the app, `Head.look`; `cli::head_from` reads it from the environment it is given, no draw function reads the process), `form_for` (a room of 18 columns by 14 rows takes `Large`, 12 by 7 takes `Head`, 12 by 5 takes `HeadShort`, less takes none: the drawing plus 2 columns and 1 row of margin), `room` (the
    Runs body inside its border and below its header row), `placement` (centred in the room), `draw_with(f, body, look, frame)`, `draw_runs_empty(f, body, empty, look, frame)` and `draw_head(f, at, form, look, frame)` (the head whole or not at all). The large form is written as a grid of pixels (one letter per pixel: `BODY`, `FLAME` and the three `SMOKE` frames) and drawn two pixel rows to a terminal row; the character's three rows (`HEAD_SHORT`) are written as glyph rows and one tone letter per glyph (lower case: one
    colour; upper case: a `▀` with a foreground over a background), and the head's flame and smoke (`HEAD_FLAME`: `FYF..FYF` over `.R....R.`, an orange edge each side of a yellow core with a red tip under it; `HEAD_SMOKE`: three frames of two pixel rows) are pixel rows drawn two to a terminal row like the large form. `mascot_art.rs` decodes what `lines_at` draws back to the pixel grids the art was designed on, so a glyph, a letter or a colour that moves one pixel fails there, and it pins what moves between frames (only smoke pixels, 4 to 8 of them by one cell, never the first three smoke rows, and the two plumes never touch; for the head only the last pixel row, 4 to 8 places a step, and a different glyph row in every frame in both looks). The colours are `PALETTE`, twelve xterm-256 indexes with their hex:
    orange 208 `#ff8700`, eye white 231, ink 16, shirt 59 `#5f5f5f`, shorts 137 `#af875f`, flame yellow 220 `#ffd700`, flame orange 202 `#ff5f00`, flame red 160 `#d70000`, smoke 242 `#6c6c6c`, 245 `#8a8a8a`, 249 `#b2b2b2` and 252 `#d0d0d0`; no theme colour, no 24-bit colour. The orange is the one nearest the picture's orange (CIE76 distance 9.3, against 27.3 for xterm 202) that still reads: the tests compute 3:1 or better on
    black and 2:1 or better on white for the orange (8.7 and 2.4), 3:1 on both for the shirt and the shorts, 2:1 for the eye white and 4.5:1 for the ink against the orange they sit in, 3:1 on both for the flame's orange (6.9 and 3.0) and red (3.9 and 5.4) and for the two darker greys of the smoke (4.0 and 5.2, 6.1 and 3.5), 3:1 on black for the two lighter greys with 2:1 (9.9 and 2.1) and 1.5:1 (13.6 and 1.5) on white, and 2:1 for the flame's yellow core against the orange edge it sits in (2.2); changing a colour is a decision, and `the_orange_is_the_one_nearest_the_picture_that_still_reads` says so.
    `docs/images/ham-mascot.svg` is frame 0 of the large form for the README, and `mascot_svg.rs` rasterises its paths and compares them with it pixel for pixel. The frame comes from the app: `App::mascot_frame()` is 0 when `ui.mascot_motion` is off, else the ticks of the app's timer (`App::tick`, one second; `mascot_tick` counts one on every screen while `Head.band` is on, so the smoke of the band and of the picture are always in the same frame, and without the band counts one while the Runs screen shows the picture, with no run, no error and `ui.mascot` not off, and resets to 0 otherwise, so the picture starts at frame 0 whenever it appears) divided by `MASCOT_TICKS_PER_FRAME` (1) modulo `FRAMES`. No new timer and no redraw is added: the event loop redraws on every tick already, and the draw reads the frame, never the clock (`tui_mascot_screen.rs` drives the ticks). Where it shows: `draw_runs` in `ui.rs` makes the one call `draw_runs_empty(f, body, Head.empty_table && App::runs_answered && no run && no error,
    Head.look, App::mascot_frame())`, so the large form (the head with its flame and smoke, then the short head, when the table has no room for it) shows in the empty table of the Runs screen after the service has answered a read of the list (`App::runs_answered`, set by `set_runs`, so it never shows while the list is still loading), never on another
    screen, never as a splash, and in both modes of the title band (it is a function of the table body alone). It writes only inside the empty table, below the header row, so no text row moves: `tui_mascot_screen.rs` compares every other cell with the screen before the answer at six sizes, and with the band on. The head
    beside the name is `head.rs`. A change to the art keeps every line of a form the same width, the glyph set, the sizes and the palette rules; the tests fail otherwise and name the decision.
  - `matchrow.rs`: the pure cut of the one row under the Ctrl-R bar. `cut(text, marks, width) -> Vec<Piece>` takes the row text, the ranges of the
    query's occurrences in characters (`ham_core::history::Shown`) and a width in terminal cells, and gives pieces (`Piece {text, marked}`) that together take at most `width` cells; text that fits is whole,
    text that does not is cut to a window that starts a few cells before the first occurrence (a quarter of the width, at most `CONTEXT` = 12) with an `ELLIPSIS` on each side that was cut, and a width of
    three cells or less has no ellipsis and starts at the occurrence. Widths come from `select::cell_width`. `ui.rs` draws a marked piece reversed and bold (a style attribute, so it shows without colour).
  - Several backends: while `App::backend_view` holds two backends or more (the loop sends `Msg::Backends` after every read of the list; a client with one backend sends none), the Runs table draws a BACKEND column after STATUS (`runs_table::BACKEND` wide, from `OwnerLookup`, the plane's run map), the status row shows one banner for each backend that did not answer (joined with the error of a failed read), the New run screen draws a Backend box before the Model box (`Field::Backend`, `BOX_BACKEND`, Tab order `FIELDS_MULTI`, ←→ picks, the note names whether the backend is declared safe for customer content), a start with no pick is refused on the form, the word `--customer-content` in the Options box marks the start (refused on a backend not declared safe) and never reaches the parser, and the start is `Effect::SubmitDirectOn`. `tui::attach_backends` (called by `cli::tui_session` before the first frame) gives the app the run map, the pick, the declaration of each backend (`ControlPlane::declared_safe`, a one-backend client's too) and the first `BackendView`, so a client with one backend refuses `--customer-content` unless `customerContent` is true, and a client with several knows its backends before the first read of the list. The title tag reads `backend mixed` when the versions differ (`version_tag`). Single-backend screens are unchanged.

  - `runs_table.rs`: the pure, public plan of the Runs table's columns. `runs_columns(width) -> RunsColumns { name: Option<u16>, created, target, phase }` takes the screen width.
    The constants are GUTTER 1, RUN 28, STATUS 14, TARGET 24, PHASE 12, CREATED 20 and a gap of 1. The inner width of the table is the screen width minus the 2 border columns, and the gutter, RUN, STATUS and the gap before NAME take 46 of it. NAME takes the rest and is shown from 12 columns
    (a 60-column screen). CREATED, TARGET and PHASE join in that order, each only while NAME keeps at least 30 columns: NAME is 12 at 60 columns and 32 at 80, CREATED joins at 99, TARGET at 124 and PHASE at 137, and the columns and gaps fill the inner width exactly. On screen the order is
    gutter, RUN, STATUS, NAME, TARGET, PHASE, CREATED, so RUN is always followed by STATUS (the live drivers read the status as the first word after the run id). `ui.rs` `draw_runs` draws the columns the plan names.
- `history`: the full-history chat view. Opening a run
  asks for the newest 32 chunks (`before=end`), or, from a search hit, the chunks after its `open` token; the progress feed and its label stay until the first page says the store has the
  run; `unsupported` or `purged` keeps them and asks no more. The view is `stored::compose` of the pages read, the run's events and its messages, so a reopen shows the same. Scrolling to
  the top (`ChatView::at_top`), or a conversation that fits the screen with older chunks behind it (`ChatView::fits`), asks for the chunks below the oldest read (`before=<seq>`), and
  `TranscriptState::keep_view` keeps the view on the block it showed. A live run is polled with the last cursor every 3 s, one request at a time; an ended run read to its end is not
  polled (three polls that bring nothing stop it). A follow-up run also reads the previous run's conversation once (`Parent`; the first page, older pages when
  the view reaches its top, a first read given up after three failures) and `with_previous_run` puts its blocks (ids prefixed `prev:`) above a divider notice, `previous run, read-only. The new session received
  only the prompt you submitted, not this history.`, above the run's own blocks; the run's own older chunks are then read as the timer fires, because its top is below the divider. Tests `crates/ham/tests/tui_followup_link.rs`. A first page that the store answers `unsupported` while the run is still going is not kept: the feed stays and the page is asked again after 2 s, doubling to 30 s, until the capability appears; a run that has ended gives up on the answer. A failed read keeps what is shown, says so once on the notice row and is asked again after 10 s, doubling to 60 s; a first page that fails three times in a row (a stage without the route) is given up for the run. Nothing a stored
  record says reaches a key, a role, a card or a command: the keys come from `RunView.input` (the event question state machine), and a stored block is engine origin.
- `send`: what Enter does by the
  run's state (`Plan`): an open question is answered through the input route, unchanged; an active run on an image with message control gets a message for after the turn; an image
  without it (a capability record with no `message.next`, or a `no-message-control` answer) keeps the text and says that Cline is not waiting; a finished and delivered run prepares a
  follow-up. The box title says which (`Enter: Send next to run <8 characters>`, `Enter: Answer question`, `Enter: Prepare follow-up from commit <sha>: new billable run`, `Not available: ...`,
  `Enter: Interrupt run <id> (/now)` while the draft is `/now ...`). Enter is bound to what the box showed: a change of the plan, the open question or the session less than 300 ms
  before the key blocks it (`send_context_changed` notes the change, `send_enter` checks it against the key's time) and never retargets the text. The guard covers an answer too: a question that
  closes or is replaced less than 300 ms before the key blocks Enter. A blocked Enter adds `(changed just now)` to the end of the title for 2 s (`FLASH`).
  - Sending: a send makes a `Local` (the message id is the idempotency key) and a pending row `sending`; the server's answer replaces it with the message. A definite refusal
    (`ham_core::messages::explain_refusal`) puts the text back. No answer or a 5xx keeps the id, and the row reads `sending; no answer yet, the message is checked again by its id`. The id is asked
    again every 4 s for 5 asks (`FAST_TRIES`), then after 8, 16 and 32 s, then every 60 s (`retry_pause`, `RETRY_MAX`), for as long as the service is silent. The answer to a repeat is the message as
    the service holds it. Only a definite refusal puts the text back; an uncertain send is never called `not delivered`.
  - Answers: an answer typed in the box while a question is open (`answer_with`, after `send_enter` returned `None`) is a sent message. `send_answer_started` makes an `AnswerRow` and a pending row
    `sending`. `send_answer_done`, called first in `answered`, marks the answer taken: the row reads `sending: waiting for Cline to take it` while the question it answers is still open, and the answer
    stands in the conversation as the user's own block (`ans:<request id>`, placed by the app's clock time, from `send_user_items`). A refusal removes the row and puts the text back. A run whose
    conversation is only the progress feed gets the user's own items too (`send_with_user_items`, called by `hist_model`).
  - Held messages: `w` (browse mode, `Action::Withdraw`) withdraws the chosen message when two or more wait for a confirmation, else the newest message that is pending or held (`409 already-dispatching`
    says so and reads the list again). With two or more messages held or delivery-unknown, one is chosen at a time (`SendState::choice`, `App::chosen`). `h` (`Action::ChooseHeld`) moves the choice to
    the next one and wraps. `x` (`Action::ConfirmHeld`) shows the preview of the only such message; with two or more and none chosen it chooses the first and shows no preview; with one chosen it
    shows that one's preview panel (run, old and new session, the full text, a warning for a message that may have reached Cline). The second Enter, not within 300 ms of the preview, sends a new
    message with `confirms` naming the held one and the session shown; Esc cancels. It sends nothing, and says so, when the session changed again since the preview (the page's epoch is not the one
    shown) or the message is no longer waiting. The chosen row reads `held, chosen: ...` (or `may have reached Cline, chosen ...`), the others `held: ... (h chooses)`.
  - Keys and footer: `w`, `x` and `h` are rows of `keys::TABLE` and run through `browse_action`, after the search bar and the result panel; `send_browse_key` keeps only the second Enter of a held-message
    preview. While a message can be withdrawn or confirmed, `App::send_footer` (called in `draw_dashboard`) puts `w withdraw`, `x confirm` and, for two or more, `h choose` at the front of the browse
    footer, and `Esc, then w withdraw, x confirm, h choose` (as far as they apply) at the end of the typing footer. `typing_hint` chooses the wording of `Esc browse` for the line as
    `send_footer` draws it, ending included, so a wide wording takes no room from the ending while the usual line and the ending fit (90 columns, 111 with all three keys; tests `the_typing_footer_*` in `tui_send.rs`).
  - `/now <text>` needs the session's `message.now` control and shows the same kind of preview before it sends the message with mode `now`. The preview also says that a cancelled turn leaves no
    partial text in the engine's memory, so the message must restate what it refers to.
  - The pending rows (`PendingStatus::Words` carries the words of the states that `PendingStatus` had no variant for) show the last three messages: open ones, held and may-have-reached ones until
    acted on, settled ones for 20 s; delivered messages stand in the conversation as the user's own block (`send_user_items` -> `stored::compose`). The list of messages is read at open, every 2 s
    while a message is open, and at once after a refused withdrawal.
  The follow-up: once a finished, delivered run is on screen the 1 s timer asks `Job::Head` (`GET /v1/runs/{id}/head`, `HeadLookup` in
  `SendState`: asked once, asked again after no answer, a 429 or a 5xx up to four times at 3, 10 and 30 s, never again after a refusal the server states). The checked answer
  (`ham_core::head::DeliveredHead`) gives `SendState.head`, the box title `Prepare follow-up from commit <7>: new billable run`
  (before it arrives: `from the delivered result`), Cline's last message for the key below and the validator's change list for the digest. Once the box holds text, `follow_up_hint` draws the two sentences of the offer
  in the panel's place; the first Enter puts the user's text and the digest in the box and shows the confirmation panel (the offer, the note, `ham_core::followup::confirmation`: the commit, the source run
  and its target, the target of the new run, that it is a new billable run; a head that arrives while the panel is up is written into it and the 300 ms guard of the second Enter keeps its first time); a run whose head route
  said `no-delivered-head` keeps the text and says it delivered no code. The digest (`digest_inputs`) takes the objective and the user's delivered messages from the app, the outcome label and the pull request link from `RunView`,
  the number of harness prompts from `RunView.harness_prompts` (an event count), and the changed files from the head answer: `Unavailable` when there is no record, `Listed` otherwise (an empty list reads `none`).
  Ctrl-L (`Action::ClineMessage`, a typing-mode row of `keys::TABLE`; `add_cline_message`) works while the confirmation is showing: it appends Cline's last message after everything in the box in the fenced, nonce-labelled
  `written by Cline, untrusted` block (cut to 1,500 characters; the person's edits stay; the word is the full 32-hex nonce made at the first Enter, made again when the message holds it), a second press says it is already there
  and changes nothing, and with no message (or no answer yet) it says so; outside a prepared follow-up it says "repainted the screen · " and then what the key is for (Ctrl-L also repaints the screen on every screen: the bullet "Repaint of a damaged terminal"). Nothing else puts that message in the box or in the start.
  The second Enter, 300 ms or more later and with the panel still showing, waits while the head lookup is out (it says so), then first sends `Job::FindFollowUp`
  and then, when none is going, the job `App::follow_up_job` chooses: `Job::StartRepoFollowUp` when the finished run's summary names a repository (`Repo` or `NewRepo`), else `Job::StartFollowUp`, with exactly the text in the box and the head the person was shown.
  The choice reads the same `RunDetail.summary.target` as the confirmation, so the run that starts is the one that was confirmed: a repository run's follow-up runs on that repository and opens a pull request there (the confirmation says so and names the three cases of
  `crates/delivery/AGENTS.md`), an `s3-only` run's, or one whose summary has not arrived (the confirmation then said s3-only), delivers a zip alone. `ham continue` on the command line reads the same summary (`GET /v1/runs/{id}`) and goes the same way; `--s3-only` or `--repo cc:<name>` on its line wins.
  `ext::start_follow_up` makes the ordinary direct-run request with the target (`s3-only`, or the repository of `Job::StartRepoFollowUp`; control-api refuses a repository that is not the source run's own), the user's rules and skills, `continueFrom {runId, headSha}` (control-api answers 409 `head-mismatch` when the delivery is another commit; the
  client then drops the shown head, asks again and keeps the text) and the key `followup::start_key(source, head, text)`; without a head (a stage with no head route, or a lookup that failed) it sends `continueFrom {runId}` and the
  empty seed, as `ham continue` does. The answer opens the new run (only when the old run is still on screen) or says why it did not start and keeps the text. Tests: `crates/ham/tests/followup_head.rs`, `tui_send.rs`,
  `crates/ham/tests/followup_start.rs`.
  The retry of a blocked run: a run whose status is `blocked` ended without a session, so `send_plan` answers `Plan::Retry` before it looks at an open question (`question` is `None` for a blocked run). The box title reads `Enter: Retry blocked run <8>: new billable run`; with text in the box the hint panel (`follow_up_hint`) says what happened. Enter on text (`retry_offer`), `r` in browse mode (`Action::Retry`, `retry_action`; on any other run it opens the Runs list as before) and `/retry` (`retry_word`; on another run it says "only a blocked run can be retried" and starts nothing) show the confirmation panel and set `Confirm::Retry`. The text stays in the box and is not sent. The second Enter, or the second `r` in browse mode, 300 ms or more later and not inside a key burst (a held Enter or a paste is never a confirmation; `retry_confirm_key` through `send_typing_key` and `send_browse_key`, so an empty box confirms too) sends `Job::RetryRun`; an earlier press is ignored, Esc withdraws the offer, any other key withdraws it and acts as itself. The answer (`retry_done`) opens the new run when the blocked one is still on screen and carries the unsent text to its box; a refusal keeps the run and the text and shows the words. `SendState.retrying` ignores a press while the call is out. The key line (`usual_help`) shows `r retry` in place of `c cancel`, typing mode ends with `Esc, then r retry` (`send_footer`), the blocked card reads `blocked · r retry`, `c` says the run has ended, and the command popup (`popup_for`) leaves out `/cancel` on a blocked run and `/retry` on every other run. The Runs list marks a run with `continueFrom` with `↳` in its NAME cell (`ui.rs` `name_cell`; the filter never marks the mark) and the note under the table for the selected row reads `continues run <id>`.
- `model_title` (`model_title.rs`; tests `crates/ham/tests/tui_model_fallback.rs`): the title and the status card never depend on one event. `ui.rs` takes the model from the session (`RunView::direct_model_id`), else `App::model_fallback`: the model asked
  for when the run was started from the composer (`model_asked`/`model_started`), else the run index's (`run_index`); `model_probe` reads the list once (`Effect::RefreshRuns(Refresh::Timer)`, then at most every 30 s) for a run that shows events and no model.
- `run_name`: which name the run screen shows for the focused run, and when to read it. A model's name (source `ai`)
  wins wherever it comes from: the last read of the run's row, the held Runs list, or the seed this app computed from the prompt it sent from the composer (`name_asked` at submit, `name_started` when the start is answered, `ham_core::runname::seed_name` over `prompt_head`, the
  function the server uses). When no model has written one, the first that exists in that order shows, dim. `name_open` runs when a run is opened: it starts the age clock and, unless a model's name is known, makes `Effect::ReadRunName` (one read at open). `run_name_tick` runs on the
  dashboard's 1 s timer: the fifth tick after the last read asks again while the shown name is not a model's, no read is out and the run is under 60 seconds old (age from `createdAt` of the held list or the run detail and the wall clock, else the ticks since the screen opened). The
  reads stop at the first model name. The read is `Api::run_name_of`: `GET /v1/runs?q=<run id>&since=<ten minutes back>`, one page, one attempt. Its answer is `Msg::RunName { run_id, name }`. A reply for a run that is not on screen changes nothing; a failed read, or a row with no name, keeps the
  name shown and the schedule; a seed never takes the place of a model's name. Nothing here writes: the name is display text only, and no request, file or label reads it. `ui.rs` hands the shown name to the chat view (`ChatFacts.name`, below).
- `openbox`: one box over the whole screen, opened by Ctrl-O (Runs and run screens) or `/open <words>` (a lone run id still opens
  the run). Rows are the held runs that match the words (the Runs filter's matcher, which searches the run's name as well as its title) with the hits of `GET /v1/search` merged in (asked once, after the words rest for 1 s and are at least 2
  characters), grouped by repository, each with state, the run's name and, for a chat hit, a snippet. Up and Down move the mark, Enter opens the run (a chat hit reads the transcript from its `open`
  token), Esc closes. `/sessions` is the Runs screen.
- `engine_ui`: the capability record of the focused run
  (`App::caps`, read from its `session.capabilities` events after every poll) drives the popup (`engines::popup`: the built commands the session offers, then the engine's own commands named
  `//word`; a typed `//` lists only those), the header's state words (`active · mode act`), `/engines` (the panel of `engines::panel_rows`) and the forwarding of a typed `//word` that the record
  advertises (a message for after the turn with one slash; a pasted draft is refused). `/help` lists the engine's commands first. The `current_mode_update` and `available_commands_update` records of the stored conversation since
  the last `session` record are applied over the snapshot after every page (`StoredChat::capability_updates`, `Capabilities::apply_update`).
- `ChatBox` (frozen for the modules that build against it; the in-house core and the `ratatui-textarea` core give the same behaviour):

  ```rust
  pub enum Kind { Multi, Line, Token }          // many lines; one line; one line without whitespace
  pub struct ChatBox { .. }                     // Clone, Debug, Default, PartialEq on kind and text
  impl ChatBox {
      pub fn new(kind: Kind) -> Self;
      pub fn with_text(kind: Kind, text: &str) -> Self;     // caret at the end
      pub fn kind(&self) -> Kind;
      pub fn text(&self) -> String;                         // lines joined with \n
      pub fn lines(&self) -> &[String];
      pub fn char_before_cursor(&self) -> Option<char>;     // for the backslash-then-Enter rule
      pub fn is_empty(&self) -> bool;
      pub fn cursor(&self) -> (usize, usize);               // (line, column in characters), from 0
      pub fn caret_index(&self) -> usize;                   // the caret as a character offset in the whole text (a line break counts one)
      pub fn set_caret_index(&mut self, index: usize);      // clamped to the end; drops a selection
      pub fn set_text_caret(&mut self, text: &str, caret: usize);
      pub fn remove_chars(&mut self, range: Range<usize>);  // one edit of the undo history; the caret goes to range.start
      pub fn undo(&mut self) -> bool;                       // Ctrl-Z; at most 50 edits
      pub fn redo(&mut self) -> bool;                       // Alt-Z
      pub fn select_all(&mut self);
      pub fn selection(&self) -> Option<String>;            // made with Shift and a movement key
      pub fn clear(&mut self);
      pub fn set_text(&mut self, text: &str);               // caret at the end
      pub fn insert_str(&mut self, text: &str);             // a paste; line breaks follow the kind
      pub fn insert_char(&mut self, c: char);
      pub fn newline(&mut self);                            // ignored unless Kind::Multi
      pub fn handle_key(&mut self, key: KeyEvent) -> bool;  // editing keys only; false means "the screen's key"
      pub fn rows_at(&self, width: usize) -> usize;         // display rows at an inner width, at least 1
      pub fn render(&self, f: &mut Frame, area: Rect, block: Block<'_>, focused: bool);
      pub fn render_marked(&self, f: &mut Frame, area: Rect, block: Block<'_>, focused: bool, marks: &[Range<usize>], mark_style: Style);
  }
  ```

  The core is `ratatui-textarea` 0.9.3: caret, selection, undo and redo, word moves and word deletes, soft wrap at word boundaries
  for `Kind::Multi` and a sideways scroll for the one-line kinds. `input.rs` keeps the app's keys (Enter, Tab, Esc, Ctrl-J, Ctrl-R, Ctrl-U,
  Ctrl-C, Ctrl-S, Ctrl-G, Ctrl-M) away from the crate, puts undo on Ctrl-Z and redo on Alt-Z, hides the crate's cursor cell (the terminal's
  cursor is the caret, except under a selection made backwards, whose first character is under the caret: `sync_cursor_style` gives that cell the selection style after every edit) and measures the wrapped height by laying the text out once and reading the caret's row.
  Enter, Tab, Esc and any key with Ctrl or Alt held that the editor core does not own are the screen's. `render` scrolls so the caret row
  is shown and, when `focused`, puts the terminal's cursor at the caret. Drafts are kept by `App`, one `ChatBox` per run id, for the life
  of the process (invariant 5: no history file, no new local state).
- Seams for other modules (names fixed; the TUI-core owner implements them in `app.rs` and `ui.rs`):
  - `CommandSink`: `pub type CommandSink = fn(&mut App, &str) -> Submit;` with `pub enum Submit { Handled(Vec<Effect>), Refused(String), Pass }`
    and `App::set_command_sink`. The chat box calls the sink with the submitted text (trimmed, never empty). `Handled` clears the box and runs
    the effects; `Refused` keeps the text and shows the reason on the notice row; `Pass` leaves the line to the default route, which is the
    router of `ham_core::router`: chat answers the open question (else the text stays and the screen says Cline is not waiting); a
    refusal goes to the result panel and the text stays; a command that only reads or writes a file runs at once as `Effect::RunCommand`; a
    command that changes something shows a preview (`commands::Armed`) and waits for a second Enter; text that was pasted always shows the
    preview; `/watch`, `/open`, `/new`, `/help` and a start line are handled by the screen (the runner refuses `watch` and a new run);
    `rules edit`, `push` and `continue` show their preview like the rest and, after the second Enter, hand the terminal to a child `ham` (`Effect::RunTerminal`, `handoff.rs`); a `push` with no URL first opens the slot for it; Alt-Enter sends the whole box as chat unchanged and bypasses the sink and the router. A custom sink sees the line first (trimmed, as before) and keeps working, and a sink that runs commands must apply the router's rules itself: `App::draft_is_tainted` tells it whether the draft holds pasted text. Text that arrives as a burst of keys taints the draft as a paste does.
  - `Effect::RunCommand { invocation, argv }` and `Msg::CommandDone { argv, out }`: a command typed in the box is run by `perform` with
    `cli::run_captured` (the code of the shell, its text kept as data) and its `CmdOutput` goes to the panel. `App::take_flush` tells the event
    loop to drop the key events it has queued after a preview was armed; `App::set_config_revision` takes the function that reads the config file's
    revision (the process passes `commands::config_revision`): a preview made under another revision does not run.
  - The run screen is the chat view (W3): `App::draw_dashboard` builds the `Header`, the `Card`s and the `ChatFacts` and calls
    `ChatView::draw_in` (`draw_with` is `draw_in` with no `Band`; with the title band the app passes `Band { title, cap_base }`: the header goes to the band's title rectangle and fits that width less `header_reserve`, the box keeps a third of `cap_base` rows, and `areas_capped` is `areas_named` with that cap told apart from the area; `tui_head.rs` compares `areas_named` with the layout as it was for every height and combination), which returns the rows it kept free between the pending rows and the box (`Areas::reserved`) for the result panel
    and the preview of a command. `App` keeps the `ChatView` in a `RefCell` (a draw sets the cards on it); `rebuild_chat` makes the
    `ChatModel` again from the events after every poll and when the detail arrives, without the model's own `status` and `delivery` blocks.
    Browse mode maps ↑↓ j k PgUp PgDn Home End g G to `ChatView::scroll`, Tab and Shift-Tab to `select` and Space to
    `toggle_selected_fold`.
    Search inside the chat is wired in browse mode: `App::search_key` runs before every other browse key (`/` opens the bar through
    `ChatView::with_state`, `ChatView::transcript_area` is the area `mark_of` needs; while `takes_text` every character is the query's; Esc is the search's while
    `is_active`; `n` and `N` walk only after Enter); the bar is drawn by `SearchBar` in the last of the reserved rows above the box, with the terminal cursor after
    the query while it is typed; `rebuild_chat` calls `model_changed` after every new model; opening another run drops the search. The header writes the whole run id: the live drivers read it from the title.
- `Transcript`: a ratatui `StatefulWidget` over a `ham_core::chat::ChatModel` that draws
  into the `Rect` it is given and writes nowhere else. `Transcript::new(&model)` with `TranscriptState` (kept by the chat view). The area holds a
  role gutter (`GUTTER`, 7 columns: the word of `ChatModel::role_word`), a fold marker (columns 7 and 8; `▸` folded, `▾` open, only for a block
  that has both a head and a body), the text from `CONTENT_X` (column 9), a tool call's status right-aligned (`done`, `failed`, `running`,
  `pending`), and in the last column a scrollbar when the transcript is longer than the area. A progress-feed model starts with the label
  `progress feed only: not the full conversation` as its first row. Text is content: it is drawn only right of the gutter, goes through the
  sanitizer again at layout (tabs become spaces, controls and escape sequences go, bidi marks are shown), and cannot draw a role word, a border or
  a prompt; the chrome the app draws around the area (the confirm line, the input box) is outside it.
  - Layout is cached per (block id, width, fold state, `Block::rev`); `TranscriptState::wrap_computations()` counts layouts, so a scroll lays out
    nothing and an appended or a growing block lays out only itself (tests count, they never time). Line offsets are `usize` (a 16-bit
    `Paragraph` scroll stops at 65,535); the block under a line is a binary search over prefix sums.
  - Follow mode keeps the newest line in view. `scroll_up`, `page_up`, `to_top` or `scroll_to_line` pause it and `[ Jump to bottom ↓ n new ]`
    shows how many blocks arrived since; `to_bottom`, or scrolling to the end by hand, follows again. `range_text()` is `lines 41-64 of 480`.
  - Folds and selection live in the state, keyed by block id: `toggle_fold(&model, id)`, `select_next` and `select_prev` (the selected block's gutter is
    reversed and the next draw scrolls it into view), `toggle_selected_fold`. `hit(x, y)` names the block under a point of the last draw
    (`Hit {block_id, index, row, on_fold_marker}`), and `click(&model, x, y)` folds or opens the foldable block whose head row was pressed.
    The chat view maps keys and mouse events to these methods; the widget reads no events.
- `ChatView`: the screen of one run, drawn into a `Rect` by `ChatView::draw(&self, f, area,
  &ChatFacts, &dyn InputPane)`. From the top: a `Header` line (`ham · Run 01M5287J… · Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · updated 2s
  ago`; a narrow screen gives up the age, the cost, the tokens, the elapsed time and the model, in that order), an optional banner, the transcript
  (`Transcript` over the `ChatModel`), up to three `PendingRow`s (`you      <text> — sending | waits for this turn to end | delivered in turn 3 |
  not delivered: <why>`), the pinned input box, a notice row and the footer line. `areas(area, banner, pending_rows, input_rows, notice)` is the
  layout: the parts tile the area exactly, the box keeps at most a third of the screen, and on a short screen the conversation keeps three rows
  first, then the box, the notice, the banner and the pending rows take what is left. The module holds no `App` and reads no events.
  - Seams for the TUI-core owner (the branch that replaces the Dashboard): the app builds `ChatFacts` (header, banner, notice, pending rows, footer,
    box title, focus) and the `Card`s from strings it already has, and calls `set_model` when new events arrive and `set_cards` for the status and
    agent card (folded), the question card (open, `alert`), the blocked panel and the delivery card (open). A card line never wraps at
    `ChatView::card_width(area_width)` columns, so the delivery card's one-line link is fitted to it (`render::link_line`). `InputPane` is the box:
    `rows_at(inner_width)` and `render(f, area, block, focused)` are the frozen `ChatBox` signatures, so `impl InputPane for ChatBox` forwards them.
  - Keys and mouse: `scroll(ScrollAction)` (line, page, top, bottom), `select(Step)`, `toggle_selected_fold()`, `click(x, y)` and the read-outs
    `is_following`, `new_blocks`, `range_text` go to the transcript; `ChatView` draws from `&self` (its scroll state sits in a `RefCell`).
  - Every string handed to the view is cleaned again (controls and escape sequences go, bidi marks are shown, token-shaped strings are hidden), and the
    view writes only inside the area it is given.
  - The status card's outcome: the whole label of `run.completed` (a harness stop names its reason in it, so it wraps and is not cut), then one line per
    `RunView.observations` entry directly under it (`! ` for the kind `last_log_entry_red`, a plain indented line for another kind); tests `crates/ham/tests/tui_result_card.rs`.
  - The card of a goal-loop brake: `push_card` adds the `RunView.goal_brake` card (`ham_core::goal_card`) to the cards of `chat_cards`, after the question card, as an `alert` card
    (the needs-input style): the rule, how far the loop got, the engine's BLOCKED line and `ham continue <run> '<text>'`. It is one line in `ui.rs` and one `mod` line; tests `crates/ham/tests/tui_goal_brake.rs`.
    The Options box of the composer also takes `--goal on|off`, `--research`, `--no-research`, `--allow-third-party`, `--compute auto|microvm|instances` and `--attach <file>` (`OPTIONS_TAKE`, `option_words`, `option_text`; tests `tui_composer_members.rs`).
    `option_text` writes every start option of a parsed `DirectArgs` back into the box, `--compute auto` too (it beats `run.compute` of the file), so a start line typed in the chat box opens the form with all its options and Ctrl-S sends what was typed (test `tui_router.rs`). A new start option therefore needs its word in `OPTIONS_TAKE`, `option_words`, `value_hint` and `option_text` together.
    The empty box shows a dim hint of its words, as many whole words of `OPTIONS_HINT` (`ui.rs`) as fit its width, so a narrow box never shows half a word; `--compute` is the last of them.
  - The skills card: `push_card` adds the `RunView.skills` card (`ham_core::skills_card`) to the cards of `chat_cards` right after the agents card, as a folded card titled
    `skills · N placed` (Space opens it: one row per skill with the first 12 digits of its digest, and the two sentences about where the skills sit and whether the engine's own list showed them). It is one line in
    `ui.rs` and one `mod` line; tests `crates/ham/tests/tui_skills_card.rs`.
  - The engine settings card: `push_card` adds the `RunView.engine_config` card (`ham_core::engine_config_card`) to the cards of `chat_cards` right after the skills card, as a folded card titled `engine settings placed · <engine> · N set · settings hash <12 digits>` (Space opens it to one `key = value` row for each placed setting). It says placed and never applied. It is one line in `ui.rs` and one `mod` line; tests `crates/ham/tests/tui_engine_config_card.rs`.
  - The follow-up's line about engine settings (`send.rs`, `SendState::engine_now`; tests `crates/ham/tests/followup_engine.rs`): the first Enter of a follow-up returns `Job::EngineSettings`; the answer is kept and `follow_up_panel` adds `Engine settings changed since run <id>: ...` when the hash differs from the hash of the source run's card (a run with no card counts as none). A refused file adds nothing there.
  - The goal loop's notices (`NoticeKind::GoalLoop`, `GoalRound`, `GoalBrake`) are drawn plain, `GoalQuiet` (off, fallback, rearm) dim and `GoalBlocked` (the
    engine's BLOCKED line) bold; the screen needs no code of its own for them (tests `crates/ham/tests/tui_goal_card.rs`).
  - The name row: `ChatFacts.name: Option<(&str, bool)>` is the run's name and whether a model wrote it (`App::run_name_shown`). `areas_named(area, banner, pending_rows, input_rows, notice, reserved_rows, name)` adds `Areas.name`, one row directly under
    the header and above the banner; `areas` and `areas_with` call it with `false`, so a screen without a name lays out exactly as before. The row is taken last, after the pending rows, so a short screen drops it first and the conversation keeps its three rows. It is absent without a name and on a screen
    narrower than 20 columns. The text is cleaned like every string handed to the view, cut by display width with an ellipsis, and drawn plain for a model's name and with `Modifier::DIM` for a seed. The header line is unchanged: the live drivers and the version tag read it. The tags of the run follow the name on the same row, two spaces later and dim: the largest number of whole tags that fits after the name, then ` +N` for the rest (`+N` alone when no whole tag fits); the name has priority, so a name that fills the row leaves no tag, a run with tags and no name draws no row, and a screen with no name row draws none; tests `crates/ham/tests/tui_chat_tags_row.rs`.
- Search inside one chat. The pure part is
  `ham_core::search` (see `crates/ham-core/AGENTS.md`); this module carries out what it asks of a `TranscriptState`. The TUI-core owner stores one
  `ChatSearchUi` beside the `ChatView` and wires:

  ```rust
  pub fn mark_of(state: &TranscriptState, area: Rect) -> ScrollMark;          // where the view is: top line, follow mode, selection, block on the first row
  impl ChatSearchUi {
      pub fn new() -> Self;
      pub fn search(&self) -> &ChatSearch;                                    // phase, query, hits, current, count_text, bar_text, truncated
      pub fn is_active(&self) -> bool;                                        // bar open or matches marked: Esc is the search's first (true while typing too)
      pub fn takes_text(&self) -> bool;                                       // the bar is open: every character, an `n` too, is the query's
      pub fn walks_matches(&self) -> bool;                                    // accepted with Enter: `n` and `N` walk the matches (not before)
      pub fn open(&mut self, state: &mut TranscriptState, model: &ChatModel, area: Rect); // browse-mode `/`; `area` is the transcript's Rect of the last draw; a bar already open is cancelled first
      pub fn close(&mut self, state: &mut TranscriptState);                    // leaving the chat: search, marks and a queued jump are dropped, folds stay
      pub fn key(&mut self, key: ham_core::search::Key, state: &mut TranscriptState, model: &ChatModel); // typing in the bar; Esc on an accepted search
      pub fn next(&mut self, state: &mut TranscriptState, model: &ChatModel); // `n` once accepted (also Down or Ctrl-N while typing)
      pub fn prev(&mut self, state: &mut TranscriptState, model: &ChatModel); // `N`
      pub fn model_changed(&mut self, state: &mut TranscriptState, model: &ChatModel); // after every `ChatView::set_model`: nothing happens when no block changed, else hits and marks are recomputed; the view stays
  }
  pub struct SearchBar<'a>; impl SearchBar { pub fn new(&ChatSearch) -> Self } // a Widget: one row, `/query  3/17`, nothing while no search is open
  ```

  `Key` is `Char(char) | Backspace | DeleteWord (Ctrl-W) | Clear (Ctrl-U) | Enter | Esc`; the key table maps the real keys. Behaviour: typing jumps to the first
  match at or after the block on the first row of the view (the newest match when the view follows the end); Enter accepts (marks stay, `n` and `N` walk
  them with a wrap); Esc while typing, and a query with no match, put the scroll position or follow mode back (a jump queued for the next draw is
  cancelled too); Esc on an accepted search only clears the marks; a match in a folded block, in its head or its body, opens that block (one at a
  time) and cancel closes it again; Enter hands the open block to the user, and a later `n` closes only the blocks the search opened after that; the hit is
  brought to the middle of the view. Drawing: the app draws `SearchBar` in the row it reserves and, while typing, puts the terminal cursor after
  `/query`. Nothing here reads events, calls `App`, or writes outside the areas it is given.
- `TranscriptState` gained: `fits` (the whole transcript fits the area at the last draw), `keep_view` (remembers the block at the top of the view and its offset; the next draw puts the
  view back on it, so older history that loads above does not move what is read; nothing is kept in follow mode) and `ChatView::{fits, at_top}`. `PendingStatus::Words(String)` carries the words of a message
  state that the other variants do not name.
- `TranscriptState` gained: `is_folded(model, id) -> Option<bool>` (`None` for a block that has nothing to fold), `open_fold` and
  `close_fold` (`true` when the state changed), `reveal_text(id, nonblank_before)` (at the next draw the row that holds that non-blank character of
  the block, head first, goes to the middle of the view and follow mode pauses), `set_marks(HashMap<block id, Vec<Mark>>)` and `clear_marks`.
  A `Mark {nb_start, nb_len, current}` counts non-blank characters, because wrapping drops blanks and nothing else; the widget paints the cells of every
  marked character (and the blanks inside a mark) with `OTHER_MARK` or, for the current match, `CURRENT_MARK`, also when the wrap cut the match in two
  rows. Marks assume one cell width per character as the widget lays text out, so an emoji joined with ZWJ may be painted a cell off.
- Runs screen filter. The TUI-core owner wires: the text of the find-or-command box goes to `RunsFilter::set_text` on every change (a command attempt filters nothing); the rows are `RunRow::from_summary` of the held `RunSummary` list with `with_index` of the run index entry held for the run, rebuilt when a list arrives; the list draws the indices of `visible(&rows)`, first a one-character gutter (`Gutter::glyph`; bold for `NeedsYou`, red for `Failed`), then the cells with `highlight(cell)` ranges marked; the note line is `summary(shown, total)`; the scope key calls `toggle_scope(focused_row)`; Enter opens the run at the visible position; after a change the selection comes from `select_after`. `Effect::LoadRuns` carries a `Refresh` and runs `api.read_runs(how)`, which gives a `RunsRead`; the answer message applies it with `read.apply(&held_now)` against the list held when it arrives, never against
  a copy taken when the request began (a timer read and a manual read can be in flight together, and a stale copy would overwrite the longer list); the 1 s timer reads the newest page only (`Refresh::Timer`), `r` reads every page (`Refresh::Manual`); a failed read keeps the held list. A run older than the newest page is refreshed only by `r`: if it starts to wait, its gutter mark and the bell come at the next `r` (the open run is polled through its own events). `newly_needing_you(old, new)` names the runs that began to wait, for one bell each. Wired in `app.rs` and `ui.rs` (W3): `App` holds `runs_rows` (`RunRow::from_summary` of `runs`, rebuilt by `set_runs`), `runs_filter`, `runs_editing` (the filter line is open; `/` opens it and every character is its text, Enter keeps the filter, Esc clears words and scope) and the timer state. `selected` stays an index into `runs`; Up, Down, the wheel and
  Enter go through `runs_visible()` so a hidden run is never selected or opened, and `select_after` keeps the selection on its run. `p` toggles the scope of the selected run. A command attempt typed on the line filters nothing; Enter on it clears it and says that commands run in a run's message box (`RUNS_COMMAND_NOTE`). `tick` on the Runs screen asks for `Effect::RefreshRuns(Refresh::Timer)` every fifth tick (5 s) and never while the last one is unanswered (`runs_inflight`); `Msg::RunsRead` applies the read with `RunsRead::apply(&self.runs)` against the list held when it arrives, and an error keeps the list and frees the timer. `r` stays `Effect::LoadRuns` (every page, `Msg::Runs`). The gutter is a separate first column (`Gutter::glyph()`, bold for `NeedsYou`, red for `Failed`); matched characters are underlined. Both are wired: `set_runs` makes each row with `RunRow::with_index` from the run
  index entry held in `App::run_index`, and `alert_effects` runs after every list that arrives, queues one `Effect::Bell` for the batch of runs that began to wait and an `Effect::Notify` for each when the option is on; the first list read only seeds it. `Alerts` is given `<run id>|<request id>` for each run that waits, so a run that asks a new question is a new wait. A run whose question already rang on its own screen is left out of the batch, and the bell goes to the first run in the batch that has not rung; opening a run whose question rang on the list rings nothing more.
- Run names on the Runs screen: the table's columns follow `runs_table::runs_columns` (RUN, STATUS, NAME first; CREATED, TARGET and PHASE as the width allows). The NAME cell shows `RunRow::label()`, the run's name, else the title of its
  prompt (a backend without names sends none), else a dim `-`. The text passes `runfilter::clean` (in `RunRow::with_index`) and `sanitize_line`, and is cut by display width with `cut_cols`, so a wide Japanese character is never split and a cut cell ends with an ellipsis. A name a model wrote
  (`nameSource` `ai`) is drawn plain; a seed (`nameSource` `seed`), any other source and a title are drawn with `Modifier::DIM`. The filter underlines the characters that are drawn: no mark reaches the ellipsis and none lies beyond the cut. The note line under the table adds, after the count (and the words about runs that wait), the selected run's tags as ` · tags project:calc topic:csv-export`, then the selected run's
  columns that the width hides, as ` · target cc:calc · phase planning · created 2030-01-01T00:00:00Z`, so a narrow table loses nothing the wide one shows; the terminal clips the row at its edge. The key line reads `n new run · Enter open · ↑↓ · / find · t tags · p <words> · r refresh · q quit` (69 columns without the `p` words; `s settings` joins it from 91) and, while a filter or a pick narrows the list, `Enter open · ↑↓ · / edit · Esc clears · t tags · p <words> · r refresh · q quit` (70). The `p` words say what a press does: `RunsFilter::scope_key_labels(selected_row)` returns the wordings from the longest to the shortest and `draw_runs` takes the first that fits the room the other keys leave, so `q quit` stays on an 80-column screen: with the limit off `p only cc:calc` (the repository cut to 24 characters), `p this repo`, `p repo`, and for a run with no repository `p only s3-only`, `p s3-only`; with the limit on `p show all runs`, `p all runs`, `p all`. The bottom border of the table carries `run 12 of 769` at its right end (`ham_core::runfilter::position_text`; the note line is full at 80 columns, so the position never goes into it), and nothing when no run is selected (tests `crates/ham/tests/tui_runs_key_label.rs`, `crates/ham-core/tests/runfilter_key_label.rs`). The hit map (one region for each run) and the version tag on row 0 do not change. The note under the table is one row. The note of a failed read (`cannot list runs: <error>`) that does not fit one row wraps at word boundaries onto up to four (`RUNS_ERROR_ROWS`), the table gives up those rows, and an error that needs more ends in an ellipsis; the key line stays the last row (tests `crates/ham/tests/tui_runs_unresolved.rs`).
- Chat box history, paste placeholder and external editor.
  The TUI-core owner wires, beside the per-run `ChatBox` drafts: one
  `HistoryRing` for the process (`record(&store.expand_all(text))` on every send; `set_run_messages` from the stored conversation's user blocks when a run opens;
  `set_earlier_runs` from the run index: wired, see `earlier.rs` below). Up on the first row and Down on the last row call `prev(box_text, draft_tainted)` and `next` and put `Recall::text` in
  the box with `ChatBox::set_text`; the draft's taint is saved with the walk and comes back with it; a recalled text is tainted.
  When `Recall::fetch_full` names a run, fetch the run's detail and call `supply_full(run_id, objective)`; while `pending_full()` is set the send key is refused with a notice
  ("fetching the full prompt"), `forget_pending()` when the user edits the box, and the answer replaces the box only if the box still holds `Recall::replaces`. Ctrl-R opens
  `ring.search(box_text, draft_tainted)`, the bar shows `bar_text(width)`, and Enter, Esc and Ctrl-R map to `ring.accept_search(search)`, `cancel` and `older`.
  One `PasteStore` per draft: `Event::Paste(text)` goes to `offer(text)` and the result (the text or the token) to `ChatBox::insert_str`, tainting the draft;
  `placeholders_in(text)` marks the tokens in the box; Backspace and Delete at a token take the whole of it (`token_ending_at`, `token_starting_at`); the expand key calls
  `expand_at(text, caret)` (the caret as a character index) and sets the text and the caret; send, the editor and the history use `expand_all`, and a send is refused while
  `dangling(text)` is not empty (an undo brought back a token whose text is gone); `clear()` when the draft is sent or emptied. Whenever the whole text of the box is replaced by
  text that is not placeholders (an editor's answer, a recall) call `clear()` first. Ctrl-G calls `Editor::from_env()` and
  `edit_with_report(&ed, &store.expand_all(text), &default_dir(), |cmd| suspend_for(cmd))`; `Edited {text, tainted}` replaces the box text and taints it, `Unchanged` and
  `Cancelled` leave it (show `Report::hint()` when there is one), an `EditError` goes to the notice row by its `Display` text. The editor runs through `suspend_for`
  so the terminal is restored around it.
- History and Ctrl-R. `App` holds one
  `HistoryRing` (in memory; `app.rs` and `history.rs` name no file function, a test reads both). `clear_chat` is where a line that was sent leaves the box (a message to Cline, a command, a form word), so it is also
  where `record` is called with the text; Ctrl-C does not go through it. `rebuild_chat` gives `set_run_messages` the You messages of the model (the run's prompt and what was sent). Up on the first row and Down on
  the last row of the box (`caret_on_first_row`, `caret_on_last_row`; a box never typed in counts as one empty row) call `prev(box_text, draft_tainted)` and `next()`; `apply_recall` puts the text in the box, taints
  the draft as the recall says (a recalled text never runs a command at once) and drops the popup, chip and preview; the popup does not open while a walk is on (`popup_entries`), so Up, Down and Esc stay the
  history's. Esc on a recalled text calls `cancel()` and puts the draft back with its taint; an edit that is not a recall (`recalled` is false) ends the walk with `cancel()`. Ctrl-R (`Action::HistorySearch`)
  opens `hsearch` (`ReverseSearch` over a snapshot): `hsearch_key` takes every key (characters and Backspace edit the query, Ctrl-R is `older`, Enter is `accept_search` and a recall, Esc leaves the box as it was);
  the bar and the match take two rows above the box (`draw_history_search`). The match row marks each occurrence of the query: `ReverseSearch::shown` gives the row text (each line break
  shown as ` ⏎ `) with the occurrences mapped through that change, `matchrow::cut` fits it to the width around the first occurrence, and `draw_history_search` draws a marked piece with
  `Modifier::REVERSED | BOLD`; letter case follows the search (`find_ranges`), and an empty query has no match, so nothing is marked.
- Status line, reconnect banner and alerts. One `Link` per open run: `on_ok(now_ms)` or `on_err(now_ms)` after each poll, and `link_overlay()` is the banner text of `ChatFacts`, which clears on the next good read (the banner shows after 1 s, or 5 s before the first good read). One `Alerts` for the process: seeded by the first Runs read (no sound; whichever of `Msg::Runs`, `Msg::RunsRead` and `Msg::RunsIndexed` brings it), then `observe` with the ids of the runs that wait (the `needs_input` rows of the Runs list and the open run's open question) after every read; a batch rings `Effect::Bell` once, and an alert that
  carries the notification comes out as `Effect::Notify(Alert)` (its `bell` cleared) that `mod.rs` writes with `Alert::write_to` into the writer the `TerminalSession` owns (the OSC 9 notification only with `set_notify(true)`, `HAMH_NOTIFY=1|true|on`, and a window that is not focused: `set_focus` from `FocusGained` and `FocusLost`; `set_tmux` from `TMUX`). This replaced the one-bell-per-question effect of the dashboard. The notification and the bell have the config keys `ui.notify` (`HAMH_NOTIFY` wins when it is set) and `ui.bell` (`App::set_bell`, which silences the bell of a question on the run screen and of the Runs
  list; the notification does not depend on it), read by `cli::apply_ui_settings`, which also gives the ham mascot (`App::set_mascot`: `ui.mascot`, with `HAMH_NO_MASCOT` over it, and the look the environment asks for; `head.rs`) and whether its smoke moves (`App::set_mascot_motion`: `ui.mascot_motion`, on unless the file says `false`). The header's model is the route in use: the model the latest `provider-switch` went to (`RunView.route.model`), else the one `session.started` named, else the run index's (a `new-session` event puts the route back on the model the user chose); a retry, a wait or an outage
  (`RunView::route_trouble`) is added next to the state (`active · provider error, try 2 of 4`, kept when a narrow header gives up its tail), and the status block keeps `Model <the model the user chose>` and, while the run goes on, adds `Route <RunView::route_now>` under it (`Route running on GPT-6.1 Sol after 1 switch`); a run that is over shows neither the trouble nor the row, and its `Ran on` row says where it ended.
- `markdown`: Markdown, code and diff rendering of block text as styled rows. In-house, no crate. It draws nothing and reads no event.
  Wired in `transcript.rs` (tests `crates/ham/tests/tui_markdown_screen.rs`): `layout_block` takes the rows of an engine message (`BlockKind::Message` with the engine role) and of a diff block's body from `markdown::render_block`
  (`body_rows`); the other kinds (the person's words, tool calls, thoughts, notices, cards) are wrapped as written. A `Row` keeps `text` (the plain characters: what a search counts and a selection copies) and
  `styled: Option<Line>` (what is drawn, with `Buffer::set_line`). A message with nothing in it that Markdown reads (`plain_prose`: no marker character, no line starting with a space, a bullet, a rule or a number and its dot)
  keeps the plain wrap, which is what its Markdown would be and costs a fraction of it, so the draw budget of ten thousand plain messages holds. A block a search marks (`marks` holds its id) is laid out as written (`Laid::plain`
  is part of the cache key), because a mark counts the characters of the text as written. The renderer cleans the text first and makes no hyperlink, so no OSC 8 reaches the terminal.

  ```rust
  pub struct Row { pub line: Line<'static>, pub join: Join }     // join: how the row continues the one before it (select::Join)
  pub fn render(text: &str, width: usize) -> Vec<Row>;           // Markdown; every row fits `width` cells; at most MAX_ROWS (20,000) rows, then a dim note row
  pub fn render_counted(text: &str, width: usize) -> (Vec<Row>, u64);   // the same, with the steps taken (tests bound the cost per character)
  pub fn render_diff(text: &str, width: usize) -> Vec<Row>;      // a unified diff coloured by line kind, changed words reversed; text that is not a diff is shown raw
  pub fn render_block(block: &Block, width: usize) -> Vec<Row>;  // the body of a block: engine message = Markdown, Diff = diff, Thought/Usage/Gap = dim text, the rest = text as written
  pub fn is_unified_diff(text: &str) -> bool;
  pub fn unified_diff(path: &str, old: &str, new: &str) -> String;     // three lines of context; "" when equal
  ```

  Rules. The text goes through `display_text` first, so no span holds an escape byte or a control character (bidirectional and zero-width marks show as `<U+XXXX>`); a link is its text and its
  address in parentheses (dim), never a hyperlink; raw HTML and entities are shown as written. A source line break stays a line break. Setext headings are not supported (`---` is a rule).
  Emphasis follows the CommonMark delimiter rules in one pass (an unmatched marker stays visible, `snake_case` is not emphasis, `**Note:**text` is not strong). Fences close on the same
  character at least as long as the opener, so a longer fence holds shorter ones. Quotes and lists nest to depth 8; deeper marks are text. A table is an aligned pipe table (copying it gives
  Markdown back) that wraps inside its cells; when the screen is too narrow even for that, the source lines are shown. Spaces in prose collapse as in Markdown; code, user text and tool output
  keep theirs. The rows carry their `Join`, so the transcript can hand `select::TextLines` the rows it draws. Colours: code yellow, quote bar and rule and link address dim, headings bold (the
  first level also underlined), added green, removed red, hunk headers cyan. The wiring decides which blocks call which function; it caches the rows by block revision and width (re-parsing a
  long message at every chunk is linear, not free).
- `mouse`: the hit map every draw fills and the pointer state machine over crossterm mouse events. It
  reads no terminal and draws nothing; the clock is passed in. Whether the terminal reports the mouse at all is the session guard's policy (`session::wants_mouse`: on by
  default, `mouse = false` and `HAMH_NO_MOUSE=1` are the off switches, `auto` is a third value that is not the default), so with no reports this module is idle.

  ```rust
  pub struct BoxId(pub u16); pub struct ListId(pub u16); pub struct PaneId(pub u16);     // the screen numbers its boxes, lists and scrolling panes
  pub enum Target { Box(BoxId), Row { list: ListId, index: usize, id: u64 }, Fold { block: String }, Key(KeyCode), Link(u16),
                    Text(PaneId) /* selectable and scrolls */, Scroll(PaneId), Block /* shields what lies under a popup */ }
  pub struct HitMap { .. }      // Default
  impl HitMap {
      pub fn begin(&mut self, frame: Rect);                                   // start of every draw: forgets the regions
      pub fn push(&mut self, rect: Rect, target: Target, key: &'static str);  // later wins; clipped to the frame; `key` names the key that does the same
      pub fn resized(&mut self);  pub fn is_stale(&self) -> bool;  pub fn epoch(&self) -> u64;   // a resize makes the map stale until the next begin
      pub fn target_at(&self, x: u16, y: u16) -> Option<&Region>;  pub fn pane_at(..) / text_at(..);  pub fn unkeyed(&self) -> Vec<&Region>;
      pub fn pane_rect(&self, pane: PaneId) -> Option<Rect>;                  // the text pane as drawn now; None when the map is stale or has no such pane
  }
  pub fn stable_id(text: &str) -> u64;     // a row's identity from the run id, so a second click counts only on the same run
  pub enum Action { Focus(BoxId), Select { list, index, id }, Open { list, index, id }, ToggleFold { block }, Press(KeyCode), OpenLink(u16),
                    Wheel { pane: Option<PaneId>, lines: i32 }, SelectionStart { pane, x, y }, SelectionExtend { pane, x, y },
                    SelectionEnd { pane, x, y }, Autoscroll { pane, lines }, ClearSelection }
  pub struct WheelSettings { pub step: u16, pub accelerate: bool }   // new(step) clamps to 1..=30; parse("3"); Default step 3
  pub struct Pointer { .. }
  impl Pointer {
      pub fn new(wheel: WheelSettings) -> Self;
      pub fn handle(&mut self, map: &HitMap, ev: MouseEvent, now: Instant) -> Vec<Action>;
      pub fn tick(&mut self, map: &HitMap, now: Instant) -> Vec<Action>;   // repeats the autoscroll of a drag held outside its pane; call it when autoscroll_due says so
      pub fn autoscroll_due(&self, map: &HitMap, now: Instant) -> Option<Instant>;   // when tick next scrolls; None while no drag is held outside its pane
      pub fn has_selection(&self) -> bool;  pub fn clear_selection(&mut self);  pub fn note_key(&mut self);  pub fn reset(&mut self);
  }
  ```

  Rules for the wiring. Every draw calls `begin` and then `push`es a region for every box, run row, fold, key chip, link and text pane, each with the key that does the same (a test asserts `unkeyed()` is empty), and the event loop calls `resized()` on `Event::Resize`, `note_key()` on any key (it also stops the autoscroll repeat of a drag in flight), `reset()` on `FocusLost`, and `clear_selection()` when Esc or a layout change (`Selection::fingerprint`) drops the selection. `Action::Focus` puts the focus in the
  box so the next typed character lands there. `Select`/`Open` act on the row named by `id`. `SelectionStart`/`Extend`/`End` carry screen cells inside the pane; `select::Viewport::pos` turns them into positions, `End` copies. `Autoscroll` and `Wheel` scroll the pane (`pane: None`: the screen's own default scroll). A click is acted on at the press for a box, a row, a chip and a link, and at the release for a fold (so a drag across a fold selects text); a press while text is selected only clears it, except a
  press on selectable text, which may start a new selection. Wired in `app.rs`, `ui.rs` and `mod.rs` (W3, first part): `App` holds `hits: RefCell<HitMap>` and a `Pointer`. `App::draw` calls `begin`, and each screen pushes its regions: the Runs list (a `Scroll` pane over the table and a `Row` for each run the window shows, `ListId(0)`, id `stable_id(run_id)`, `index` its position in the shown list), the filter line (`Box`), the composer, the run screen (the transcript as `Text(PaneId(0))` with a `Fold` for each
  head from `ChatView::fold_heads`, the message box as `Box(BOX_MESSAGE)`, and a `Block` for the result panel, the preview and the search bar) and the key chips of every key line (`push_chips`: a part of ` · `-separated text that starts with `Enter`, `Esc`, `Tab`, `Shift-Tab` or one letter and goes on with words is a `Key`). `handle_mouse_at` runs `Pointer::handle` and acts: `Focus` sets the composer's field or starts typing on the run screen (not while the search bar is open), `Select` and `Open` act on the
  row (`runs_visible()` for the Runs list, the model for the composer's list), `ToggleFold` calls `ChatView::toggle_fold`, `Press` runs the key through `key_at` (so the key table and every guard apply) and `Wheel` moves the Runs selection by the lines the pointer gives, scrolls the conversation by the same lines, and moves the Artifacts selection one row. `key_at` calls `Pointer::note_key`; `Event::Resize` calls `resized` and `Event::FocusLost` calls `focus_lost`. Selection: the transcript is the `Text` pane,
  so a drag in it gives `SelectionStart`, `SelectionExtend` and `SelectionEnd`, which call `ChatView::select_start`, `select_extend` and `selection_text` and the release copies the text through `App::copy_text` (one `Effect::Copy`); `Autoscroll` scrolls the conversation by the lines it carries and `ClearSelection` drops the selection. `TranscriptState` holds the selection (a `select::Selection` with the fingerprint of its rows) and paints it reversed at the end of each draw (`settle_selection`); it builds the
  rows to copy from its own layout, `text_lines(from, to)` (content only, each row with its `Join`: `wrap_text_joined` tells a new line, a wrap at a space and a cut inside a word apart), so the text is exact for rows that are off the screen too. The selection is dropped at the draw after the rows under it changed (the fingerprint is looked at only when the layout count moved, so rows added at the end keep it), when a press starts a new gesture or when the map goes stale on a resize. `Pointer::tick` (the
  autoscroll repeat for a pointer that holds still outside the pane) is called by the event loop ("Autoscroll of a held drag" below). The wheel step is the default 3; `App::set_wheel` takes another, and the config key that sets it is not added yet.
- Autoscroll of a held drag. `Pointer::tick(map, now)` repeats the scroll of a drag held outside its text pane: it returns `Action::Autoscroll { pane, lines }`, then
  `Action::SelectionExtend` at the edge cell of the pane, at most once per `AUTOSCROLL_EVERY` (60 ms; a late call scrolls once, not once per missed interval).
  - `Pointer::autoscroll_due(map, now) -> Option<Instant>` is `Some` while a drag is held outside its pane: the instant of the last scroll plus 60 ms, or `now` when it never scrolled. Both methods ask the private
    `held_outside`, so a deadline that has passed means `tick` scrolls; the property test `the_deadline_and_the_tick_always_agree` pins it.
  - It is `None` when nothing is held, the drag has not left its first cell, a key stopped the repeat (`Pointer::note_key` clears the gesture's `repeat`; the next drag report sets it again), the pointer is inside the pane,
    the map is stale or has another epoch than the pointer (a resize since the press), or the screen has no such text pane. `HitMap::pane_rect(pane)` is the last `Target::Text` region with that id in this draw, `None` for a stale
    map, so the edge is the pane as drawn now, not as it was at the press.
  - `App::autoscroll_at(now)` runs `Pointer::tick` and feeds the actions through `mouse_action`: the wheel path scrolls the conversation by the step and `SelectionExtend` moves the end of the selection to the edge cell, so the
    selection follows the scroll and the release copies every row passed. `App::autoscroll_due(now)` hands the deadline to the loop.
  - `tui::event_loop_timed` asks `App::autoscroll_due` after each draw and sleeps until that instant (`tokio::time::sleep_until`, a branch of the `select!` that is enabled only while it is `Some`), then calls
    `App::autoscroll_at` and draws. `HOLD_FLOOR` (5 ms) is the least time between two such wake-ups. The 1 s poll `App::tick` does not scroll. With no drag held there is no timer but that poll.
- The question that closes under a half-written answer: `close_input` sets `closed_banner` (how it ended, the notice words) when the box holds text; `draw_dashboard` puts it in the typing title
  (`▶ Message to Cline · the question closed (<how>): Enter sends this as a message`); `untaint` (box emptied) and the Ctrl-C clear drop it. Enter then goes through `answer_with`, which finds no open question. Tests `crates/ham/tests/tui_closing_question.rs`.
- Command popup and plain-phrase chip. `App::popup_entries()` is the list: while the person types in the
  run's box, on one line that is a bare `/word` and not closed by Esc at this text, it is `registry::popup(word)`. `typing_key` offers each key to the armed preview, then `chip_key`, then `popup_key`, then the box. `popup_key`: Up, Down
  (`popup_sel`), Tab (`popup_complete`: `/word` and a blank for a command with arguments), Enter when calm and the draft is not already a whole word, Esc (`popup_closed`).
  A submission goes through `offer_chip` before `route_line`: a typed (not tainted) line that is not a command attempt and is `registry::phrase_command(line)` makes `Chip {phrase, argv}` (the command word and the focused run when the entry takes
  one) and sends nothing. `chip_key`: Enter when calm takes the chip and acts on `router::prepare_argv(argv, false)` like a typed line (`outcome_effects`, shared with `route_line`), Esc dismisses it into `chip_dismissed` (cleared when the box is
  emptied) and keeps the draft, any other key drops it. `draw_dashboard` reserves rows for the chip and the popup next to the box (they keep their rows before the result panel) and pushes `Block` regions for them. The popup has no mouse rows yet. Engine text never offers a chip: `offer_chip` is reached only from `send_box`,
  which only a calm Enter key reaches, and no event of the run writes the box; four tests at the end of `tui_popup_chip.rs` bring each phrase as a message, a tool title, a goal-loop note and a question card and pin that the box,
  a typed draft and a waiting chip are untouched (red by three mutants). A new path that puts text into the box (history recall, the editor, a placeholder expansion) must taint the draft.
- `copy`: from text the user chose to the clipboard. It draws nothing and reads no event. The screen calls
  it from the three things a user does to copy: the `y` key, the `/copy` command (both wired) and the release of a mouse selection (`mouse::Action::SelectionEnd`), all wired; nothing else may.

  ```rust
  pub const COPY_LIMIT: usize = 64 * 1024;
  pub fn prepare(text: &str) -> Result<Prepared, CopyError>;   // clean (display_text) -> mask (redact_tokens) -> refuse empty or over the limit; never a prefix
  pub struct Prepared { .. }   // text(), bytes(), masked(); only prepare() makes one
  pub enum CopyError { Empty, TooLarge { bytes, limit }, NoRoute, Write(String) }      // Display words the user sees
  pub fn plan(os: Os, env: &dyn Fn(&str) -> Option<String>, has_program: &dyn Fn(&str) -> bool) -> Vec<Route>;   // plan_here() uses the real machine
  pub enum Route { Tool(Tool), Osc52 }   // Route::label() names it for the notice
  pub trait Runner { fn run(&mut self, argv: &[&str], input: &[u8]) -> io::Result<bool>; }   // SystemRunner { deadline } is the real one
  pub fn copy(p: &Prepared, routes: &[Route], runner: &mut dyn Runner, terminal: &mut dyn Write) -> Result<Outcome, CopyError>;
  pub struct Outcome { pub route: Route, pub bytes: usize, pub masked: bool }   // notice() names the path used
  pub fn last_answer(model: &ChatModel) -> Option<&Block>;  pub fn block_text(b: &Block) -> String;   // y and /copy copy the selected block, by default the last answer
  pub fn osc52(p: &Prepared) -> Vec<u8>;  pub fn base64(bytes: &[u8]) -> String;  pub fn size_text(bytes: usize) -> String;
  ```

  Rules. Over ssh (`SSH_TTY` or `SSH_CONNECTION` set and not empty) the only route is OSC 52, because a tool would fill the clipboard of the machine at the far end; otherwise `pbcopy`
  (macOS), `wl-copy` (Wayland), `xclip` then `xsel` (X11), each only if installed, then OSC 52. A tool runs as an argument vector with the text on stdin, never through a shell. The OSC 52
  bytes are made in this file only, from a `Prepared` text, and written to the terminal (the backend's writer) only on the OSC 52 route; a test fails if any other file of the crate names
  the sequence. The sequence is sent plain, also inside tmux. A terminal may ignore it, so the notice says the terminal decides. `copy` blocks for at most the runner's deadline per tool (2
  s), so the wiring runs the tool routes off the draw path. Masking is a heuristic second line behind the worker's redaction, not a guarantee.
- `export`: `/export` writes a chat as a Markdown file. The command router calls it; the module draws nothing.

  ```rust
  pub struct Meta<'a> { pub run_id: &'a str, pub model: Option<&'a str>, pub exported_at: &'a str }   // exported_at: text in the zone the screen shows
  pub fn to_markdown(model: &ChatModel, meta: &Meta<'_>) -> String;     // title, facts, then every block in order; same chat, same bytes
  pub fn neutralize(text: &str) -> String;                              // quoted engine text: raw HTML, ![ and link definitions made literal; code left alone
  pub fn file_name(run_id: &str, stamp: &str) -> String;                // ham-chat-<run>-<stamp>.md, safe characters only
  pub enum Destination { Dir(PathBuf), File(PathBuf) }
  pub fn resolve(arg: Option<&str>, default_dir: &Path, home: Option<&Path>) -> Destination;   // ~ expanded; a .md or .markdown name is a file
  pub fn write(dest: &Destination, run_id: &str, stamp: &str, content: &str) -> io::Result<PathBuf>;   // never replaces; write_with takes the link function
  pub fn notice(path: &Path, blocks: usize, bytes: usize) -> String;
  ```

  Rules. Headings and facts come from the model's structure (`ChatModel::role_word`), never from block text. A block with no head is quoted with `> ` on every line (empty lines too), so engine text
  cannot make a heading, a rule, a fence or a table outside its quote; a block with a head puts the head in a code span and the body in a fence longer than any backtick run in it (a diff in a
  `diff` fence). Every piece passes `display_text` and `redact_tokens`. "Equal to the transcript" means the text after the quote marks and the backslashes `neutralize` adds, and it is tested against the stored record: `tui_export.rs` starts from chunks as the worker stores them (gzip, `decode_chunk`, the route's JSON read by `parse_page`, `StoredChat`, `compose` with the prompt, a harness notice and a message of the user), exports, and compares every block with the texts of the records, derived from the records and not from the model. The file is
  made as a temp file in the destination directory (mode 0600), hard-linked to the final name (which fails instead of replacing), and the temp name removed; a taken generated name gets
  `-2`, `-3`; a name the user typed that is taken is an error and nothing is written; a missing directory is an error and nothing is created (the screen creates the default download
  directory itself). Without hard links the complete temp file is renamed onto a name found free a moment before. `/export` has no "share" form: the file stays on the user's machine.
- Copy and export commands. The registry words `copy` and `export` are Built;
  `ui_word` handles them. `App::copy_text(text)` is the one place a copy starts: `copy::prepare` makes the `Prepared` text, `Effect::Copy(Prepared)` carries it, and a text that cannot be copied
  (empty, over 64 KiB) says why in the notice row. `/copy` copies `copy::last_answer`; the browse key `y` (key table row, no footer label) copies the selected block (`ChatView::selected_id`),
  else the last answer. `perform` runs `run_copy(prepared, routes, runner, terminal)` in `spawn_blocking` (a tool route can take its 2 s deadline) with `copy::plan_here()`, `SystemRunner` and stdout as the terminal, so
  OSC 52 reaches the terminal only from `copy::copy` on that route; the answer is `Msg::Copied(Ok(notice) | Err(words))`, shown in the notice row. `/export [file]` builds the Markdown on the screen from
  `ChatView::conversation()` (the model's own blocks, without the cards) with `export::to_markdown` and `Meta {run id, model name, UTC time and the local clock}`, and sends `Effect::Export {run_id, arg, stamp,
  content, blocks}`; `run_export(dir, arg, ...)` makes the download directory when no name was typed, resolves the place with `export::resolve`, writes with `export::write` (never replaces) and answers
  `Msg::Exported`. A tainted draft (a paste) never exports: the panel says so.
- `select`: the geometry of an in-app selection over transcript text. It reads no terminal and
  draws only through `Selection::paint` onto a `Buffer` it is handed. The names the wiring may rely on:

  ```rust
  pub fn cell_width(c: char) -> usize;  pub fn text_cells(s: &str) -> usize;     // 0, 1 or 2 cells; the cells a text takes
  pub struct Pos { pub line: usize, pub col: usize }                              // absolute transcript line, display cell; Ord
  pub enum Join { Hard, Space, Cut }      // how a row continues the one before it: new line, soft wrap at a space, cut inside a word
  pub struct TextLines { .. }             // the laid-out rows, content only (no gutter, role word or scrollbar)
  impl TextLines {
      pub fn new(base: usize) -> Self;  pub fn push(&mut self, text: impl Into<String>, join: Join);
      pub fn from_rows(base: usize, rows: impl IntoIterator<Item = (S, Join)>) -> Self;   // the exact path: the transcript layout's own rows
      pub fn from_buffer(buf: &Buffer, content: Rect, first_line: usize) -> Self;         // fallback: the rows on screen, every join Hard
      pub fn base(&self) -> usize;  pub fn len(&self) -> usize;  pub fn is_empty(&self) -> bool;
      pub fn line(&self, n: usize) -> Option<&str>;  pub fn join(&self, n: usize) -> Option<Join>;  pub fn all_text(&self) -> String;
      pub fn fingerprint(&self, from: usize, to: usize) -> u64;
  }
  pub struct Selection { .. }             // Copy; both end cells are in it
  impl Selection {
      pub fn new(anchor: Pos) -> Self;  pub fn from_points(a: Pos, b: Pos) -> Self;  pub fn extend(&mut self, to: Pos);
      pub fn anchor(&self) -> Pos;  pub fn head(&self) -> Pos;  pub fn bounds(&self) -> (Pos, Pos);  pub fn is_single_cell(&self) -> bool;
      pub fn text(&self, lines: &TextLines) -> Option<String>;          // None when a row it needs is not held; never a shorter text
      pub fn cells_on(&self, lines: &TextLines, line: usize) -> Option<Range<usize>>;
      pub fn paint(&self, lines: &TextLines, buf: &mut Buffer, content: Rect, first_line: usize);   // REVERSED on the selected cells only
      pub fn fingerprint(&self, lines: &TextLines) -> u64;
  }
  pub struct Viewport { pub content: Rect, pub first_line: usize }   // content: the text columns (right of the gutter, left of the scrollbar)
  impl Viewport { pub fn pos(&self, x: u16, y: u16) -> Pos }          // clamps a cell outside the rectangle to its nearest edge
  ```

  Rules. A `Join` describes how a row continues the row before it, so `text()` puts back `"\n"`, `" "` or nothing: a wrapped paragraph copies as it was
  written and a URL cut at the width copies without a space. A wide character, a letter with its combining marks and an emoji joined by zero-width
  joiners are one unit, taken whole when either cell is in range. A folded block contributes the rows on screen, so a selection across a fold copies the head
  row and not the hidden body (a whole block is `copy::block_text`). The screen keeps `Selection::fingerprint` from when the selection was made or last
  extended and drops the selection when the fingerprint of the same rows differs (a resize, a fold opened above it, a rewritten row); rows added at the end do not
  change it. `from_buffer` also reads a right-aligned tool status that sits in the content columns, so the exact path (the transcript handing over its layout
  rows with their joins) is the one the wiring should build.
- Confirmation notices: browse-mode `c` shows the notice "press c again to cancel run <id>" (`ASK_CANCEL`) and `f` shows "press f again to finish run <id> with what exists" (`ASK_FINISH`). `browse_key` takes both confirm flags (`confirm_cancel`, `confirm_finish`) at every key. Any key other than the one that was asked for withdraws the question and clears its notice: `clear_notice_starting(prefix)` drops the notice only while it still starts with that text, so a notice that something else wrote since stays. The file has five tests, among them "`c` then `f` replaces the cancel question with the finish question" and "the second `c` still cancels".
- The Artifacts screen's table (`draw_artifacts` in `ui.rs`; BATR1 defect D5; tests `crates/ham/tests/tui_artifacts_columns.rs`, `tui_w2.rs`): the ARTIFACT column is as wide as the longest artifact id and never narrower than 28 columns (`artifact_columns`; the packager's `evidence-` and 20 hex digits are 29), so an id is whole at 80 columns and wider. When the terminal has not the room the FILE column gives way first (down to 4 columns), and only then an id is cut, by `cut_cols`, with a `…` as its last column. KIND, SIZE and SHA-256 keep their widths, so they stay under their headings.
- Rules for every change here:
  - The app checks its key table before the editor sees a key. Every key the editor crate would take for itself and the
    app uses (Ctrl-J, Ctrl-R, Ctrl-U, Ctrl-C) has a row in the table.
  - A paste is text, never keys: bracketed paste on, `Event::Paste` inserted whole into the focused box, ignored with a notice elsewhere.
  - A change to the terminal modes goes through `session.rs`; a child process that needs the tty goes through `suspend_for`.
  - A new event that can mean "the terminal lost part of its picture" calls `App::request_repaint`; a draw that clears the screen by itself goes through `clear_screen`, never `Terminal::clear` (ratatui 0.30 asks the terminal for its cursor first).
  - New behaviour goes in a module with its own tests; a change to `app.rs`, `ui.rs` or `mod.rs` only connects modules, except where this
    file says a seam lives there.

## Work Guidance

- Write the failing test first and keep its red output with the change's records. Terminal behaviour is proved on the compiled
  binary in a pseudo-terminal (`crates/ham-e2e`, `HamPty` and the vt100 screen), not only through `TestBackend`.
- Drawing changes update the insta snapshots in `crates/ham/tests/snapshots`; review each changed snapshot before accepting it.

## Verification

```bash
cargo nextest run -p ham --test tui_version_tag --test tui --test tui_input --test tui_links --test tui_model --test tui_w2 --test tui_direct --test tui_chat_search --test tui_history --test tui_send --test tui_engines --test tui_openbox --test tui_history_screen --test tui_matchrow --test tui_autoscroll --test tui_repaint --test tui_repaint_keys --test tui_continue --test tui_repaint_cost --test tui_session --test tui_runs_names --test tui_runs_scroll --test tui_chat_name_row --test tui_name_poll --test tui_openbox_names --test tui_phase_shown --test tui_tags --test tui_tags_mouse --test tui_tags_message --test tui_chat_tags_row --test runs_note_tags --test cli_runs_tags --test tui_tags_scroll
python3 scripts/dox_lint.py
```

## Child DOX Index

None.

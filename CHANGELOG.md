# Changelog

This file records notable features, architecture updates, and fixes for Hyperscale Agent Meta Harness, starting with version 4.29, the first public release. The layout follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), with Added, Changed, Deprecated, Removed and Fixed groups appearing only when they contain an entry. Versions appear newest first, with `## [Unreleased]` at the top for changes without a version and version headings dated when the version number increases.

The root `VERSION` file supplies the version shown on every terminal screen as `ham vN.NN | backend vN.NN`, where N is the release line's major number and NN is a two-digit counter. Commands and variables appear under their current names.

## [Unreleased]

## [5.33] - 2026-11-26

### Changed

- Where it is cheaper, the session program makes the pack it keeps of a run's commits from the copy it has just saved, which takes a fraction of the time after many commits that git has not packed.
- The Runs screen shows the newest runs after the first answer and loads the older runs behind them. The table's title reads `runs · loading older runs` meanwhile. `R` and the tag explorer's `d` wait for the whole list.

## [5.32] - 2026-11-25

### Changed

- `HAMH_TEST_STORAGE_REFUSAL=on` is now a setting of a test stage that `./setup.sh --update` passes on; `HAMH_TEST_STORAGE_REFUSAL=off ./setup.sh --update` takes it off.

## [5.31] - 2026-11-24

### Fixed

- A slow download of the zig toolchain no longer holds a setup or an update for minutes: the build takes it from another source of the same archive.
- A read of a stopped session's storage that runs out of time is now always reported as not finished within its time limit. Before, `ham` sometimes said that the storage could not be read back whole.

## [5.30] - 2026-11-24

### Changed

- Saves of a run's work get back to their usual pace sooner after many commits in a row.

## [5.29] - 2026-11-24

### Changed

- The worker's log line for a copy that is taking long states how long the look has taken, the limit that applied, the set limit and how long the last stored copy took. It named only the set limit before.

### Fixed

- The worker asks the storage under the work tree whether it still answers every 20 seconds from the start of a copy of the work, so a copy that hangs is reported as not saved sooner when earlier copies took long. The copy made when a session is asked to end says it is taking long by the same rule as any other copy.
- The worker now writes its save line for each attempt to save the copy of the work at the end of a session, stored or failed, as it does for its other saves. The line names it as the final copy.

## [5.28] - 2026-11-23

### Changed

- A storage notice of a later session that had saved no copy of its own names the copy the session started from. `ham pull <runId> --before-refusal <notice id>` downloads it.
- A save of the run's work no longer reads every commit the run made from the session's storage again: the worker packs them once on its own disk. The worker logs one line for each save that stored a copy or failed.

### Fixed

- The delivered zip no longer depends on the time zone of the machine that builds it. Every entry shows the head commit time in UTC.

## [5.27] - 2026-11-22

### Added

- `ham watch --json` carries the notices that a session's storage refused a write in the new member `storageNotices`.

### Changed

- The record of a storage notice says when the refused write was the marker of a session's start.

## [5.26] - 2026-11-21

### Added

- A run says when its session's storage refuses writes while its files leave room. The notice gives the size of the files and what such a refusal has meant on this storage, and comes once per stretch of refusals. The run goes on.
- `ham pull <runId> --before-refusal <notice id>` downloads the last copy saved before ham first saw the refusal. `ham pull <runId> --snapshot` downloads the newest copy.
- The card of a run that ended has `Storage` rows when the run saw a refusal.
- A stage of an account entry with the role `test` that is deployed with `HAMH_TEST_STORAGE_REFUSAL=on` answers one test write of a session with a refusal, so that the notice can be tested. A stage of any other entry ignores the switch.
- A run prints two lines at the start of a session on the microVM when its workspace folder holds more than 200 MiB. The first gives the folder's size and the sizes at which the measurements saw refused writes on this storage. The second names `--compute instances` for a new run. The run goes on.
- `ham watch --json` carries the worker's disk notices in the new member `diskNotices`.

### Fixed

- `ham watch` prints the worker's disk notices, such as the notice that the workspace folder is nearly full. It printed none of them before. The run screen shows each notice without the words “Notice from the run”.
- A long run no longer leaves one ended process entry in its session for every commit.
- A run no longer ends as "not what the run started with" when the session storage refused the worker's write of the git settings at the end of the run. The worker repeats the write.

## [5.25] - 2026-11-20

### Added

- The terminal app shows a notice beside the version tag when a newer release is available. A click opens the repository's page in the browser, and the app stays open.
- `ui.update_notice`, on by default, sets whether the terminal app looks for a newer release. `ham config set ui.update_notice off` switches the look off.
- A follow-up of a run that reported done starts without that run's lifecycle records. Before the first prompt the harness takes the goal card, the run log, the red records and the research notes that runs wrote under `.ham` out of the work tree in a commit of its own. They stay in the history and in the earlier run's zip.
- `run.remove_earlier_records`, on by default, sets whether such a follow-up starts without the earlier records. `ham config set run.remove_earlier_records off` leaves them in place. The setting is sent with a follow-up only.
- The start request has the optional member `removeEarlierRecords`. The service writes `true` for a follow-up of a run that reported done, and a request without the member behaves as before. The protocol version is 1.64.0.

### Changed

- A saved copy of a run's work holds what the run changed since it started, not the repository's history. A later session of a run, a delivery from a saved copy and a download read the repository the run started from first.
- `ham pull <runId> --snapshot` keeps such a copy as two files, the copy and the repository the run started from, and prints the git command that puts them together. The command puts the copy on a branch of its own, `ham-snapshot`.
- A `ham` of an earlier release is told to run `ham update` when it asks for such a copy. The work stays saved.
- A stage that has saved such copies cannot go back to an earlier release and still restore or deliver from them.
- A deploy changes the delivery stack before the runtimes stack.
- The worker sends a copy to the store as a stream and hashes it once, so storing a copy no longer holds up the worker's other work.
- A request that stores or fetches a copy is given up when nothing moved for 30 seconds and is sent again. The last copy of a session keeps its two minutes: a request that still moves is no longer cut off after 30 seconds.
- Making copies takes less processor time. The worker's private git repositories pack with one thread and clean up only after they have grown.
- The worker reads the work tree once before the engine starts. The first copy of a session no longer reads every file in front of a tool call.
- A follow-up of a run that did not report done, and `ham resume`, keep the earlier records. So does a follow-up when the service cannot read how the earlier run ended.
- A file under `.ham/lifecycle` or `.ham/research` that you committed yourself stays, also when the engine changed, renamed or moved it. So do the records in a folder that `.ham/records` names.
- A follow-up of a run that reported done no longer takes its test command for the clean-checkout report from the earlier run's log. `.ham/test-command` names a command for every run.
- Messages that ask for an update name `ham update`.
- The publish tool refuses a public release that does not carry the tag `v` and the version of the tree.

### Fixed

- The detail of a finished run reads as sentences. A period is no longer followed by a semicolon. After a sentence, the words about the pull request and the test report begin with a capital letter, for example `No pull request: Cline left the repository unchanged`. The reason of a blocked run joins its findings the same way.
- A copy no longer fails on a lock file that an earlier copy left behind.
- A run that rewrites a large file between tool calls no longer fills the worker's disk with old versions of it.
- Stopping a copy also stops the helper processes that git started for it.

## [5.24] - 2026-11-17

### Added

- GPT-6.2 Sol and GPT-6.2 Astra are selectable for both engines, Cline and OpenCode. On Cline both reason at xhigh effort by default.
- On a short terminal the Model box of the New run screen shows the models around the picked one, and its title says which rows show.
- When a stage refuses a model that this `ham` lists, `ham` names the backend's version and says to run `ham update` or to pick a model that `ham models --server` lists.
- `ham models --server` adds a note when the stage holds another short name for a model than this `ham`.

### Changed

- `sol` and `astra` now select GPT-6.2 Sol and GPT-6.2 Astra. GPT-6.1 Sol and GPT-6 Astra are `sol-6.1` and `astra-6` and stay selectable by their full ids. A default saved with `ham config set default_model` holds a full id and keeps its model. The default model stays Claude Sonnet 5.5.
- `ham models` prints a note for GPT-6.1 Sol and for GPT-6 Astra that says which name selects them.
- When a model route fails, a run on an OpenAI model tries at most four other OpenAI routes and then routes of other providers.
- The cost of a run on GPT-6.2 Sol or GPT-6.2 Astra reads `unknown`: Amazon Bedrock has published no price for them.

### Fixed

- Cline runs on GPT-6 Astra and GPT-6.1 Sol now shorten a long conversation before Bedrock's input limit.

## [5.23] - 2026-11-17

### Added

- When a session of a run is stopped from outside or stops answering, the service reads that session's own storage back before the next session starts. The next session starts from those files when the service's checks leave no doubt. Otherwise it starts from the last saved copy.
- `ham pull <runId> --read-back <n>` downloads the files the service read back from session n, whether the run went on from them or not. `/pull --read-back <n>` does the same for the run on screen.
- `ham pull <runId> --snapshot` downloads the files read back while they are the copy the run goes on from.
- The notice of a lost session says which copy the next session starts from. When files were read back and not used, it says why and names the command that downloads them.
- A run that ends on a lost session delivers the files read back when the service chose them.
- A run that ends blocked while its newest copy is files read back names that copy and its command.
- `run.recover_after_stop`, on by default, sets whether the service reads a stopped session's storage back. `ham config set run.recover_after_stop off` switches the read and the early end of the wait off. A stage of an older release refuses a start with it off, and `ham` names the key.
- `ham cancel <runId> --discard` stops a run now and delivers nothing. Saved copies stay available through `ham pull <runId> --snapshot`.
- `ham cancel` says at once that the cancel is recorded and what follows, and names `ham pull <runId> --snapshot`. `ham watch` prints one line, the run screen shows the same line in the conversation, and `ham runs` and the Runs screen show `cancel recorded`.
- `ham runs` and the Runs screen show `session silent since <time>` when the service has missed two of a session's heartbeats and half a minute more. `ham runs --json` carries `cancelRequestedAt` and `sessionSilentSince`.
- `ham watch --json` carries `cancelRequestedAt` and `endRequestedAt`.
- `ham watch` and `ham pull` show the `Outside` row of the delivery card: the tokens the engine used outside the prompts that answered. `GET /v1/runs/{runId}` carries the sum as `cost.answeredTokens`.
- A new session of a run is given the messages you sent earlier in the run as your own words, in the order you sent them. The service supplies them from its record of the run's messages. This covers a session after the 8-hour limit, a session that follows a lost one and a fresh session. The service carries at most 16,384 bytes of message text and at most 100 messages. It leaves out the oldest messages first, and the new session is told how many were left out. These messages do not include text that you typed with an answer to a question.
- A message that `ham` shows as `may have reached Cline` is given to the new session after the delivered messages, with a line that says the earlier session may or may not have read it.
- When the service cannot supply the messages, the new session receives the record the earlier session kept, quoted as data, and its first prompt says so.
- A run says once when OpenCode does not take the command time limit in a session, and goes on.

### Changed

- A stop that the worker hears of ends the wait for the session within seconds instead of after 600 seconds. A session on Instances compute gets no read and keeps the 600 second wait.
- After a lost session the service waits 75 seconds before it decides which copy the next session starts from.
- A cancelled run whose session does not answer delivers the files the service read back from that session's storage when its checks leave no doubt, and otherwise the last saved copy. `ham cancel`, `ham watch` and the guides say this rule in the same words, and the card's `Lost` row names the copy the run ended with.
- The notice of a silent session says that the service reads the session's storage back when the service does so for the run, and no longer says that file changes are lost. A run with `run.recover_after_stop` off, a run on Instances compute and a run on a stage of an older release keep the earlier notice.
- After a recorded cancel, the notice for a silent session says that no new session starts. It says that the run ends when the service calls the session lost. If the session was asked to end, the sentence about its open question then says that nobody asks again.
- A session that starts from files read back is told so in its first words. Before, every later session was told that it starts from the last saved copy.
- A cancel that no session answers ends with the label of a run the harness ended, and the reason says when you stopped it. The label of such a run is the same in the card, the run list and the events.
- The service adds up the tokens of the `Outside` row from what each session reports, so the row is right for a run that lost a session.
- A run whose engine stopped a prompt while it repeated a wait on a long command is resumed without using up its automatic resume, up to three times in a session. A repeat counts as such a wait when the earlier identical call blocked for at least the larger of 5 seconds and the smaller of half of the wait bound and 50 seconds.
- Cline no longer counts a bounded wait that is sent again toward its guard on identical tool calls. A repeat is left out when the earlier identical call blocked for at least the larger of 5 seconds and the smaller of half of the wait bound and 50 seconds. Calls that return quickly count as before, and the repeats that are left out may take two hours in all.
- Every launch of an engine sets `HAMH_WAIT_S`, the bound of one wait on a background job: nine tenths of the run's command time limit, at most 240 seconds.
- The lifecycle pack starts a long background run in a process group of its own and writes the exit code into the end file after the command has returned. The wait is bounded by `HAMH_WAIT_S` seconds, with 100 as the fallback. It also ends when the job's process is gone, and then prints `JOB-GONE` if no exit code was written. A job that was ended from outside no longer reads as a success.
- The lifecycle pack and the default rules file no longer ask for a new number on every repeat of the wait. `ham rules init` updates a rules file that was never edited and leaves an edited file alone.

### Fixed

- `ham cancel` now sticks when the run's session is gone. The service records the cancel and starts no further session. It delivers the copy it chose, or ends the run `blocked` with the cancel as its reason when there is none.
- A cancel while a run delivers no longer discards the delivery.
- A cancel that arrives while the engine works through the goal loop's follow-up prompts ends the run as your stop also when it comes in the moment the harness asks its modules about a round. Before, such a run could end with the label of its last answered prompt.
- The stored conversation of a run with several sessions is read from every session. The notice that its end may be missing follows the run's last session.
- A message sent while Cline works through the goal loop's follow-up prompts no longer restarts Cline. Cline takes the message after the round it is working on. A message that Cline took behind the last round of the loop is sent once more when Cline is idle, so it is read. A message that can no longer go out shows the notice "not read; the run ended before this message was reached".
- A `/now` message that finds no turn to interrupt shows the notice "could not interrupt; delivered after this turn". This covers the goal loop's follow-up prompts, the time between two turns, a wait before a retry and the start of a session.
- The goal loop's goal text and its follow-up prompts name this task's goal card. A goal card that an earlier request left in the repository is no longer named.
- OpenCode runs get the command time limit: a shell command that names no limit runs up to `commands.timeout_seconds` (600 seconds by default), not 120 seconds. A limit the model names for a command is kept.
- A command that OpenCode stopped at its time limit shows as timed out in the run's records.
- Git commands work in a scratch worktree under the session's temp folder, under both engines. The review round can compare the change with the starting commit there.

## [5.22] - 2026-11-16

### Added

- `run.delivery_note`, on by default, sets whether the harness tells the engine how the run delivers. `ham config set run.delivery_note off` leaves the block out, so the engine reads your prompt first.
- A run that `ham resume` or `/retry` starts keeps the delivery-note setting of the run it comes from. A follow-up with `ham continue` uses the setting your config has when you start it.
- A stage of an older release refuses a start with `run.delivery_note` off. `ham` names the key and the two ways out: `ham config unset run.delivery_note`, or an update of the stage.
- The delivered zip says what the engine was told about the delivery: `EVIDENCE.md` quotes the block, and `evidence/direct.json` holds it as `deliveryNote`. A session that ended before its first prompt records no block.
- For a zip-only run whose zip holds a workflow file or a root `buildspec.yml`, the Detail row of `EVIDENCE.md` ends with the sentence the run card shows, and `evidence/direct.json` in the zip holds it as `zipHolds`.
- When a session is asked to end and its copy cannot be saved, `ham watch` and the run screen say whether the last saved copy is from before that request or that the run has no saved copy. The run screen says when a later try saved a copy after the request.
- The run screen's title and status block add `session asked to end`, `session silent` or `waiting for session <n>` after `active` while no session answers.
- `ham watch --json` includes the notices about copies since the newest stored copy in `copyNotices`, with `at`, `text` and `endRequest` members. A view without such notices has no `copyNotices`.
- The run screen's delivery card has an `Outside` row that counts engine tokens used outside prompts that answered, including goal-check rounds and failed prompts. It appears after the stored conversation is read in the background, later for long runs. `ham watch` and `ham pull` do not show this row.
- The screen of a paused or stalled run names `R` and says what it resumes: that run, a count of runs, or all paused and stalled runs.

### Changed

- The worker reads how a run delivers from the run's stored request, and the service no longer writes it into the request of every session. When the worker image alone is rolled back, a run that keeps its defaults starts again, and the earlier image tells the engine nothing about the delivery. A run with a setting the earlier image does not know, such as `run.delivery_note` off, does not start there. One deploy of the earlier release, which rolls the service and the worker image back together, stays the supported way back.
- The guide says that the block on how the run delivers also comes before the first prompt of a fresh chat inside a session, and that a prompt that begins with a slash follows the block like any other prompt.
- The clean-checkout test report no longer runs again a test command that the run log recorded as failed when a later logged run of the same suite passed. The Tests line then gives the passing count, and `evidence/test-report.json` names the command that was not run again.
- For the test report, the same suite means the same test runner with the same words and options. Only options that neither select tests nor can fail a run whose tests pass, such as `pytest -q` or `go test -count=1`, are left out of the comparison. A later run that left tests out, or that dropped a strict option such as `cargo test --locked` or `pytest -W error`, does not stand for the failed command.
- The lifecycle pack starts a long background run so that its shell writes the exit code into an end file, and waits for that file with a command bounded at 100 seconds, under the default command limit of both engines. Each repeat of the wait carries the next number, and the `lifecycle-check` skill uses the same two commands for the first call of the scanner kit.
- The default rules file tells a session to wait for a long build and for the first call of the scanner kit the same way: an end file, a command bounded at 100 seconds, and a new number for every repeat. Before, it asked for one blocking command. `ham rules init` updates a rules file that was never edited and leaves an edited file alone.
- The silent-session notice begins "No event from the run's session since <time>". Before, it began "No word from the run's session since". The guide distinguishes the newest event `ham` read from the last heartbeat the service received.
- An open question whose session is asked to end shows "<engine> needs you · its session was asked to end" instead of minutes left. `ham watch` and the status block say that an ended session cannot receive an answer and whether a new session will ask again.
- Questions are numbered across a run's sessions. Before, each session's first question was numbered 1.
- The status block of a zip-only run reads "PR none: this run delivers a zip" while the run works. Once a run has ended without a pull request it reads "PR none: this run delivered a zip" or "PR none". Before, both read "PR none yet".
- A run that has ended offers no cancel in the terminal app: the box title, the key lines and the command list leave it out, and `c` says that the run has ended.
- `ham pr` says that a paused or stalled run that delivered a zip has no pull request by design, and that a run that ended blocked or was cleared opens none.
- The guide explains that Tokens counts engine tokens across all sessions, how much of a run's total time is work time, whose allowlist entries `ham doctor` lists with several backends, and that a "did not start" card is kept only while `ham` stays open.

### Fixed

- In the terminal app, a second `R` on a run's screen runs the previewed `/resume --all`. Before, the second key press did nothing.
- The offer "Retry blocked run?" wraps its sentences instead of cutting them. Its line "Enter starts it · Esc cancels" stays in view on a short screen.
- The typing key line of a blocked run names the retry in full at 80 columns: "Esc browse (r retry)".
- The notice of a retry or a follow-up that did not start says "your text is kept" only when the box holds a text.
- `ham runs --json` writes a finished run's `phase` as `delivered`, matching `ham runs` and `ham watch --json`. Before, it wrote `delivering` with `status` set to `complete`.
- `ham doctor` indents repository allowlist entries under the `allowlist` line. Before, they appeared after the last check line.
- While a copy notice stands, `ham watch` does not print a `saved:` line for a copy from before the notice. Before, that line could appear up to a minute later and suggest a newer copy. After a notice that is no request to end, it says once that the last saved copy is from before the notice and names the command that downloads it.
- The card's `Lost` row and the run screen's past-tense sentence show both the session's last heartbeat and when the service called it lost. Before, they showed only the service's decision time.
- The run screen's stored conversation no longer ends with "[the last session's conversation was not closed: its last lines may be missing]" when the run's last session closed its conversation.
- `ham watch` prints "The run's session answers again." whenever a session starts after a lost session it reported. Before, it printed this only when one read included both the lost session and the new session.
- The zip of a follow-up of a run that was not delivered names the same workflow files in `EVIDENCE.md` as the run card.
- The service tries once more to write the event that says a session was lost when the first write fails. The run goes on into its next session either way.

## [5.21] - 2026-11-15

### Added

- `run.copy_every_seconds` sets the seconds between the worker's looks for changed files of a run, from 15 to 3,600 with a default of 15. `run.copy_every_minutes` keeps working. `ham` refuses a config that sets both keys and says to unset one of them.
- `ham config show` and the Settings screen show the interval in force on the row of `run.copy_every_seconds` while `run.copy_every_minutes` sets it, for example `600 seconds (run.copy_every_minutes)`.
- At the start of every session, the harness tells the engine how the run delivers, in a short block that begins `How this run delivers (written by the harness):`. The block comes before your prompt, which still reaches the engine as typed. The run's target sets what the block says, and the block has no setting of its own. A follow-up with `ham continue`, a run that `ham resume` starts and a run started again with `/retry` get the block for their own target. A later session of a run gets the block again, before the words on the earlier session.
- A `--s3-only` run is told that it delivers only a zip, that a zip takes files under `.github/workflows/` and a root `buildspec.yml`, and to name each such file it adds in its last message. A run that targets a repository is told which repository gets the pull request and that the delivery refuses a run that adds, changes or removes such a file. It is told to hold such a change back and name the file in its last message. Before, the engine had to guess which kind of run it was in, and a zip-only run could leave out a workflow file that its prompt asked for.

### Changed

- While the engine works, the worker looks for changed files every 15 seconds and saves a copy of the run's work when it finds a change. Before, it looked every minute. A look that finds no change stores nothing. A session that is stopped or dies loses about 15 seconds of file changes plus the time one copy takes, or more for a repository whose copies take long. A copy every 15 seconds does not save every write.
- After a slow copy, the next look waits nine times as long as git worked on the copy, five minutes at most, under every interval. A copy that failed is spaced the same way.
- `run.copy_every_minutes = 1` sends an interval of 60 seconds with the run. Before, one minute was the default and sent nothing.
- On an updated stage, a run started by an older `ham` that sets no interval is copied every 15 seconds.
- A stage of an older release has a longer timer of its own. A run that sets no interval, or sets 15, starts there and is copied on that timer until the stage is updated. It refuses an interval of 16 to 59 seconds: `ham` says `this stage predates copy intervals under one minute` and names the ways out.
- When a stage does not know the copy interval of a run, `ham` names the key your config sets, `run.copy_every_seconds` or `run.copy_every_minutes`.
- The backend takes a copy interval in `snapshotEveryS` of the start request from 15 seconds. Before, the lowest value was 60 seconds.
- The service writes how the run delivers into the request of every session, and a worker image of an earlier version refuses that request. No run starts when the worker image alone is rolled back. Roll the service and the worker image back together.
- The delivery rule of the default rules file ends with a new sentence: when the first words of the run are written by the harness, they say which kind of delivery the run has. `ham rules init` updates a rules file that was never edited and leaves an edited file alone.
- The guides state the bound of lost work as about 15 seconds of file changes, or the interval you set, plus the time one copy takes, or more for a repository whose copies take long. Every place that states the bound names the interval a user sets, and the guides call the timer's default a default.
- The run screen of `ham` redraws faster for a run with thousands of agents. A card or an agent row that did not change is not built again.

### Fixed

- The copies of a run that continues in a new session go on counting from the larger of the recorded number and the number of the copy the session started from. Before, after a failed upload two different copies could get the same number.
- `ham watch` without a terminal no longer shows a warning about a copy of an earlier session once that session is lost or a later session starts. Every notice stays in the run's notices and its stored conversation.
- The first prompt of a session that continues a run says that the earlier session reached its time limit or was lost. It says that the session starts from the last saved copy and that changes made after that copy may be missing. Before, it told the engine that the files are exactly what the earlier session left.
- A run goes on into its next session when the service has decided to start one and the event that announces the lost session cannot be written. Before, such a run could end.
- A copy that could not be stored after its git work ended no longer doubles the time limit of the git work of later copies. Before, four such failures during a storage outage raised that limit from five minutes to one hour.
- The Troubleshooting table of the user guide has rows for a run that ends `blocked` with exit 12 and no zip, for `ham watch` ending with `ended early`, for a run name that stays dim, for a run that shows no tags, and for the `WARN catalog` and `FAIL catalog` lines of `ham doctor`.

## [5.20] - 2026-11-14

### Added

- `run.copy_every_minutes` sets the minutes between copies of a run's work while the engine works, from 1 to 60 with a default of 1. Copies after a tool call and when the engine stops stay unchanged. No value turns copies off.
- A run started by `ham resume` or `/retry` keeps the copy interval of the run it comes from.
- A stage of an older release refuses a copy interval other than one minute. `ham` names `run.copy_every_minutes` and says to unset the key or update the stage. A value of 1 starts on any stage.
- `./setup.sh --update --allow-active-runs --allow-new-runs` keeps the stage starting runs while the update deploys. The update keeps the worker runtime and stops with nothing deployed if it would change that runtime. Setup says that the stage kept starting runs.
- The plan of `./setup.sh --update --allow-active-runs` says whether the stage starts new runs while the update deploys.
- The worker of a session that receives a termination signal while the engine works saves a copy of the work at once. If that copy fails, the run is told that the session was asked to end, and the worker tries again every 5 seconds while the session's machine lives.
- The run says when a copy of the work takes long and whether reading the work tree or storing the copy is slow. It says when that copy could not be saved. `ham watch` without a terminal prints each of these sentences once.
- The run screen and `ham watch` say when a run's session goes silent: since when nothing was heard and what the service does next.
- When the service starts a new session for a lost one, the run screen, `ham watch` and the card name the lost session, the new session and the copy it started from. `ham watch --json` carries `lostSessions`, and `silentSince` while a session is silent.
- `ham watch` without a terminal prints a `saved:` line when a copy of the run's work was saved, at most once a minute. Its `--json` view carries the newest copy as `savedCopy`.
- `ham watch` without a terminal prints the card of a blocked run with its `Saved` and `Tokens` lines.

### Changed

- The stage starts runs again as soon as the deploy of an update has finished, while the build goes on to make the client. `./setup.sh --update` prints `the deploy has finished: the stage starts runs again while the build goes on`. When it stops following the build, it says whether the stage already starts runs again or when it will.
- The stage opens before the update has filled the Go module store. A run that needs a Go module the release adds finds it once `./setup.sh --update` has ended.
- The plan of an update that deploys shows `label: the backend reports vA and will report vB after the deploy` when the version changes. It says when the number goes down.
- An update that deploys nothing says `the stage already runs this source tree`. It names the commit of the stage's record when the stage records another commit with the same source tree.
- `ham update` does not wait for your active runs before an update that deploys nothing on the stage. With an active run, it prints a line and asks setup for its plan first.
- `ham watch --json` writes the phase of a delivered run as `delivered`, as its line output does.
- Every wait for a copy of a run's work has a limit. A copy that does not return is given up, the run is told and the next minute tries again. Reading the work tree for one copy is given up after five minutes, or after ten times the time of the last stored copy when that is longer.
- A copy that could not be saved is tried again within a minute. The extra wait after a slow copy is at most five minutes, also with a longer copy interval.
- A copy names the last whole second before its files were read. `ham pull <run id> --snapshot` never names a time later than its contents.
- The copies of a run are numbered within the run. A run that continues in a new session goes on counting.
- A new session saves its first copy when a file changes. The copy it started from is saved already.
- The run screen, `ham watch` and the run list no longer offer a question that was open when its session was lost. The new session asks again when it needs your answer.
- The guides state one bound for lost work in every place: one minute of file changes plus the time one copy takes, and when it is more. They promise no copy at once for a session that is stopped from outside.

### Fixed

- The guides say what the service does with a lost session. After 600 seconds without a heartbeat it starts a new session from the last saved copy, and after 3 lost sessions in a row the run ends `stalled` with the last saved copy delivered. Before, the guides said that a missed heartbeat ends the run.
- The clean-checkout test report gives a result for a language whose only logged test commands name single test files, tests or filters. It runs the detected whole-suite command for that language in place of them.
- The `Tests` line and `EVIDENCE.md` say when the report ran a command in place of logged commands or left a logged command out, and why.
- The `Tests` line for a failed install names the command that did not run, also when no test command ran. When two installs fail, the report counts each skipped test command with the install that stopped it.
- The clean-checkout test report counts a command that chains two test suites for both languages. `dotnet test`, `rspec` and `phpunit` no longer hide the tests of other languages.
- A command the test report added that is stopped at its time limit is listed as not run. The report stays `ran`.
- `ham pull <run id> --snapshot` keeps a downloaded file that holds another copy. The second file is named `<run id>-snapshot-<n>-<first 12 characters of the commit>.bundle`.
- The stored conversation of a run that went on after a lost session no longer ends with a notice about an abnormal end. A line where the earlier session stopped says that its last lines may be missing.
- The detail of a run that ended after lost sessions names the time of the copy it delivers, also when the last session saved no copy of its own.
- `ham update --check` leaves nothing in the download folder. An update that is up to date, refused or not confirmed leaves nothing either. `ham update` and `ham update --check` remove the hidden folders that an interrupted `ham update` left there.
- `ham cancel` of a run without a live session says whether the run saved a copy of its work. It names `ham pull <run id> --snapshot` for a run that saved one.
- A build that deployed nothing and failed in a later step says `Nothing was deployed`.
- The `Next steps` of `./setup.sh` name `ham models` without a count. The README says six models.

## [5.19] - 2026-11-13

### Added

- A run keeps a copy of its latest work in your account while it works. The copy is saved every minute and after a tool call that changed a file.
- When the engine stops, the run saves the end state as the latest copy before it checks or delivers anything. A delivery that does not go through keeps this copy.
- `ham pull <run id> --snapshot` downloads the latest copy of a run that works or has ended in any way: complete, blocked, paused, stalled or cleared. The output says whether the copy is the work as the engine left it or an earlier copy, and when it was saved.
- The reason of a blocked run ends with where its work is and the command that downloads it.
- The run screen shows a `Saved` line with the time of the last copy. A run that cannot save a copy says so.
- `./setup.sh --update --wait-for-runs <minutes>` waits for the active runs of everyone on the stage before it deploys. It never cancels a run.
- A release that changes no file the stage runs gives the backend its version. `./setup.sh --update` deploys nothing for such a release and writes the stage's version label. The plan says `label: the backend reports vA and will report vB`.
- `scripts/deploy.sh --stage direct --relabel` writes the version label alone for a stage that already runs the source tree. It never lowers the label.

### Changed

- While an update waits for runs or deploys, the stage starts no new runs. A new run, a follow-up, a retry or a resume gets the sentence `This stage is being updated, so it starts no new runs. Try again in a few minutes.` Reading runs, downloading results, answering questions, sending messages, cancelling and editing tags keep working.
- An update that builds only the client deploys nothing, so the stage keeps starting runs during it.
- An update that deploys looks for active runs again after you confirm it: it pauses 45 seconds, then looks every 30 seconds until two looks in a row show no run. A run found there ends a plain `./setup.sh --update` with exit 7, and nothing is installed or built.
- `ham pull <run id>`, `ham clone <run id>` and `ham link <run id>` on a run without a zip say whether the run still works or how it ended, and name `ham pull <run id> --snapshot`.
- `ham update` waits for your own runs and then hands the rest of its wait limit to setup. One limit covers the runs of everyone on the stage.
- `ham update` ends with `Updated` after a release that changes no file the stage runs, and `ham doctor` shows equal versions.
- When the backend still reports an older version after an update, `ham update` names `ham doctor` and the command that deploys the commit again, `./setup.sh --update --force-deploy`.
- When the backend runs a version newer than the main branch, `ham update` names the deploy that sets its version.
- With Cline and its goal loop on, the usage totals of a run count a prompt that ended in a provider error and the goal-loop rounds between prompts. The totals are an estimate and can be lower than the bill of your AWS account.
- Lifecycle pack 1.16.0: the run log holds the exact command of each test step, with notes in parentheses.

### Fixed

- The clean-checkout test report installs the project's packages right before the first test command that needs them. A failed install skips only the tests that need it.
- The test report does not rerun text of the run log that is not a command. A command that could not run as written does not lead the `Tests` line.
- Resume and retry notices on the Runs screen fit the row at 80 and 120 columns. The row shows a short notice and a panel or card shows the whole text.
- A copy of the work is saved also when the engine removed or replaced the `.git` folder of the repository.

## [5.18] - 2026-11-13

### Changed

- When an update stops for a stage commit that the clone cannot place, the message names the cause that fits: a clone older than the stage, the same version, or a version that cannot be read. `ham update` says whether running it again helps and names the folder of its clone.

### Fixed

- `./setup.sh --update` and `ham update` now update a stage that was deployed from a public commit that the repository history no longer holds. Setup compares the stage's version with the clone's, part by part as numbers, and goes ahead when the stage's version is lower.

## [5.17] - 2026-11-13

### Added

- `scripts/deploy-preflight.sh --stage direct` checks whether a deploy can start and changes nothing. It checks the account, the Availability Zone mapping and the VPC range, and ends with `READY` only when all three pass.
- The deploy confirms its control endpoint before it reports success. It does not print the address.

### Fixed

- `scripts/deploy.sh` ends with a clear message on a machine without Python 3.11 or later.

## [5.16] - 2026-11-12

### Fixed

- `./setup.sh --update` keeps the stage's value of a setting when the new default would replace or delete resources, and the update goes through. Later updates keep the value until you export a value or `default`.
- Setup's plan names each kept value before anything is installed, with the export that takes the new default, such as `export HAMH_INSTANCES_TYPES=default`.

## [5.15] - 2026-11-11

### Added

- `ham update` updates the backend and then `ham` to the newest version on the main branch of the public repository. It fetches a copy of that version and runs its `./setup.sh --update`.
- `ham update --check` shows your `ham` and backend versions, the newest version with its commit and the changes between. It changes nothing.
- `ham update` asks once and waits until your own active runs end. It never cancels a run and does not stop anyone from starting one. `--yes` skips the question, `--wait <minutes>` sets the wait and `--no-wait` stops at once.
- The setting `update_source` names the repository that `ham update` reads. The word `off` turns `ham update` off.
- For a run that agreed to provider data sharing, `ham` switches the account's Bedrock data retention mode from `default` or `inherit` to `provider_data_share` before the start. One line names the account, the Region, both modes and the command that sets the mode back.
- In the terminal app, a run on a model that needs provider data sharing asks before it starts while the setting is off. `y` agrees for that run only.
- The run screen keeps the whole line about a switched mode in a `provider data sharing` card.

### Changed

- The default of `HAMH_INSTANCES_TYPES` is `m7g.4xlarge,m6g.4xlarge`, so a run can use the second type when EC2 has no capacity of the first. Export `HAMH_INSTANCES_TYPES=m7g.4xlarge` to use one type only.
- An update of a stage that has the Instances part and never chose its machine types stops before any stack change. Export `HAMH_INSTANCES_TYPES=m7g.4xlarge` to keep the machine the stage has, or `HAMH_INSTANCES_TYPES=default` to take the new list.
- A model that needs provider data sharing also runs in an account whose Bedrock data retention mode is `aws_review`.
- The help, `/help` and usage errors say "the coding agent" where they mean no single engine.
- The clean-checkout test report finds the test command of a Python project without test settings: `python3 -m pytest -q` for pytest files and `python3 -m unittest discover -v` for unittest files.
- Lifecycle pack 1.15.0: the self-check prints the flow it checks and asks of the quick path only what that path asks. `ham rules init` replaces an unedited 1.14.0 copy and keeps an edited one.
- The default rules have a `Delivery` section. `ham rules init` replaces an unedited earlier copy of the default rules and keeps an edited one.

### Fixed

- A run that delivers only a zip (`--s3-only`) keeps a workflow file under `.github/workflows/` or a root `buildspec.yml` in the zip and names each such file in the run's detail.
- The skills card and the review-round notice name the engine of the run.
- A run refused because its repository is not on the stage's allowlist names the command that allows it, `ham-admin repos allow cc:<name>`.
- A `--compute instances` run that found no EC2 capacity says so and names two ways on: start it again later, or start it without `--compute instances`.
- The `Tests` line says when a test command was stopped at its limit while it was still building, and that the report then says nothing about the tests.
- When no test command is found, the sentence lists where the report looked and says that a `.ham/test-command` file names the command.
- The clean-checkout test report installs the project's packages before test commands from the lifecycle log or the goal card, using `npm ci` or `pnpm install --frozen-lockfile` from the lockfile, or the packages of `requirements.txt` for Python commands. A failed install is named on the `Tests` line, with the tests listed as `NOT RUN`.
- A goal-loop run that changed no file says that it delivered no code and gives the command to try again from the same start.
- A run that the harness ended shows the same outcome label in its last stop digest and in its summary.
- Two setups of one build that end at the same moment no longer each leave the runner to the other. The last one removes it.
- A setup that stops following a shared build takes itself out of the deploy lock, so the other setup removes the runner.
- When setup cannot check whether the runner is still in use, it keeps the runner, prints a `RETAINED` line with the reason and ends with exit 8.
- A deploy that fails right after it takes the deploy lock releases the lock.
- A deploy lock held by `scripts/deploy.sh` or by a setup with `--build-on local` tells a second setup to wait for it to end.
- `./setup.sh --full` on a deployed stage checks the client folder only when it installs a client.
- A first full setup whose build failed after its deploy names `./setup.sh --update` as the command that finishes it.
- The scripted run of a task list through the terminal app gives every engine the task text as written, with its line breaks, and scores a run that delivered nothing as failed.

## [5.14] - 2026-11-10

### Added

- A deploy prints one `DEPLOY-SETTING` line for each stage setting. A change that replaces resources, lowers log retention or removes something needs a value you export.
- One deploy of a stage runs at a time, whatever the route. A second update stops with exit 7 and names the setup that holds the stage.
- A deploy that refuses before it changes anything prints `DEPLOY-REFUSED` and exits 3. Setup then says "The stage is unchanged." and removes the runner it installed.

### Changed

- An update of the commit the stage already runs only installs the client. `--force-deploy` deploys anyway, and an older commit needs `--revision`.
- An update installs the new client where you run `ham` and keeps the previous one beside it.
- Setup checks before the confirmation that it can write to the client folder. If it cannot, it stops with exit 1 and names `--prefix`.

### Fixed

- `./setup.sh --update` keeps every stage setting a stage has, such as the Instances part, the tool layer, the utility model, the research provider and log retention. Export `HAMH_<NAME>=default` to return a setting to its default.
- An update keeps the stage's tag `auto-delete=no` and its kind, `official` or `test`, also through the account file that `setup.sh` generates. Export `HAMH_KEEP_TAG` or `HAMH_STAGE_KIND` to change them.
- An update keeps the stage's operator and zone, also on a stage that an earlier release deployed. An update by another person stops with exit 7 unless you add `--change-operator`.
- Setup reads a stage setting exported under its earlier variable name as the current name, checks its value, shows it in the plan and passes it to the deploy. Two spellings with different values stop setup with exit 2.

## [5.13] - 2026-11-09

### Added

- Claude Fable 5.2 runs on OpenCode as well as on Cline. It needs the same agreement to provider data sharing: `--engine opencode --model fable --provider-sharing on`.

## [5.12] - 2026-11-09

### Added

- Claude Fable 5.2 can be chosen as `fable`. It needs the setting `run.provider_sharing` and an AWS account in provider data sharing mode.
- Add the setting `run.provider_sharing` and the flag `--provider-sharing on|off`. With it on, the prompts and answers of a run on a model that needs it go to the model's provider, which may keep them up to 30 days. It is off by default.
- Mark the models that need provider data sharing in `ham models` and in the model list of the terminal app, and refuse them before anything is sent while the setting is off.
- Show the setting and the account's Bedrock data retention mode in `ham doctor`.

## [5.11] - 2026-11-09

### Changed

- OpenCode runs now let a shell command run up to ten minutes, as Cline's do, and `commands.timeout_seconds` sets it for OpenCode too.

## [5.10] - 2026-11-08

### Added

- The model list offers four models on OpenCode: Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol and GPT-6 Astra. GLM 5.3 is not offered on OpenCode yet.
- Both engines now ship in one worker image.
- `engine/PARITY.md` lists every protection and tool built for engines and how each applies to every engine that runs. A test holds each row for both engines or names the exception.
- `--engine <name>` and `default_engine` choose the engine for a run, including `ham continue`. A start request without an engine uses Cline and keeps the older request format.
- The New run form has an Engine box beside the Target box. Press Up or Down while the Target box is focused to change the engine.
- `/engine <name>` chooses the engine for runs started in the terminal session without changing the config file. `ham engine <name>` reports availability and explains how to choose the engine.
- The model list supports a row for each engine, with a separate smoke record. The service refuses an engine and model pair without a passing record and names the pair.
- OpenCode runs have a harness-driven goal loop. Follow-up prompts ask whether the goal is complete, and the loop has no round or token cap.
- `AGENTD_DIRECT_TOOL_IDLE_S` sets the tool-idle limit in seconds (off for Cline, 3600 for OpenCode, and 0 disables it). A tool call with no engine output past the limit cancels the prompt and restarts the engine in a fresh session.
- `engines/opencode.toml` is accepted for OpenCode. `ham engine opencode show|check` reads it, but it has no setting yet.
- OpenCode 2.2.6 is the pinned release of the OpenCode engine.
- OpenCode loads rules through `instructions` and skills through `skills.paths` in a session configuration written by the harness.

### Changed

- Outcome sentences name the engine of the run, such as "OpenCode reported done", while Cline's sentences stay unchanged. `EVIDENCE.md`, pull request text and service answers also name that engine.
- The app's words about a run name its engine. A follow-up of a finished run uses the engine of that run.
- `/engines` lists the available engines and marks the engine of the run on screen.
- Rules and skills are checked on disk before the first model call for both engines.

## [5.09] - 2026-11-07

### Added

- Set the most KiB one clipboard copy carries with `ui.copy_limit_kib`, default 64 KiB. Accept 1 to 1,048,576 KiB.
- Set immediate messages with `run.max_interrupts`, default 5. Set questions with `run.max_questions`, default 3. Set user-requested fresh sessions with `run.max_fresh_sessions`, default 3. These counts apply to one run. Allow 0 questions.
- Set research file sizes with `research.max_file_mib`, default 1 MiB. Set unpacked sizes with `research.max_unpacked_mib`, default 200 MiB. Set total run sizes with `research.max_run_mib`, default 300 MiB.
- Set container-starting file signals with `run.container_signals`. Use the built-in list by default. Accept `default`, `none`, or one line of semicolon-separated entries. Signals include a Testcontainers dependency and a docker compose file. Also recognize `.ham/test-command` when it mentions docker.
- Set one code search's wall time with `AGENTD_TOOLS_SEARCH_WALL_S`. Default to 10 seconds.
- Set log record retention with `HAMH_LOG_RETENTION_DAYS`, default 30 days.
- Set pull request check length with `HAMH_PR_CHECK_MINUTES`, default 60 minutes. Set its build machine with `HAMH_PR_CHECK_COMPUTE`, default `small`.
- Configure the Instances machine with `HAMH_INSTANCES_TYPES`, `HAMH_INSTANCES_ROOT_FREE_GIB`, `HAMH_INSTANCES_WORKSPACE_GIB`, `HAMH_INSTANCES_LIFETIME_S`, and `HAMH_INSTANCES_IDLE_S`. Keep the existing defaults.
- Set Cline's output silence limit with `HAMH_PROVIDER_SILENCE_S`, default 900 seconds. Accept 60 to 3,600 seconds. Cancel the prompt when this limit is reached.
- Set setup build length with `HAMH_SETUP_BUILD_MINUTES`. Set its machine with `HAMH_SETUP_BUILD_COMPUTE`. Use these for the build in your own account. Make the minimum `HAMH_SETUP_AUTHORITY_HOURS` follow the build length.
- Set sub-agent result silence with `AGENTD_GOAL_HELPER_SILENCE_S`, default 1500 seconds. Set oldest sub-agent wait with `AGENTD_GOAL_HELPER_WAIT_S`, default 4500 seconds. Accept 1 to 86,400 seconds for each. Let 0 switch either limit off.

### Changed

- Allow runs lasting 30 days (720 hours), raised from 7 days (168 hours). Allow 256 sessions, raised from 64. Keep the default run length at 24 hours. Accept 1 to 720 for `run.max_run_hours`.
- Allow 43,200 minutes for `--input-wait`, `HAMH_INPUT_WAIT`, and `input_wait_minutes`. Raise the limit from 240 minutes. This answer window covers 30 days, the longest run. Cut each wait to the session's remaining time.
- Allow a delivered changed file up to 2 GB. Raise the limit from 100 MiB.
- Allow research searches requesting 20 results and 300 domains. Accept 1 to 20 for `ham-research search --max`. Raise that command's limit from 10.
- Allow 20 web addresses per research fetch call, raised from 5.
- Allow a prompt to name 150 web addresses.
- Allow 20 messages waiting for running Cline, raised from 3. End the message box title with the waiting count. Show this count when more than 3 messages are open.
- Allow pull request checks to build for 60 minutes. Raise the limit from 20 minutes.
- Allow both API functions 28 seconds to answer, raised from 15.
- Allow 10 repository name rules per stage, raised from 8.
- Bound messages after Cline's first apparent finish by remaining run time. Remove the 8-turn limit. Remove the 30-minute limit on extra work.
- Cancel the prompt after 25 minutes without a sub-agent result. Also cancel when the oldest open sub-agent reaches 75 minutes. End every sub-agent when cancelling. Tell Cline in the same session that they did not return. Let Cline finish with its own tools. Allow this recovery at most twice per run. End the run on a third stalled wait.
- Wait for quiet engine output after cancelling a prompt. Then send the next prompt. Resend it once if the engine answers immediately without a model call.
- Keep repositories whose checks start containers on the microVM under `--compute auto`. The Instances machine cannot run containers.
- Reject `--compute instances` for repositories whose checks start containers. End the run before any machine starts. Name the signal and file. Show the ways to change the choice.
- Allow request member `compute` to carry `containerSignals` in protocol 1.57.0. Keep the previous request when a run sets no list.
- Add `--network-wait-minutes N|full` to `./setup.sh --teardown` and `scripts/teardown.sh`. Wait for AWS to release the interface held by AgentCore. Accept 0 to 480 minutes for `N`. Let `full` mean 480 minutes. Set the setup default with `HAMH_SETUP_NETWORK_WAIT_MINUTES`, default 0. Keep the script default at 60 minutes.
- Name the network interface held by AgentCore during teardown. Include its id. Explain that AWS releases it up to 8 hours after runtime deletion. Print the command for running teardown again.
- Exit 9 when only network resources held by that interface remain. Change `./setup.sh --teardown` from exit 8. Change `scripts/teardown.sh` from exit 1.

### Fixed

- Prevent completed code from hitting the two hour run limit. Fix waits where sub-agents started further sub-agents that kept working.
- Keep research enabled for the whole run. Remove the shutdown about nine hours into a run. A default 24-hour run reaches this in its second session.
- Read every answer when searching a long run. Remove the limit of the first 50 control items.
- Count run messages by their bytes in the reply. Keep the message list loading for a full run.

## [5.08] - 2026-11-05

### Added

- Reject start requests larger than 6,225,920 bytes before sending them, counting quotes, backslashes and line breaks twice.
- Report the request limit, the largest content part and the largest file when a start request is too large, including service rejections with HTTP 413 `payload-too-large`.
- Reject attachment, rule, skill, continue and review files larger than the whole-request limit before reading them.

### Changed

- Allow up to 100 rule files per run.
- Allow up to 32 skills per run, leaving room for 25 custom skills alongside the lifecycle pack's seven.
- Show up to 32 rows in the run screen's skills card.
- Allow rule file names, attachment names and skill file paths up to 255 bytes.
- Accept any whole number for `max_nudges` in `continue.md`.
- Allow up to 262,144 bytes of text in an answer to a question or a message to a running Cline.
- Show message length as "N of 262,144 bytes" on the typing screen.
- Allow up to 4 MiB of message text per run.
- Keep up to 256 KiB of each command's output in the clean-checkout test report, retaining the last bytes and marking truncated output as cut. Set `AGENTD_REPORT_TAIL_KIB` from 4 to 4096 to change the size.
- Keep up to 32 report steps, plan up to 12 test commands, and retain command lines of up to 2,000 characters and reasons of up to 1,000 characters.
- Allow reports up to 16 MiB, shortening the longest outputs first when combined steps exceed the limit. Older `ham` clients show no test line for reports over 64 KiB.
- Allow each report command to run for up to 465 minutes while its session lives, without exceeding the run's own time limit. Set `run.report_command_minutes` from 0 to 465 minutes, or `AGENTD_REPORT_STEP_S` and `AGENTD_REPORT_BUDGET_S` up to 27900 seconds.
- Keep Cline's complete final message, up to 200,000 characters, in the run summary and `evidence/direct.json`, and show it whole in `ham`.
- Include the start and end of final messages over 32 KiB in `EVIDENCE.md` and pull requests, with a marker counting omitted characters and naming `evidence/direct.json`.
- Split pull request comment text into parts of at most 10,240 characters to fit the repository service limit.
- Keep up to 1,000 characters in the detail beneath the outcome label.
- Keep up to 96 tries, 96 model switches and 32 interventions in the run summary.
- Pass a brief of up to 16 KiB to the next slice.
- Keep event payloads of up to 16 KiB and post backlogs of large events in batches of at most 4 MiB.
- Search back to the first run unless `since` is given.
- List every plan entry and every tool-call edit in the chat view.
- Show chat blocks of up to 200,000 characters (100,000) and complete stored records of up to 2865 KiB.
- Store transcript records of up to 256 KiB by default, with `AGENTD_TRANSCRIPT_RECORD_CAP_BYTES` allowing up to 440 KiB.
- List up to 2,000 changed file paths.
- Give fresh sessions the user's complete task.
- Allow task notes of up to 200,000 bytes, include up to 24,000 bytes in the prompt, and keep a stop digest of up to 256 KiB.

### Removed

- Remove separate size limits for prompts, rule files, `continue.md`, `review.md`, attachments and skill files.
- Remove the limit on the number of attachments.
- Remove limits on the number of files in a skill and the depth of skill paths.
- Remove the 8,000-character limit on answers and messages to a running Cline.

### Fixed

- Return the whole prompt in run details when it exceeds 256 KiB.

## [5.07] - 2026-11-05

### Added

- Test steps marked NOT RUN for the container runtime show socket diagnostics and up to 2,048 bytes of runtime output, prefixed with `RUNTIME: `.

### Changed

- Write the DOX starter to root `AGENTS.md` before the first prompt, adding `THIRD-PARTY-NOTICES.md` when it contains the DOX framework.
- Commit the DOX starter before reading the run's base commit, excluding it from engine changes and the "no changes" label.
- Deliver the DOX starter even when a run ends early. Later sessions restore it from the saved bundle without another commit.
- Update the lifecycle pack to 1.14.0. The delivery skill completes and corrects the seeded tree at run end and removes the starter for a missed documentation ban.
- `ham rules init` replaces unedited lifecycle pack 1.13.0 copies and preserves edited copies.
- Remove the never-enforced 8 KB (before 5.06) and 9 KB (5.06) size advice from default rules; the opening now asks only for no secrets, with limits unchanged at 64 KiB per file and 32 files per run. `ham rules init` replaces unedited 5.06 defaults and preserves edited copies, where users may delete the size advice.

### Fixed

- Preserve container runtime processes and their child processes when the engine stops. Test containers remain available until `ham-containers down` after the report.
- Read gotestsum summary counts in Go test reports, calculating passed tests as total tests minus skipped tests and failures.
- Recognize `go run` and `go tool` invocations of gotestsum in test reports.
- Select the last command run even with `timeout` or leading environment settings, so a later passing run replaces an earlier failure in the report.

## [5.06] - 2026-11-05

### Added

- Configure multiple backends with `[[backend]]` tables containing `alias`, `apiEndpoint`, `region`, `awsProfile` and `customerContent`.
- Add top-level `defaultBackend` for commands that name no backend, and `customerContent` to declare whether the default backend is safe for customer content.
- Reject `--customer-content` starts unless the selected backend declares `customerContent = true`; customer content is disallowed by default.
- Add root `CHANGELOG.md` in Keep a Changelog format for every public version. Contributor rules require an `Unreleased` entry in the same change for each user- or maintainer-visible change.

### Changed

- Keep the three existing top-level configuration keys as the `default` backend. Clients with one backend behave as before.
- Read all backends concurrently in the Runs list with per-backend deadlines; merge runs newest first and show a backend column when two or more backends are configured.
- Show a separate banner for each unavailable, signed-out or slow backend without stopping reads from the others.
- Route commands for one run to its backend using the plain run ID; `--backend <alias>` skips the lookup.
- Require `--backend <alias>` or the start screen's Backend field when starting a run, unless `defaultBackend` is set.
- Check all backends concurrently with `ham doctor`.
- Make backends with versions older than the client supports read-only; show `mixed` in the title when backend versions differ.
- Make setup-wizard tests wait for the first status call or process exit instead of fixed time limits.
- Add `HAMH_TEST_WAIT_SCALE` (1-8) to extend remaining test time limits on slow or busy machines; the parallel scripts runner defaults to 4. Hung tests still fail.
- Default rules gain a "Dated rules" section for rule stamps in `YYYY-MM-DD HH:MM TZ` format. The agent flags stale or conflicting rules and requires user confirmation to change them.

## [5.05] - 2026-11-04

### Fixed

- Fixed PostgreSQL 16 startup on long session paths: it starts without a Unix socket, using TCP only. Early exits show the log tail and FATAL message; shutdown does not wait for open connections.

## [5.04] - 2026-11-03

### Added

- The `ham resume` and `/resume` commands start follow-up runs from paused or stalled work; `/resume --all` resumes all, and `R` in run lists asks for confirmation.
- The `run.max_run_hours` setting accepts 1 to 168 whole hours (default 24 hours) and takes effect from the next run; the `--max-run-hours <hours>` flag also sets it.
- New run marks: a yellow `=` for `paused` runs, which used their total time, and a magenta `~` for `stalled` runs, which lost their progress; both delivered their work.

### Changed

- Unfinished runs nearing the 8-hour microVM session limit continue from saved work in a new session under the same run id, until their total time is used up.

## [5.03] - 2026-11-03

### Changed

- Clean-checkout test reports use the repository's own test scripts and state what each script ran without.
- Lifecycle pack 1.13.0: run the failing test with the repository's test command before other edits. Full and auto commit the test and red record alone before code; quick: one commit, no record.

### Fixed

- Fixed finished runs showing `complete (delivering)` in `ham runs`, `ham status` and `ham watch`; they now show `complete (delivered)`. JSON keeps the stored phase.
- Fixed helper-agent reviews causing completed work to end as a stall; the run uses the reported-done label.

## [5.02] - 2026-11-03

### Changed

- The `ham` title band shows flame and animated smoke in five rows at 33 or more terminal rows; `ui.mascot_motion = false` stops motion in the band and empty Runs table.

## [5.01] - 2026-11-02

### Added

- The `ham config migrate [--dry-run]` command copies earlier settings into `~/.config/ham`, resumes interrupted copies and refuses differing files or symbolic links without overwriting them.
- The `setup.sh` installation keeps a relative link with the earlier command name beside `ham`.
- The `ui.deprecation_notices` setting controls earlier-name notices, defaults to on and takes effect after restart; `HAMH_NO_DEPRECATION_NOTICES=1` forces it off.

### Changed

- The empty Runs list mascot gains rocket flames and moving smoke; `ui.mascot_motion` defaults to on and takes effect immediately.
- The terminal app reads earlier settings read-only when `~/.config/ham` has no user content; writes exit 2 with migration guidance, and the current folder wins when both hold content.
- The terminal app, setup and deployment accept previous variable names; non-empty `HAMH_` values win, and setup and deployment refuse conflicting selectors.
- Agentd and lifecycle tools use `.ham` or an earlier repository folder for lifecycle pack records, refuse state in both and name that folder in default goals; user-written goals stay unchanged.
- The `./setup.sh --teardown` command covers both naming generations with one plan and confirmation; `--only legacy|current` limits coverage, and repositories remain in place.
- The `setup.sh --status` command reports `state=legacy` with exit 7 or `state=legacy-retained` with exit 8; `--doctor` fails with exit 3 and a FAIL line about the previous names.

## [5.00] - 2026-10-31

### Changed

- The project becomes Hyperscale Agent Meta Harness and the terminal app becomes `ham`.
- The variable prefix becomes `HAMH_`.
- AWS resource names use the `hamh-direct-*` prefix, and the project tag becomes `hyperscale-agent-meta-harness`.

## [4.42] - 2026-10-31

### Added

- Stages accept 1 to 8 repository names or wildcard rules through `./setup.sh --repo-name`, `repoNames` and `HAMH_REPO_NAMES`; updates preserve rules unless replacements are supplied.
- Full setup builds, checks and installs `ham-admin` alongside `ham`.

### Changed

- Updates refuse active runs before building with exit 7; `./setup.sh --update --allow-active-runs` defers the check but still refuses worker runtime changes while runs are active.
- Setup reuses a matching Go module store with a passed offline check, skipping the fill and full check.
- When no stage URL is set, `ham doctor` and `ham models --server` name the `api_endpoint` setting and say the URL ends in `/direct`.
- Unresolved stage addresses suggest `./setup.sh --status` or `ham config set api_endpoint <url>`. The Runs screen wraps long errors.

### Fixed

- Setup and deployment keep the stage's `owner` tag. Deployment refuses a differing `HAMH_OWNER` or account-file `owner` before building.
- `./setup.sh --build-on local` refuses a differing `HAMH_OWNER` with exit 7. On the default runner route, setup warns and continues with the stage's owner; the runner does not receive `HAMH_OWNER`.
- When the stage owns the destination, `./setup.sh --teardown` restores prior Bedrock invocation logging or unsets it; other destinations stay unchanged. Failures keep the record and exit 8.
- Teardown deletes queued secrets after their stacks are gone and exits 8 for incomplete deletion; runner cleanup retries caller IAM denials with the CloudFormation execution role.
- Teardown reports RETAINED only after CloudFormation accepts retention, shows AgentCore endpoint deletion progress, and explains retained network resources when a rerun holds only a VPC.
- Worker runtimes omit descriptions to prevent unintended runtime versions. A stage with an existing description gets one more runtime version at its next update.

## [4.41] - 2026-10-30

### Added

- Command-line help works before setup with `ham --help`, `ham -h`, `ham help`, `ham help <command>` and `ham <command> --help`.

### Changed

- `./setup.sh --teardown` empties the stage's two versioned buckets and installs no runner; it needs neither git nor a clone.
- Teardown limits deletion to stage-owned resources in the confirmed plan; resources that appear later remain untouched and make teardown incomplete.
- Noninteractive teardown requires `--account-id` with `--yes --acknowledge-teardown`; a missing ID exits 4 and a mismatched account exits 7.
- Stage and delivery repository owner tags and run-session's `TENANT` use the selected `accounts.json` entry's `owner`, falling back to the operator's `name`, then `ham-user`.

### Fixed

- Harness-protocol 1.52.0 groups test steps by runner, takes each group's largest counts and adds distinct count triples; counts stay absent if a test step lacks them.
- Fixed interactive client-only setup failing after four answers when `ham` is missing. The wizard asks for its source first; `./setup.sh --help` says at most six questions.
- Fixed setup modifying an existing `CDKToolkit` stack with a custom qualifier; it stops with exit 7 before creating resources and explains the required qualifier.
- Fixed full setup deployment failing when unresolved paths prevented the container from mounting `accounts.json`.
- Fixed setup marking stages complete without a deployment record when all stacks exist; they are partial, `--full` and `--update` redeploy, and `--status` prints state=partial (exit 5).
- Fixed local setup spinning: without `flock` or with bash older than 4.1, builds start without a slot; `flock` output or exit 126 or more stops the run with exit 2.

## [4.40] - 2026-10-30

### Added

- `setup.sh` defaults to full setup: it installs a CodeBuild runner, builds and deploys the stage, then downloads and configures the client; client-only installation is also available.
- `setup.sh` supports resumable operations, `--status`, `--doctor`, `--update` and `--teardown`, with runner or local builds and a read-only preflight.
- Client installation supports Apple Silicon Macs and Linux amd64 or arm64, checks downloads with SHA-256 and refuses Intel Macs with a reason.
- `HAMH_ACCOUNTS_FILE` selects one file for build and deploy tools without editing `accounts.json`; relative paths, missing files and directories are refused.

### Changed

- Default rules require the scanner kit before the last commit, give its command, and require the agent to run it in the background and save output to a file.
- The scanner kit reports its path and scan scope, includes uncommitted and untracked files, and limits fallback scopes to the 300 newest files.

### Fixed

- Fixed setup re-runs to check all nine stage stacks and current inputs before skipping deployment, check stored client bytes and complete interrupted Rust toolchain installs.
- Setup runner tries bucket writes up to three times with five-second pauses, so a completed build survives an S3 throttle.
- Setup preflight rejects `--invocation-logging enable-and-restore` unless `--build-on local` is selected, before creating configuration or infrastructure.

## [4.39] - 2026-10-28

### Added

- The scanner kit sorts findings by file and line, shows at most 40, and reports missing checks with `KIT-NOT-RUN <part>` and `missing=<parts>` while other checks continue.

## [4.38] - 2026-10-28

### Added

- Lifecycle pack 1.10.0 adds semgrep 1.78.0 scans of changed Python and shell files with pinned rules. The kit cannot scan when the rules package is unpublished.
- With `HAMH_SCANNER_KIT=off`, the kit reports `KIT-OFF`. Install or run failures emit `KIT-NOT-RUN` with a reason and exit 0; Cline logs NOT RUN and copies the line into its last message.

### Changed

- Default rules ask the agent with a scanner kit to scan changed files before the last commit. Findings must be fixed or explained as false positives in the repository's own form.

## [4.37] - 2026-10-28

### Changed

- Default rules tell the agent to compare failing tests' first error lines and read every differing error: tests already failing at the base because of this session can fail for a new reason.

## [4.36] - 2026-10-28

### Changed

- Default rules ask the agent before the last commit to list and read repository suppressions and reasons, check its diff for matches and add matching suppressions in the same form with its own reason.

## [4.35] - 2026-10-28

### Changed

- Default rules ask the agent before the last commit to list repository-suppressed IDs, check its diff for matches and, on a match, add the same suppression in the same form with its own reason.

## [4.34] - 2026-10-28

### Changed

- Default rules require checks after every change, even docs or comments, retries with redirect settings for missing paths or unwritable folders, and NOT RUN with a reason if still blocked.
- Default rules require suppression comments to match repository forms, rule ids and reasons; `ham rules init` replaces unedited installed copies and keeps edited copies.

## [4.33] - 2026-10-28

### Changed

- The lifecycle pack moves to 1.9.0 and the harness protocol to 1.50.0 with wording-only changes. Behaviour is unchanged.

## [4.32] - 2026-10-28

### Changed

- Lifecycle pack 1.8.0 generalizes contract and loop wording; `ham rules init` updates unedited 1.7.0 rules and loop skills, leaving edited files unchanged.

### Fixed

- Fixed slow process startup by capping the soft open-files limit for agentd and its child processes at 65536 by default, configurable with `AGENTD_OPEN_FILES_CAP`.

## [4.31] - 2026-10-27

### Added

- Run summaries record the engine identity under protocol 1.49.0; older summaries remain unchanged when identity is absent.

## [4.30] - 2026-10-27

### Added

- Agentd allows new successful tool calls before stall limits via `run.stall_new_calls`: 0 to 1000 calls between file changes, default 100; 0 disables the allowance, effective next run.
- Run evidence records the declared flow as `quick`, `full` or `none`; `ham pull`, `ham watch` and delivery cards show it, and `ham pull --json` includes it.
- Settings `run.cargo_no_debug_info` and `run.cargo_no_incremental` default to on for the next run; turning them off leaves the corresponding Cargo environment overrides unset.
- Settings `run.exclude_target_dir` and `run.outside_checkout_note` default to on for the next run; turning them off stops adding `target/` to the checkout's local exclude list and removes the outside-checkout goal note.

### Changed

- Reasoning defaults to `xhigh` for GPT-6.1 Sol and GPT-6 Astra, and `high` for Claude Opus 5.5, Claude Sonnet 5.5 and GLM 5.3; explicit levels override these defaults.
- Explicit reasoning levels a model does not support are rejected.
- Session check text marks unavailable checks `NOT RUN`, explains root, Docker socket and SysV shared-memory limits, and tells the agent to leave code unchanged for them.

### Fixed

- Fixed missing worker tools: every worker image family now includes `diffutils`, `patch` and `jq`, and a `pip` wrapper running `/usr/bin/python3.12 -m pip` was added.

## [4.29] - 2026-10-27

### Added

- The `ham` terminal app starts remote Cline runs on Amazon Bedrock AgentCore from Mac or Linux; runs continue with the laptop closed, and `ham watch` rebuilds run state when you return.
- Runs return an S3 zip with source, a git bundle and checksums; `ham pull` resumes downloads and checks SHA-256, while `ham clone` builds a local git repository from the bundle.
- `ham pr` prints the link to the CodeCommit pull request of a run.
- Bedrock is the only provider: choose Claude Opus 5.5, Claude Sonnet 5.5 (default), GPT-6.1 Sol, GPT-6 Astra or GLM 5.3 via `--model`, `HAMH_MODEL` or `ham config set default_model`.
- The `ham rules init` command installs a lifecycle pack with a rules file and seven skills; `ham rules edit` and `ham rules show` manage rules, with edits applying to the next run.
- Runs end with one of 15 fixed stop outcomes, without a quality verdict; "Cline reported done" requires a final Cline message and at least one changed file.
- Failed recovery shows "Cline needs you" and rings once: `p` messages, `s` starts fresh, `m` tries a model, `f` twice finishes. Runs ask up to 3 questions; unanswered runs deliver existing work.

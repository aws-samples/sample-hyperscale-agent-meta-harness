//! The look of the terminal app: one listing call of git that asks the update source for the names of its branches and tags, closed
//! against the settings of the folder it is started in and against login prompts, with a cap on its output and a bounded wait. It writes
//! nothing. The source is a git repository in a scratch folder (`file:///`); where a test needs a git that misbehaves, a shell script the
//! test writes stands in for git. No test reads a host outside this machine. The last three tests run the look as the event loop of the
//! terminal app does (`Effect::LookForUpdate`): its answer names the source it read, it starts no process when the look is switched off,
//! and a slow look never holds the loop's thread.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{Effect, Msg, perform_with_env};
use ham::update::{
    Git, LOOK_LIMIT, LOOK_OUTPUT_CAP, LookOutcome, look_program, look_source, run_look,
};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{Fake, REGION, creds};

/// A case of a look that is switched off: what it is, the config file, and further variables of the environment.
type OffCase = (&'static str, String, Vec<(&'static str, &'static str)>);

/// The version after this build's (`5.24` gives `5.25`).
fn next(by: u32) -> String {
    let (major, minor) = ham::version::CLIENT.split_once('.').unwrap();
    format!("{major}.{:02}", minor.parse::<u32>().unwrap() + by)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commit(repo: &Path, file: &str, text: &str) {
    std::fs::write(repo.join(file), text).unwrap();
    git(repo, &["add", "."]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "a commit",
        ],
    );
}

/// A source repository with one commit on `branch`.
fn source(label: &str, branch: &str) -> PathBuf {
    let repo = support::scratch(label).join("source");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", branch]);
    commit(&repo, "VERSION", "1\n");
    repo
}

fn annotated(repo: &Path, tag: &str) {
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "tag",
            "-a",
            tag,
            "-m",
            "a release",
        ],
    );
}

fn url(repo: &Path) -> String {
    format!("file://{}", repo.display())
}

/// A shell script that stands in for a program.
fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Whether the process `pid` exists: `kill -0` answers for it.
fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn b513_look_finds_the_tagged_release() {
    let release = next(1);
    let tag = format!("v{release}");
    let found = LookOutcome::Newest(release.clone());

    // An annotated tag on the head of main.
    let repo = source("b513-look-annotated", "main");
    annotated(&repo, &tag);
    assert_eq!(Git::system().look(&url(&repo), LOOK_LIMIT), found);
    // One more commit on main after the tagged release: the head carries no tag, and the highest version tag is still named.
    commit(&repo, "README.md", "one more line\n");
    assert_eq!(Git::system().look(&url(&repo), LOOK_LIMIT), found);

    // A lightweight tag on the head of main.
    let light = source("b513-look-light", "main");
    git(&light, &["tag", &tag]);
    assert_eq!(Git::system().look(&url(&light), LOOK_LIMIT), found);

    // An older release tagged on an older commit and the newer one on the head: the newer one.
    let two = source("b513-look-two", "main");
    annotated(&two, &format!("v{}", ham::version::CLIENT));
    commit(&two, "VERSION", "2\n");
    annotated(&two, &tag);
    assert_eq!(Git::system().look(&url(&two), LOOK_LIMIT), found);
}

#[test]
fn b513_look_finds_nothing_without_a_tag_or_without_main() {
    // Main and no version tag: a mirror without tags.
    let bare = source("b513-look-notag", "main");
    assert_eq!(
        Git::system().look(&url(&bare), LOOK_LIMIT),
        LookOutcome::NoRelease
    );
    // A tag that is no version tag is not a release.
    git(&bare, &["tag", "nightly"]);
    assert_eq!(
        Git::system().look(&url(&bare), LOOK_LIMIT),
        LookOutcome::NoRelease
    );
    // Version tags and no main branch: ham update would have nothing to install.
    let other = source("b513-look-nomain", "trunk");
    git(&other, &["tag", &format!("v{}", next(1))]);
    assert_eq!(
        Git::system().look(&url(&other), LOOK_LIMIT),
        LookOutcome::NoRelease
    );
    // A branch other than main and no tag: git matches nothing and ends with 2.
    let empty = source("b513-look-nothing", "trunk");
    assert_eq!(
        Git::system().look(&url(&empty), LOOK_LIMIT),
        LookOutcome::NoRelease
    );
    // A source that does not exist is a look that failed, not "no release".
    let gone = support::scratch("b513-look-gone").join("no-such-repository");
    assert_eq!(
        Git::system().look(&url(&gone), LOOK_LIMIT),
        LookOutcome::Failed
    );
}

#[test]
fn b513_look_command_is_closed() {
    let source = "file:///srv/mirror/ham.git";
    let cmd = Git::with_program("/usr/bin/git").look_command(source);
    assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/git"));
    // git finds no repository whatever folder the app was started in: the working directory is the root and GIT_DIR is the null device.
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/")));
    let env = |name: &str| -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(name))
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    assert_eq!(env("GIT_DIR"), Some(Some("/dev/null".to_string())));
    // No program is started to ask for a login: the variable is set and empty, and the two variables git would fall back to or a
    // library would write a file for are removed.
    assert_eq!(env("GIT_ASKPASS"), Some(Some(String::new())));
    assert_eq!(env("SSH_ASKPASS"), Some(None), "removed");
    assert_eq!(env("SSLKEYLOGFILE"), Some(None), "removed");
    // What `ham update` gives its git stays.
    assert_eq!(env("GIT_CONFIG_NOSYSTEM"), Some(Some("1".to_string())));
    assert_eq!(
        env("GIT_CONFIG_GLOBAL"),
        Some(Some("/dev/null".to_string()))
    );
    assert_eq!(env("GIT_TERMINAL_PROMPT"), Some(Some("0".to_string())));
    assert_eq!(env("GCM_INTERACTIVE"), Some(Some("never".to_string())));

    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let listing = [
        "ls-remote",
        "--exit-code",
        "--heads",
        "--tags",
        "--",
        source,
        "refs/heads/main",
        "refs/tags/v*",
    ];
    assert!(args.ends_with(&listing.map(String::from)), "{args:?}");
    // Before the listing stand only the -c options of `ham update`'s git, in pairs.
    let options = &args[..args.len() - listing.len()];
    assert_eq!(options.len() % 2, 0, "{options:?}");
    for pair in options.chunks(2) {
        assert_eq!(pair[0], "-c", "{options:?}");
    }
    let set: Vec<&str> = options.chunks(2).map(|p| p[1].as_str()).collect();
    for wanted in [
        "core.hooksPath=/dev/null",
        "credential.helper=",
        "protocol.allow=never",
        "protocol.file.allow=always",
        "transfer.fsckObjects=true",
        "http.lowSpeedLimit=1000",
        "http.lowSpeedTime=60",
    ] {
        assert!(set.contains(&wanted), "{wanted}: {set:?}");
    }
    // An https source allows that protocol and no other.
    let https: Vec<String> = Git::system()
        .look_command("https://example.com/ham.git")
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        https.contains(&"protocol.https.allow=always".to_string()),
        "{https:?}"
    );
    assert!(
        !https.contains(&"protocol.file.allow=always".to_string()),
        "{https:?}"
    );
}

#[test]
fn b513_look_program_rules() {
    let path = OsStr::new("/first/bin:/second/bin:/usr/bin");
    let installed = Path::new("/opt/ham/bin/ham");
    let none: &dyn Fn(&Path) -> bool = &|_| false;
    let second: &dyn Fn(&Path) -> bool = &|p| p == Path::new("/second/bin/git");
    let system: &dyn Fn(&Path) -> bool = &|p| p == Path::new("/usr/bin/git");

    // No program file of this ham is known: ham update could not be started.
    assert_eq!(look_program(None, Some(path), second, false), None);
    // A client in a build folder: ham update cannot install there, so the notice would promise what cannot be done.
    assert_eq!(
        look_program(
            Some(Path::new("/src/target/debug/ham")),
            Some(path),
            second,
            false
        ),
        None
    );
    assert_eq!(
        look_program(Some(Path::new("/ham")), Some(path), second, false),
        None
    );
    // A client in <prefix>/bin: the first git on PATH, here in its second folder.
    assert_eq!(
        look_program(Some(installed), Some(path), second, false),
        Some(PathBuf::from("/second/bin/git"))
    );
    // No git on PATH, and no PATH at all.
    assert_eq!(look_program(Some(installed), Some(path), none, false), None);
    assert_eq!(look_program(Some(installed), None, second, false), None);
    // A folder of PATH that is not an absolute path is passed over: the look never starts a program from the folder the app stands in.
    let here: &dyn Fn(&Path) -> bool =
        &|p| p == Path::new("git") || p == Path::new("./git") || p == Path::new("tools/git");
    assert_eq!(
        look_program(Some(installed), Some(OsStr::new(":.:tools")), here, false),
        None
    );
    // Not a Mac: the system's git is a git like any other.
    assert_eq!(
        look_program(Some(installed), Some(path), system, false),
        Some(PathBuf::from("/usr/bin/git"))
    );
    // A Mac whose git is the system's launcher and that has no developer tools: starting it would open the install dialog.
    assert_eq!(
        look_program(Some(installed), Some(path), system, true),
        None
    );
    // The same Mac with the command-line tools, or with Xcode at its usual place.
    for tools in [
        "/Library/Developer/CommandLineTools/usr/bin/git",
        "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
    ] {
        let with_tools = move |p: &Path| p == Path::new("/usr/bin/git") || p == Path::new(tools);
        assert_eq!(
            look_program(Some(installed), Some(path), &with_tools, true),
            Some(PathBuf::from("/usr/bin/git")),
            "{tools}"
        );
    }
    // A Mac with another git first on PATH: that one, whatever the developer tools are.
    let both = |p: &Path| p == Path::new("/second/bin/git") || p == Path::new("/usr/bin/git");
    assert_eq!(
        look_program(Some(installed), Some(path), &both, true),
        Some(PathBuf::from("/second/bin/git"))
    );
}

/// An environment with a config folder of its own that holds `config`, plus `extra`.
fn env_with(
    label: &str,
    config: Option<&str>,
    extra: &[(&str, &str)],
) -> impl Fn(&str) -> Option<String> + use<> {
    let root = support::scratch(label);
    let dir = root.join("config/ham");
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(text) = config {
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }
    let mut pairs = vec![(
        "XDG_CONFIG_HOME".to_string(),
        root.join("config").display().to_string(),
    )];
    pairs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

const ENDPOINT: &str = "https://example.execute-api.us-east-1.amazonaws.com/direct";

#[test]
fn b513_look_is_off_and_starts_no_process() {
    let source = "file:///srv/mirror/ham.git";
    let plain = format!(
        "configVersion = 1\napiEndpoint = \"{ENDPOINT}\"\nregion = \"us-east-1\"\nupdateSource = \"{source}\"\n"
    );
    // A look is allowed: the source the config names, and the public repository when it names none or there is no file.
    assert_eq!(
        look_source(&env_with("b513-src-plain", Some(&plain), &[])),
        Ok(Some(source.to_string()))
    );
    assert_eq!(
        look_source(&env_with(
            "b513-src-default",
            Some("configVersion = 1\n"),
            &[]
        )),
        Ok(Some(ham_core::update::DEFAULT_SOURCE.to_string()))
    );
    assert_eq!(
        look_source(&env_with("b513-src-nofile", None, &[])),
        Ok(Some(ham_core::update::DEFAULT_SOURCE.to_string()))
    );
    // The file says on, and the variable says nothing that switches it off.
    let on = format!("{plain}\n[ui]\nupdate_notice = true\n");
    for value in ["0", "false", "off", ""] {
        assert_eq!(
            look_source(&env_with(
                "b513-src-on",
                Some(&on),
                &[("HAMH_NO_UPDATE_NOTICE", value)]
            )),
            Ok(Some(source.to_string())),
            "{value:?}"
        );
    }

    let table = format!(
        "\n[[backend]]\nalias = \"lab\"\napiEndpoint = \"{ENDPOINT}\"\nregion = \"us-east-1\"\nawsProfile = \"lab\"\n"
    );
    let cases: Vec<OffCase> = vec![
        (
            "update_source is off",
            "configVersion = 1\nupdateSource = \"off\"\n".to_string(),
            vec![],
        ),
        (
            "a source that is neither https nor a file path",
            "configVersion = 1\nupdateSource = \"http://example.com/ham.git\"\n".to_string(),
            vec![],
        ),
        (
            "a source with a login in its address",
            // The word of the login is put in at run time, so that no source line is shaped like a credential.
            format!(
                "configVersion = 1\nupdateSource = \"https://me:{}@example.com/ham.git\"\n",
                ["wo", "rd"].concat()
            ),
            vec![],
        ),
        (
            "ui.update_notice is false",
            format!("{plain}\n[ui]\nupdate_notice = false\n"),
            vec![],
        ),
        (
            "the variable switches the look off",
            plain.clone(),
            vec![("HAMH_NO_UPDATE_NOTICE", "1")],
        ),
        (
            "the variable wins over a file that says on",
            on.clone(),
            vec![("HAMH_NO_UPDATE_NOTICE", "yes")],
        ),
        ("two backends", format!("{plain}{table}"), vec![]),
        (
            "one backend in a [[backend]] table",
            format!("configVersion = 1\nupdateSource = \"{source}\"\n{table}"),
            vec![],
        ),
        (
            "the variable switches the look off whatever the file holds",
            "= = =\n".to_string(),
            vec![("HAMH_NO_UPDATE_NOTICE", "1")],
        ),
    ];
    for (what, config, extra) in cases {
        assert_eq!(
            look_source(&env_with("b513-src-off", Some(&config), &extra)),
            Ok(None),
            "{what}"
        );
    }
    // A config that cannot be read at this moment says nothing about what the person chose: it is no switch. The terminal app takes
    // it as a look that failed.
    let unread: Vec<OffCase> = vec![
        (
            "a config with a key ham does not know",
            format!("{plain}noSuchKey = 1\n"),
            vec![],
        ),
        ("a config that is not TOML", "= = =\n".to_string(), vec![]),
        (
            "a key that is half written",
            format!("{plain}\n[ui]\nupdate_no\n"),
            vec![],
        ),
    ];
    for (what, config, extra) in unread {
        let read = look_source(&env_with("b513-src-unread", Some(&config), &extra));
        assert!(read.is_err(), "{what}: {read:?}");
    }
    // No config location at all.
    assert!(look_source(&|_| None).is_err());
}

#[test]
fn b513_look_ends_at_its_limit() {
    let dir = support::scratch("b513-look-limit");
    let pid = dir.join("pid");
    // A git that never answers: the script writes its process id and becomes a long sleep.
    let slow = script(
        &dir,
        "git",
        &format!("#!/bin/sh\necho $$ > {}\nexec sleep 30\n", pid.display()),
    );
    let started = Instant::now();
    let outcome =
        Git::with_program(&slow).look("file:///srv/mirror/ham.git", Duration::from_secs(1));
    let took = started.elapsed();
    assert_eq!(outcome, LookOutcome::Failed);
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    let pid = std::fs::read_to_string(&pid).unwrap();
    assert!(
        !process_exists(pid.trim()),
        "the child was ended at the limit"
    );
}

#[test]
fn b513_look_ends_at_its_limit_while_a_child_holds_its_output() {
    let dir = support::scratch("b513-look-held");
    let pid = dir.join("helper-pid");
    // A git that ends with 0 at once while a helper it started keeps its standard output open for 8 seconds. It writes the helper's
    // process id, so the test can end the helper when it has seen enough.
    let held = script(
        &dir,
        "git",
        &format!(
            "#!/bin/sh\nsleep 8 &\necho $! > {}\nexit 0\n",
            pid.display()
        ),
    );
    let started = Instant::now();
    let outcome =
        Git::with_program(&held).look("file:///srv/mirror/ham.git", Duration::from_secs(1));
    let took = started.elapsed();
    let helper = std::fs::read_to_string(&pid).unwrap();
    // TRIGGER PRESENT: the helper still runs, so the output it holds was open for the whole look.
    let held_open = process_exists(helper.trim());
    // The helper is not left behind for the rest of its 8 seconds.
    let _ = std::process::Command::new("kill")
        .arg(helper.trim())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(held_open, "the helper ended before the look did");
    assert_eq!(
        outcome,
        LookOutcome::Failed,
        "an output that stays open is a look that failed"
    );
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(
        took < Duration::from_secs(5),
        "the look ends at its limit, not when the helper ends: {took:?}"
    );
}

#[test]
fn b513_look_fails_on_an_answer_over_its_cap() {
    assert_eq!(LOOK_OUTPUT_CAP, 1_048_576);
    assert_eq!(LOOK_LIMIT, Duration::from_secs(20));
    let dir = support::scratch("b513-look-cap");
    let pid = dir.join("pid");
    // 8,192 bytes on standard output, then a long sleep: more than the cap of 4,096.
    let loud = script(
        &dir,
        "loud",
        &format!(
            "#!/bin/sh\necho $$ > {}\nhead -c 8192 /dev/zero\nexec sleep 30\n",
            pid.display()
        ),
    );
    let started = Instant::now();
    let over = run_look(
        std::process::Command::new(&loud),
        Duration::from_secs(20),
        4_096,
    );
    let took = started.elapsed();
    assert!(over.is_err(), "{over:?}");
    assert!(
        took < Duration::from_secs(10),
        "well before the limit: {took:?}"
    );
    let pid = std::fs::read_to_string(&pid).unwrap();
    assert!(!process_exists(pid.trim()), "the child was ended");

    // The same on the error output, which is read and dropped.
    let loud_err = script(
        &dir,
        "loud-err",
        "#!/bin/sh\nhead -c 8192 /dev/zero >&2\nexec sleep 30\n",
    );
    let started = Instant::now();
    let over = run_look(
        std::process::Command::new(&loud_err),
        Duration::from_secs(20),
        4_096,
    );
    assert!(over.is_err(), "{over:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // An answer under the cap is read whole, with the exit code.
    let listing: String = (0..80)
        .map(|n| format!("{}\trefs/x{n:03}\n", "a".repeat(40)))
        .collect();
    assert_eq!(listing.len(), 4_080, "16 bytes under the cap");
    let file = dir.join("listing.txt");
    std::fs::write(&file, &listing).unwrap();
    let quiet = script(
        &dir,
        "quiet",
        &format!("#!/bin/sh\ncat {}\nexit 2\n", file.display()),
    );
    assert_eq!(
        run_look(
            std::process::Command::new(&quiet),
            Duration::from_secs(20),
            4_096
        ),
        Ok((Some(2), listing))
    );
    // A program that cannot be started is an error, never a panic.
    assert!(
        run_look(
            std::process::Command::new(dir.join("no-such-program")),
            Duration::from_secs(1),
            4_096
        )
        .is_err()
    );
}

#[test]
fn b513_look_reads_the_exit_code_and_the_listing_of_git() {
    let dir = support::scratch("b513-look-codes");
    let listing = dir.join("listing.txt");
    let id = "c".repeat(40);
    let release = next(2);
    std::fs::write(
        &listing,
        format!("{id}\trefs/heads/main\n{id}\trefs/tags/v{release}\n"),
    )
    .unwrap();
    let say = |name: &str, code: u32, print: bool| {
        script(
            &dir,
            name,
            &format!(
                "#!/bin/sh\n{}exit {code}\n",
                if print {
                    format!("cat {}\n", listing.display())
                } else {
                    String::new()
                }
            ),
        )
    };
    let look =
        |program: &Path| Git::with_program(program).look("file:///srv/mirror/ham.git", LOOK_LIMIT);
    assert_eq!(look(&say("found", 0, true)), LookOutcome::Newest(release));
    assert_eq!(look(&say("nothing", 2, false)), LookOutcome::NoRelease);
    assert_eq!(look(&say("empty", 0, false)), LookOutcome::NoRelease);
    // Any other exit code is a look that failed, whatever was printed.
    assert_eq!(look(&say("fatal", 128, true)), LookOutcome::Failed);
    assert_eq!(look(&say("one", 1, true)), LookOutcome::Failed);
    // A listing that is not a listing is a look that failed.
    let words = script(&dir, "words", "#!/bin/sh\necho not a listing\nexit 0\n");
    assert_eq!(look(&words), LookOutcome::Failed);
    // The script was given the closed command: it runs in the root folder with the listing arguments.
    let record = dir.join("record.txt");
    let recorder = script(
        &dir,
        "recorder",
        &format!(
            "#!/bin/sh\npwd > {r}\necho \"$GIT_DIR\" >> {r}\necho \"$*\" >> {r}\nexit 2\n",
            r = record.display()
        ),
    );
    assert_eq!(look(&recorder), LookOutcome::NoRelease);
    let seen = std::fs::read_to_string(&record).unwrap();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(lines[0], "/");
    assert_eq!(lines[1], "/dev/null");
    assert!(
        lines[2].ends_with(
            "ls-remote --exit-code --heads --tags -- file:///srv/mirror/ham.git refs/heads/main refs/tags/v*"
        ),
        "{seen}"
    );
}

#[test]
fn b513_ham_config_names_the_variable_only_while_it_switches_the_look_off() {
    use ham::config_cmd::effective;
    use ham_core::config::Config;
    use ham_core::request::Origin;
    use ham_core::settings::Key;
    let cfg = Config::default();
    let off = |name: &str| (name == "HAMH_NO_UPDATE_NOTICE").then(|| "1".to_string());
    let e = effective(&cfg, &off, Key::UiUpdateNotice);
    assert_eq!(e.value, "false");
    assert_eq!(e.origin, Origin::Env("HAMH_NO_UPDATE_NOTICE"));
    // Any other value of the variable is no layer: the default shows.
    let zero = |name: &str| (name == "HAMH_NO_UPDATE_NOTICE").then(|| "0".to_string());
    let e = effective(&cfg, &zero, Key::UiUpdateNotice);
    assert_eq!(e.value, "on");
    assert_eq!(e.origin, Origin::Default);
    assert!(!e.set);
}

// ---- the look as the event loop runs it ----------------------------------------------------------------------------------------------

/// The API the event loop is given. The look makes no call to it.
fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(10),
    })
}

fn config_with_source(source: &str) -> String {
    format!(
        "configVersion = 1\napiEndpoint = \"{ENDPOINT}\"\nregion = \"us-east-1\"\nupdateSource = \"{source}\"\n"
    )
}

#[tokio::test]
async fn b513_the_look_effect_answers_with_what_it_found_and_the_source_it_read() {
    let release = next(1);
    let repo = source("b513-effect-found", "main");
    annotated(&repo, &format!("v{release}"));
    let env = env_with(
        "b513-effect-found-cfg",
        Some(&config_with_source(&url(&repo))),
        &[],
    );
    let fake = Fake::start().await;
    let dir = support::scratch("b513-effect-found-dir");
    let msg = perform_with_env(
        &api(&fake),
        &dir,
        Effect::LookForUpdate {
            generation: 7,
            program: PathBuf::from("git"),
        },
        &env,
    )
    .await;
    let Some(Msg::UpdateLook {
        generation,
        outcome,
        source,
    }) = msg
    else {
        panic!("{msg:?}")
    };
    assert_eq!(
        generation, 7,
        "the answer carries the generation of its look"
    );
    assert_eq!(outcome, LookOutcome::Newest(release));
    assert_eq!(
        source,
        Some(url(&repo)),
        "the source the look read, for the page a click opens"
    );
}

/// The look effect of generation 3 with `program` as git, performed as the event loop does.
async fn look_effect(
    fake: &Fake,
    dir: &Path,
    program: &Path,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Option<Msg> {
    perform_with_env(
        &api(fake),
        dir,
        Effect::LookForUpdate {
            generation: 3,
            program: program.to_path_buf(),
        },
        env,
    )
    .await
}

#[tokio::test]
async fn b513_the_look_effect_answers_off_and_starts_no_process() {
    let dir = support::scratch("b513-effect-off");
    let record = dir.join("started.txt");
    let recorder = script(
        &dir,
        "recorder",
        &format!("#!/bin/sh\necho started >> {}\nexit 2\n", record.display()),
    );
    let mirror = "file:///srv/mirror/ham.git";
    let plain = config_with_source(mirror);
    let fake = Fake::start().await;
    let cases: Vec<OffCase> = vec![
        (
            "the variable switches the look off",
            plain.clone(),
            vec![("HAMH_NO_UPDATE_NOTICE", "1")],
        ),
        (
            "ui.update_notice is false",
            format!("{plain}\n[ui]\nupdate_notice = false\n"),
            vec![],
        ),
        (
            "update_source is off",
            "configVersion = 1\nupdateSource = \"off\"\n".to_string(),
            vec![],
        ),
    ];
    for (what, config, extra) in cases {
        let env = env_with("b513-effect-off-cfg", Some(&config), &extra);
        let msg = look_effect(&fake, &dir, &recorder, &env).await;
        assert!(
            matches!(
                msg,
                Some(Msg::UpdateLook {
                    generation: 3,
                    outcome: LookOutcome::Off,
                    source: None,
                })
            ),
            "{what}: {msg:?}"
        );
        assert!(!record.exists(), "{what}: the look started a process");
    }
    // The same program is started as soon as a look is allowed.
    let env = env_with("b513-effect-on-cfg", Some(&plain), &[]);
    let msg = look_effect(&fake, &dir, &recorder, &env).await;
    let Some(Msg::UpdateLook {
        outcome, source, ..
    }) = msg
    else {
        panic!("{msg:?}")
    };
    assert_eq!(outcome, LookOutcome::NoRelease, "the stand-in ends with 2");
    assert_eq!(source.as_deref(), Some(mirror));
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "started\n");
}

#[tokio::test]
async fn b513_a_config_that_cannot_be_read_is_a_look_that_failed() {
    let dir = support::scratch("b513-effect-unread");
    let record = dir.join("started.txt");
    let recorder = script(
        &dir,
        "recorder",
        &format!("#!/bin/sh\necho started >> {}\nexit 2\n", record.display()),
    );
    let plain = config_with_source("file:///srv/mirror/ham.git");
    let fake = Fake::start().await;
    // The file as an editor leaves it for a moment, and a file ham cannot take: neither says that the look is switched off.
    let cases: Vec<OffCase> = vec![
        (
            "a key that is half written",
            format!("{plain}\n[ui]\nupdate_no\n"),
            vec![],
        ),
        (
            "a config with a key ham does not know",
            format!("{plain}noSuchKey = 1\n"),
            vec![],
        ),
        ("a config that is not TOML", "= = =\n".to_string(), vec![]),
    ];
    let failed = |msg: &Option<Msg>| {
        matches!(
            msg,
            Some(Msg::UpdateLook {
                generation: 3,
                outcome: LookOutcome::Failed,
                source: None,
            })
        )
    };
    for (what, config, extra) in cases {
        let env = env_with("b513-effect-unread-cfg", Some(&config), &extra);
        let msg = look_effect(&fake, &dir, &recorder, &env).await;
        assert!(
            failed(&msg),
            "{what}: a look that failed is tried again in an hour and keeps what the last look found: {msg:?}"
        );
        assert!(!record.exists(), "{what}: the look started a process");
    }
    // No config location at all.
    let nowhere = |_: &str| -> Option<String> { None };
    let msg = look_effect(&fake, &dir, &recorder, &nowhere).await;
    assert!(failed(&msg), "{msg:?}");
    assert!(!record.exists());
}

// ---- the closed command, run ---------------------------------------------------------------------------------------------------------

/// The variable that makes this test binary run one look and end: `<git program>`, `<source>` and `<file for the outcome>`, a line each.
/// The two tests below start the binary again with it, so that the look runs in a process whose standard input and working folder the
/// test chose, as the terminal app's are the person's.
const CHILD: &str = "B513_LOOK_CHILD";

/// Runs the look the variable asks for and writes what it found; `false` when the variable is not set.
fn look_as_a_child() -> bool {
    let Ok(ask) = std::env::var(CHILD) else {
        return false;
    };
    let mut lines = ask.lines();
    let (program, source, out) = (
        lines.next().unwrap(),
        lines.next().unwrap(),
        lines.next().unwrap(),
    );
    let outcome = Git::with_program(program).look(source, Duration::from_secs(10));
    std::fs::write(out, format!("{outcome:?}\n")).unwrap();
    true
}

/// This test binary, to be started again for the test `name` alone: it runs one look of `source` with `program` as git and writes the
/// outcome to `out`.
fn child_look(name: &str, program: &Path, source: &str, out: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([name, "--exact", "--nocapture"])
        .env(
            CHILD,
            format!("{}\n{source}\n{}", program.display(), out.display()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

#[test]
fn b513_the_look_gives_git_no_standard_input() {
    use std::io::Write;
    use std::process::Stdio;
    if look_as_a_child() {
        return;
    }
    const TYPED: &[u8] = b"a line typed at the terminal\n";
    let dir = support::scratch("b513-look-stdin");
    // A git that reads a line of its standard input and writes what it got.
    let reader = |name: &str, record: &Path| {
        script(
            &dir,
            name,
            &format!(
                "#!/bin/sh\nif read line; then echo \"read: $line\" > {r}; else echo end > {r}; fi\nexit 2\n",
                r = record.display()
            ),
        )
    };
    // TRIGGER PRESENT: started with a standard input that holds a line, the stand-in reads the line and says so.
    let direct_record = dir.join("direct.txt");
    let mut direct = std::process::Command::new(reader("direct", &direct_record))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    direct.stdin.take().unwrap().write_all(TYPED).unwrap();
    direct.wait().unwrap();
    assert_eq!(
        std::fs::read_to_string(&direct_record).unwrap(),
        "read: a line typed at the terminal\n"
    );
    // The look runs in a process whose standard input holds that line, as the terminal holds what the person types into the app.
    let (record, out) = (dir.join("look.txt"), dir.join("outcome.txt"));
    let mut child = child_look(
        "b513_the_look_gives_git_no_standard_input",
        &reader("git", &record),
        "file:///srv/mirror/ham.git",
        &out,
    )
    .stdin(Stdio::piped())
    .spawn()
    .unwrap();
    child.stdin.take().unwrap().write_all(TYPED).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "NoRelease\n",
        "the stand-in ran as the look's git and ended with 2"
    );
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "end\n",
        "the look's git has no standard input: it never reads what is typed into the app"
    );
}

#[test]
fn b513_the_look_reads_no_configuration_of_the_folder_it_is_started_in() {
    if look_as_a_child() {
        return;
    }
    let (release, higher) = (next(1), next(5));
    // The update source, and a second repository that names a higher release.
    let real = source("b513-look-rewrite-source", "main");
    annotated(&real, &format!("v{release}"));
    let second = source("b513-look-rewrite-second", "main");
    annotated(&second, &format!("v{higher}"));
    // A repository whose own configuration rewrites the address of the source to the second repository.
    let work = source("b513-look-rewrite-work", "main");
    git(
        &work,
        &[
            "config",
            &format!("url.{}.insteadOf", url(&second)),
            &url(&real),
        ],
    );
    // TRIGGER PRESENT: a plain git started in that folder follows the rewrite and lists the tag of the second repository.
    let listed = git(&work, &["ls-remote", "--tags", &url(&real)]);
    assert!(listed.contains(&format!("refs/tags/v{higher}")), "{listed}");
    assert!(
        !listed.contains(&format!("refs/tags/v{release}")),
        "{listed}"
    );
    // The look of an app that was started in that folder reads the source it was given, with the git of the machine.
    let out = support::scratch("b513-look-rewrite-out").join("outcome.txt");
    let status = child_look(
        "b513_the_look_reads_no_configuration_of_the_folder_it_is_started_in",
        Path::new("git"),
        &url(&real),
        &out,
    )
    .current_dir(&work)
    .stdin(std::process::Stdio::null())
    .status()
    .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{:?}\n", LookOutcome::Newest(release))
    );
}

#[test]
fn b513_the_look_hands_git_no_program_to_ask_for_a_login() {
    if look_as_a_child() {
        return;
    }
    let dir = support::scratch("b513-look-askpass");
    // A git that writes down the variables that decide whether git may start a program, or use the terminal, to ask for a login, and
    // the one that makes a library write a file.
    let recorder = |name: &str, record: &Path| {
        script(
            &dir,
            name,
            &format!(
                "#!/bin/sh\nfor name in GIT_ASKPASS SSH_ASKPASS SSLKEYLOGFILE GIT_TERMINAL_PROMPT; do\n  if printenv \"$name\" > /dev/null; then\n    printf '%s is set to [' \"$name\"; printenv \"$name\" | tr -d '\\n'; echo ']'\n  else\n    echo \"$name is not set\"\n  fi\ndone > {}\nexit 2\n",
                record.display()
            ),
        )
    };
    // The app's environment names a program to ask for a login in both variables, a file for the keys, and allows the terminal prompt.
    let ask = "/stand/in/ask-for-a-login";
    let keys = dir.join("keys.log");
    let app_env = |cmd: &mut std::process::Command| {
        cmd.env("GIT_ASKPASS", ask)
            .env("SSH_ASKPASS", ask)
            .env("SSLKEYLOGFILE", &keys)
            .env("GIT_TERMINAL_PROMPT", "1")
            .stdin(std::process::Stdio::null());
    };
    // TRIGGER PRESENT: started in that environment as it is, the stand-in sees all four.
    let direct_record = dir.join("direct.txt");
    let mut direct = std::process::Command::new(recorder("direct", &direct_record));
    app_env(&mut direct);
    direct.status().unwrap();
    assert_eq!(
        std::fs::read_to_string(&direct_record).unwrap(),
        format!(
            "GIT_ASKPASS is set to [{ask}]\nSSH_ASKPASS is set to [{ask}]\nSSLKEYLOGFILE is set to [{}]\nGIT_TERMINAL_PROMPT is set to [1]\n",
            keys.display()
        )
    );
    // The look of an app with that environment: its git is told no program, may not use the terminal, and is given no file for keys.
    let (record, out) = (dir.join("look.txt"), dir.join("outcome.txt"));
    let mut child = child_look(
        "b513_the_look_hands_git_no_program_to_ask_for_a_login",
        &recorder("git", &record),
        "file:///srv/mirror/ham.git",
        &out,
    );
    app_env(&mut child);
    assert!(child.status().unwrap().success());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "NoRelease\n");
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "GIT_ASKPASS is set to []\nSSH_ASKPASS is not set\nSSLKEYLOGFILE is not set\nGIT_TERMINAL_PROMPT is set to [0]\n"
    );
}

/// The time this thread has spent ready to run and waiting for a processor: the second number of `/proc/thread-self/schedstat`
/// (Linux). `None` where the machine has no such file.
fn waited_for_a_processor() -> Option<Duration> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let nanos = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

/// One look whose git takes 2 seconds, performed as the event loop performs it on a runtime with one thread, beside a 10 ms ticker: the
/// largest gap between two ticks, and the number of ticks.
///
/// A gap is read as the draw bound of `tui_w2` reads its time: the wall clock less the time the loop's thread was ready to run and waited
/// for a processor, where the machine reports that wait. On a build host where every processor is taken, that wait says nothing about
/// the look. A look that held the loop's thread would not wait for a processor: the thread would sleep until git ended, and the gap
/// would be the 2 seconds of the look.
async fn ticks_beside_a_slow_look(
    api: &Api<HttpsPlane>,
    dir: &Path,
    slow: &Path,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> (Duration, u32) {
    let started = Instant::now();
    let look = perform_with_env(
        api,
        dir,
        Effect::LookForUpdate {
            generation: 0,
            program: slow.to_path_buf(),
        },
        env,
    );
    tokio::pin!(look);
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut last, mut worst, mut ticks) = (Instant::now(), Duration::ZERO, 0u32);
    let mut waited_at_last = waited_for_a_processor();
    let msg = loop {
        tokio::select! {
            msg = &mut look => break msg,
            _ = ticker.tick() => {
                // A gap ends with the wait read first and the wall clock second, and the next gap starts with the wait read once more
                // after the wall clock: a wait for a processor that falls between two readings is then counted in a gap, at either end.
                let waited = waited_for_a_processor();
                let now = Instant::now();
                let for_a_processor = match (waited_at_last, waited) {
                    (Some(before), Some(after)) => after.saturating_sub(before),
                    _ => Duration::ZERO,
                };
                worst = worst.max(now.duration_since(last).saturating_sub(for_a_processor));
                last = now;
                waited_at_last = waited_for_a_processor();
                ticks += 1;
            }
        }
    };
    let took = started.elapsed();
    assert!(
        matches!(
            msg,
            Some(Msg::UpdateLook {
                outcome: LookOutcome::NoRelease,
                ..
            })
        ),
        "{msg:?}"
    );
    assert!(
        took >= Duration::from_millis(1_900),
        "the look took its 2 seconds: {took:?}"
    );
    (worst, ticks)
}

/// The bound of what the look costs the work in front of it, held with every build: on a runtime with one thread, a look whose git takes
/// 2 seconds runs beside a 10 ms ticker, the ticker makes at least 50 ticks and the largest gap between two ticks stays under 200 ms.
/// The git call is on a blocking thread; the loop's thread only waits for its answer.
///
/// The bound must hold beside one of three looks. A look that held the loop's thread would hold it every time, for the 2 seconds of its
/// git, so it fails all three. A pause of the machine is not the look's and does not repeat. On a build host whose processors and disk
/// other builds had taken, a run of every test of the client measured one gap of 401 ms beside 186 ticks of a possible 200, with the wait
/// for a processor already left out: the loop ran, and the machine paused it once.
#[tokio::test(flavor = "current_thread")]
async fn b513_a_slow_look_never_holds_the_loops_thread() {
    let dir = support::scratch("b513-effect-slow");
    let slow = script(&dir, "slow", "#!/bin/sh\nsleep 2\nexit 2\n");
    let env = env_with(
        "b513-effect-slow-cfg",
        Some(&config_with_source("file:///srv/mirror/ham.git")),
        &[],
    );
    let fake = Fake::start().await;
    let api = api(&fake);
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (worst, ticks) = ticks_beside_a_slow_look(&api, &dir, &slow, &env).await;
        if ticks >= 50 && worst < Duration::from_millis(200) {
            return;
        }
        seen.push((worst, ticks));
    }
    panic!(
        "beside each of three 2 s looks a 10 ms ticker made fewer than 50 ticks or its largest gap was 200 ms or more: {seen:?} (largest gap, ticks; a gap is the wall clock less the loop thread's wait for a processor where the machine reports it)"
    );
}

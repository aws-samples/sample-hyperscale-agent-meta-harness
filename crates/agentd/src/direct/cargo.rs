//! The engine's cargo settings of a direct run: where cargo keeps its files, how crates.io is replaced by the
//! loopback package proxy, and the switches that suit a session of 2 vCPU and 8 GB.
//!
//! The worker image carries the Rust toolchain (`rustc`, `cargo`, clippy, rustfmt, nextest) and no registry credential. The package
//! proxy (`pkgproxy`, prefix `cargo`) holds the CodeArtifact token and answers the sparse registry that `config.toml` names, so the
//! engine's `cargo fetch`, `build`, `test`, `nextest` and `clippy` resolve crates.io dependencies without a token of their own.
//!
//! * [`home_dir`] and [`place_config`]: a `.cargo` directory in the engine's home with the proxy's `config.toml`, named by `CARGO_HOME`.
//!   This needs a running proxy and is skipped without one. A project's own `.cargo/config.toml` in its tree still applies on top (cargo
//!   merges the files), and `git = "..."` dependencies fail at the egress list.
//! * [`build_env`] and [`build_env_with`]: the build switches, which hold nothing of the network and are set in every run: `CARGO_BUILD_JOBS`
//!   (the number of vCPUs of a session, `AGENTD_DIRECT_CARGO_JOBS` changes it), no debug information in the dev and test profiles (smaller
//!   targets and faster links), no incremental state, a backtrace for a failing test, and cargo's own git client. The debug-information and
//!   incremental defaults are the user's to turn off (`run.cargo_no_debug_info`, `run.cargo_no_incremental`, request member `build`). The
//!   standalone `cargo` ignores a `rust-toolchain.toml` (it has no rustup to ask), so a project that pins a channel builds with the installed one.
//! * [`TargetSetting`], [`target_dir`] and [`place_target_dir`]: the target folder. The checkout is on the session's managed storage,
//!   which holds 1 GiB and reports no use, so `target/` there fills it unseen. `CARGO_TARGET_DIR` names a folder on the root volume outside
//!   the checkout instead (`AGENTD_CARGO_TARGET_DIR`: another absolute folder, or `checkout` for cargo's own default). The report steps build
//!   in a clean checkout that is already on the root volume and do not receive the variable.

use harness_protocol::direct::BuildOptions;
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat, renameat, statat,
    unlinkat,
};
use rustix::io::Errno;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, fchown};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `CARGO_BUILD_JOBS` of a session unless the deployment says otherwise: a session has 2 vCPU.
pub const DEFAULT_JOBS: u32 = 2;
/// The most jobs `AGENTD_DIRECT_CARGO_JOBS` can ask for: a larger number is this one.
pub const MAX_JOBS: u32 = 64;

/// `$CARGO_HOME` of the engine: `.cargo` in its home directory (where cargo looks when the variable is unset).
pub fn home_dir(engine_home: &Path) -> PathBuf {
    engine_home.join(".cargo")
}

/// The build switches for a small session, with `jobs` parallel jobs.
pub fn build_env(jobs: u32) -> BTreeMap<String, String> {
    build_env_with(jobs, &BuildOptions::default())
}

/// [`build_env`] with the user's switches: `cargoNoDebugInfo: false` leaves `CARGO_PROFILE_DEV_DEBUG` and
/// `CARGO_PROFILE_TEST_DEBUG` unset and `cargoNoIncremental: false` leaves `CARGO_INCREMENTAL` unset, so cargo and the repository's own
/// configuration decide. The job count, the backtrace and cargo's git client are not switches.
pub fn build_env_with(jobs: u32, build: &BuildOptions) -> BTreeMap<String, String> {
    let mut env = vec![("CARGO_BUILD_JOBS", jobs.to_string())];
    if build.cargo_no_debug_info() {
        env.push(("CARGO_PROFILE_DEV_DEBUG", "0".to_string()));
        env.push(("CARGO_PROFILE_TEST_DEBUG", "0".to_string()));
    }
    if build.cargo_no_incremental() {
        env.push(("CARGO_INCREMENTAL", "0".to_string()));
    }
    env.push(("RUST_BACKTRACE", "1".to_string()));
    env.push(("CARGO_NET_GIT_FETCH_WITH_CLI", "false".to_string()));
    env.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The name of the target folder beside the run's other agentd folders (`/var/lib/agentd/<run>/direct/cargo-target`).
pub const TARGET_FOLDER: &str = "cargo-target";

/// Where the engine's cargo keeps its `target/` folder.
///
/// The checkout is on the session's managed storage, which holds 1 GiB and reports no use, so a `target/` there fills it without a sign
/// (one `cargo test --workspace --no-run` of a large Rust workspace can fill it, then `No space left on device`). The root volume has more room,
/// so the default is a folder there, outside the checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetSetting {
    /// The default: the folder [`TARGET_FOLDER`] beside the run's other agentd folders, on the root volume.
    Default,
    /// `checkout`: cargo's own default, a `target/` in the checkout (what a repository's scripts may expect).
    Checkout,
    /// An absolute folder the deployment names.
    Dir(PathBuf),
}

impl TargetSetting {
    /// The setting a value of `AGENTD_CARGO_TARGET_DIR` means. Unset, blank or anything invalid is the default; `checkout` (any case) is
    /// [`TargetSetting::Checkout`]; an absolute path made of plain names only (no `.` or `..` part, not the root, no NUL) is that folder.
    pub fn parse(value: Option<&str>) -> Self {
        let v = value.map_or("", str::trim);
        if v.is_empty() {
            return Self::Default;
        }
        if v.eq_ignore_ascii_case("checkout") {
            return Self::Checkout;
        }
        let path = Path::new(v);
        let plain = path.is_absolute()
            && !v.contains('\0')
            && v.split('/').all(|part| part != "." && part != "..")
            && path.components().count() > 1;
        if plain {
            Self::Dir(path.components().collect())
        } else {
            Self::Default
        }
    }
}

/// The folder cargo's target goes to for `setting`, or `None` when cargo keeps its own default (`checkout`). `agent_data` is the run's
/// agentd folder (`/var/lib/agentd/<run>/direct`), on the root volume.
pub fn target_dir(setting: &TargetSetting, agent_data: &Path) -> Option<PathBuf> {
    match setting {
        TargetSetting::Default => Some(agent_data.join(TARGET_FOLDER)),
        TargetSetting::Checkout => None,
        TargetSetting::Dir(dir) => Some(dir.clone()),
    }
}

/// Makes the target folder `dir` and returns the variable that names it for every cargo call of the engine's session (`CARGO_TARGET_DIR`;
/// the environment beats a project's `build.target-dir`, and a call with `--target-dir` still wins).
///
/// agentd runs as root and a run's folders can hold what an earlier invocation left, so the folder is opened from its parent without
/// following a link: a link or a file at its name is an error (not even a link that points into the checkout is written through), a folder
/// of the parent's owner, agentd's or the engine's is kept with what is in it (the build cache survives a second invocation) and
/// an absent one is made with mode 0755. `owner` is the engine's `(uid, gid)` when agentd drops privileges: the folder is given to it so
/// its cargo can write there. A missing parent is made first.
pub fn place_target_dir(
    dir: &Path,
    owner: Option<(u32, u32)>,
) -> io::Result<BTreeMap<String, String>> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(refused("the target folder has no parent directory"));
    };
    std::fs::create_dir_all(parent)?;
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let base = openat(CWD, parent, dir_flags, Mode::empty()).map_err(errno)?;
    let base_uid = fstat(&base).map_err(errno)?.st_uid;
    let mut allowed = vec![base_uid, process_uid().unwrap_or(base_uid)];
    allowed.extend(owner.map(|(uid, _)| uid));
    let mut made = false;
    match statat(&base, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {}
        Ok(_) => return Err(refused("the target folder is not a plain directory")),
        Err(Errno::NOENT) => {
            mkdirat(&base, name, Mode::from_raw_mode(0o755)).map_err(errno)?;
            made = true;
        }
        Err(e) => return Err(errno(e)),
    }
    // Opened without following a link: a name swapped since the check above is an error here, and what is checked next is the folder
    // this handle is, not a name.
    let folder = openat(&base, name, dir_flags, Mode::empty()).map_err(errno)?;
    let st = fstat(&folder).map_err(errno)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory || !allowed.contains(&st.st_uid) {
        return Err(refused(
            "the target folder is not a plain directory of the expected owner",
        ));
    }
    if made {
        // The mask of agentd's process must not decide whether the engine can enter it.
        fchmod(&folder, Mode::from_raw_mode(0o755)).map_err(errno)?;
    }
    if let Some((uid, gid)) = owner {
        fchown(&folder, Some(uid), Some(gid))?;
    }
    Ok(BTreeMap::from([(
        "CARGO_TARGET_DIR".to_string(),
        dir.display().to_string(),
    )]))
}

/// Writes `config_toml` (the proxy's, which holds no secret) as `config.toml` in the cargo home `cargo_home`, which is `.cargo` in the engine's
/// home, and returns the variable that names it. `owner` is the engine's `(uid, gid)` when agentd drops privileges: the directory and the
/// file are given to it, so the engine's cargo can make `registry/`, `git/` and `.package-cache` there.
///
/// agentd runs as root and the engine's home can hold what the engine left in an earlier invocation of the same run, so nothing is written
/// through a name the engine could have made a link: the home is opened without following a link, `.cargo` must be a plain directory (it is made
/// when missing) and is opened from the home without following one, `config.toml` must be missing or a plain file, and the owner of each must be
/// the home's, agentd's or the engine's. The text is written to a new file in the directory (`O_EXCL`, so a planted name is an error) and
/// renamed over `config.toml`, which replaces a hard link instead of writing through it. A refusal is an error and changes nothing.
pub fn place_config(
    cargo_home: &Path,
    config_toml: &str,
    owner: Option<(u32, u32)>,
) -> io::Result<BTreeMap<String, String>> {
    let (Some(parent), Some(name)) = (cargo_home.parent(), cargo_home.file_name()) else {
        return Err(refused("the cargo home has no parent directory"));
    };
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let home = openat(CWD, parent, dir_flags, Mode::empty()).map_err(errno)?;
    let home_uid = fstat(&home).map_err(errno)?.st_uid;
    // Who may own what is there: whoever owns the home (agentd made it, or gave it to the engine), agentd itself and the engine.
    let mut allowed = vec![home_uid, process_uid().unwrap_or(home_uid)];
    allowed.extend(owner.map(|(uid, _)| uid));

    let mut made = false;
    match statat(&home, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {}
        Ok(_) => return Err(refused("the cargo directory is not a plain directory")),
        Err(Errno::NOENT) => {
            mkdirat(&home, name, Mode::from_raw_mode(0o755)).map_err(errno)?;
            made = true;
        }
        Err(e) => return Err(errno(e)),
    }
    // Opened without following a link: a name the engine swapped since the check above is an error here, and what is checked next is the
    // directory this handle is, not a name.
    let dir = openat(&home, name, dir_flags, Mode::empty()).map_err(errno)?;
    let st = fstat(&dir).map_err(errno)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory || !allowed.contains(&st.st_uid) {
        return Err(refused(
            "the cargo directory is not a plain directory of the expected owner",
        ));
    }
    if made {
        // The mask of agentd's process must not decide whether the engine can enter it.
        fchmod(&dir, Mode::from_raw_mode(0o755)).map_err(errno)?;
    }
    match statat(&dir, CONFIG_NAME, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st)
            if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
                && allowed.contains(&st.st_uid) => {}
        Ok(_) => {
            return Err(refused(
                "config.toml is not a plain file of the expected owner",
            ));
        }
        Err(Errno::NOENT) => {}
        Err(e) => return Err(errno(e)),
    }

    let temp = temp_name();
    let fd = openat(
        &dir,
        temp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(errno)?;
    let written = write_temp(fd, config_toml, owner)
        .and_then(|()| renameat(&dir, temp.as_str(), &dir, CONFIG_NAME).map_err(errno));
    if let Err(e) = written {
        // The file is agentd's own, made a moment ago under a name no one else knows.
        let _ = unlinkat(&dir, temp.as_str(), AtFlags::empty());
        return Err(e);
    }
    if let Some((uid, gid)) = owner {
        fchown(&dir, Some(uid), Some(gid))?;
    }
    Ok(BTreeMap::from([(
        "CARGO_HOME".to_string(),
        cargo_home.display().to_string(),
    )]))
}

const CONFIG_NAME: &str = "config.toml";

/// Fills the new file: its owner, its mode (whatever the process mask), then the text.
fn write_temp(fd: OwnedFd, text: &str, owner: Option<(u32, u32)>) -> io::Result<()> {
    let mut file = File::from(fd);
    if let Some((uid, gid)) = owner {
        fchown(&file, Some(uid), Some(gid))?;
    }
    fchmod(&file, Mode::from_raw_mode(0o644)).map_err(errno)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

/// A name for the new file that no one has made: the process, the clock and a counter.
fn temp_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        ".config.toml.{}.{nanos}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The effective user of this process (the owner of `/proc/self`).
fn process_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

fn errno(e: Errno) -> io::Error {
    io::Error::from(e)
}

fn refused(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what.to_string())
}

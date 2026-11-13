//! Path confinement of the `code_search` tool.
//!
//! What a model names in `paths` is resolved before ripgrep sees it: against the work tree when it is relative, and against the work tree or
//! the context directory (`HAMH_CONTEXT_DIR`, outside the work tree) when it is absolute. The result is a canonical absolute path that lies
//! beneath one of the two roots, or a refusal. Ripgrep is then given that canonical path and nothing else from the caller.
//!
//! The rule is the kernel's own `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` ("strict beneath"), and both implementations obey it, so a result
//! never depends on which one ran:
//! - the entry is resolved one component at a time inside ONE root; `..` may go up as far as that root and no further;
//! - a symbolic link is followed when its target is relative and stays beneath the root; an absolute target is refused even when it points
//!   back inside (the kernel refuses it too), and so is a loop (more than 40 links);
//! - a link that leads from one root into the other is refused: an entry names a file of the work tree or of the context directory, and the
//!   context directory is named by its absolute path;
//! - a component that is not a directory cannot be traversed (`file/..` is a missing path, as it is for the kernel), and a trailing `/`
//!   needs a directory.
//!
//! The two implementations:
//! - [`Strategy::Openat2`]: `openat2(root, entry, O_PATH, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`, and the resolved path is read back from
//!   `/proc/self/fd/N`. The kernel does the resolution, so a link swapped in while the call runs cannot lead out of the root. Available from
//!   Linux 5.6; a seccomp filter or a missing `/proc` makes it unavailable (not an error: [`openat2_available`] says so, and `Auto` falls back).
//! - [`Strategy::Walk`]: the same rule in user space (`symlink_metadata` per component). Used where `openat2` is not, and by the tests as the
//!   reference for the kernel.
//!
//! What this is and is not. Ripgrep runs as the engine user, whose own shell reads everything ripgrep can, so the confinement is the tool's
//! contract and a second line of defence, not a privilege boundary. The gap between the check and ripgrep's own open of the canonical path is
//! accepted for the same reason. Nothing here names a path in an error: a [`Refusal`] is a word.

use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};
use rustix::io::Errno;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// How many symbolic links one entry may pass through (the kernel's own limit).
const MAX_LINKS: usize = 40;

/// Which implementation resolves an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// `openat2` where the kernel offers it, else the walk.
    Auto,
    /// `openat2` only: [`Refusal::NoOpenat2`] where it is not available.
    Openat2,
    /// The user-space walk only.
    Walk,
}

/// Why an entry was not accepted. A word, never the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The entry leaves the root it belongs to (through `..`, a link or an absolute path), or belongs to neither root.
    Outside,
    /// There is no such file or directory (also: a file used as a directory, a trailing `/` on a file).
    Missing,
    /// The path exists and cannot be opened (permissions, an I/O error).
    Unreadable,
    /// [`Strategy::Openat2`] was asked for and the kernel does not offer it.
    NoOpenat2,
}

/// The root an accepted entry lies beneath.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Work,
    Context,
}

/// An accepted entry: a canonical absolute path and the root it is beneath.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub root: RootKind,
}

#[derive(Clone, Debug)]
struct Root {
    /// The root with every link resolved.
    canonical: PathBuf,
    /// The root as the run named it (an absolute entry may use either form).
    given: PathBuf,
}

/// The work tree and, when the run has one, the context directory.
#[derive(Clone, Debug)]
pub struct Roots {
    work: Root,
    context: Option<Root>,
}

impl Roots {
    /// The roots of a run. The work tree must exist. A context directory that does not exist is no root.
    pub fn new(work_tree: &Path, context_dir: Option<&Path>) -> std::io::Result<Self> {
        let root = |p: &Path| -> std::io::Result<Root> {
            Ok(Root {
                canonical: std::fs::canonicalize(p)?,
                given: p.to_path_buf(),
            })
        };
        Ok(Self {
            work: root(work_tree)?,
            context: context_dir.and_then(|d| root(d).ok()),
        })
    }

    /// The work tree, canonical.
    pub fn work(&self) -> &Path {
        &self.work.canonical
    }

    /// The context directory, canonical, when there is one.
    pub fn context(&self) -> Option<&Path> {
        self.context.as_ref().map(|c| c.canonical.as_path())
    }

    /// The root an absolute entry belongs to and the rest of the entry below it. The comparison is by components (`/work-evil` is not under
    /// `/work`); `..` inside the rest is judged by the resolution, not here.
    fn pick(&self, entry: &Path) -> Option<(RootKind, &Root, PathBuf)> {
        let candidates = [
            (RootKind::Work, Some(&self.work)),
            (RootKind::Context, self.context.as_ref()),
        ];
        for (kind, root) in candidates {
            let Some(root) = root else { continue };
            for base in [&root.given, &root.canonical] {
                if let Ok(rest) = entry.strip_prefix(base) {
                    return Some((kind, root, rest.to_path_buf()));
                }
            }
        }
        None
    }

    /// Resolves `entry` (the text a model sent in `paths`).
    pub fn resolve(&self, entry: &str, strategy: Strategy) -> Result<Resolved, Refusal> {
        let wants_dir = entry.ends_with('/');
        let path = Path::new(entry);
        let (kind, root, rest) = if path.is_absolute() {
            self.pick(path).ok_or(Refusal::Outside)?
        } else {
            (RootKind::Work, &self.work, path.to_path_buf())
        };
        let rest = if rest.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rest
        };
        let resolved = match strategy {
            Strategy::Walk => walk_beneath(&root.canonical, &rest)?,
            Strategy::Openat2 => {
                openat2_beneath(&root.canonical, &rest)?.ok_or(Refusal::NoOpenat2)?
            }
            Strategy::Auto => match openat2_beneath(&root.canonical, &rest)? {
                Some(p) => p,
                None => walk_beneath(&root.canonical, &rest)?,
            },
        };
        // The kernel has said it is beneath the root; this is the same fact read from the path we are about to hand on.
        if !resolved.starts_with(&root.canonical) {
            return Err(Refusal::Outside);
        }
        if wants_dir && !resolved.is_dir() {
            return Err(Refusal::Missing);
        }
        Ok(Resolved {
            path: resolved,
            root: kind,
        })
    }
}

/// Whether `openat2` with `RESOLVE_BENEATH` works here. Asked once per process.
pub fn openat2_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(root) = open(
            "/",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return false;
        };
        openat2(
            &root,
            ".",
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        )
        .is_ok()
    })
}

/// The kernel's resolution of `rest` beneath `root` (a canonical directory). `Ok(None)`: not available here, or its answer cannot be read
/// back; the caller walks instead.
fn openat2_beneath(root: &Path, rest: &Path) -> Result<Option<PathBuf>, Refusal> {
    let dir = match open(
        root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT | Errno::NOTDIR) => return Err(Refusal::Unreadable),
        Err(_) => return Ok(None),
    };
    match openat2(
        &dir,
        rest,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(fd) => {
            let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok();
            Ok(link.filter(|p| p.is_absolute() && !p.to_string_lossy().ends_with(" (deleted)")))
        }
        // The system call itself is not there, or a filter does not allow it, or the kernel does not know a flag.
        Err(Errno::NOSYS | Errno::PERM | Errno::INVAL) => Ok(None),
        Err(Errno::XDEV | Errno::LOOP) => Err(Refusal::Outside),
        Err(Errno::NOENT | Errno::NOTDIR) => Err(Refusal::Missing),
        Err(_) => Err(Refusal::Unreadable),
    }
}

/// One component still to be resolved.
enum Part {
    Current,
    Parent,
    Name(OsString),
}

fn parts(p: &Path) -> Vec<Part> {
    p.components()
        .filter_map(|c| match c {
            Component::CurDir => Some(Part::Current),
            Component::ParentDir => Some(Part::Parent),
            Component::Normal(n) => Some(Part::Name(n.to_os_string())),
            // The caller hands over a relative path; a root or a drive here would be an absolute link target, refused before this.
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// The rule of the module text in user space.
fn walk_beneath(root: &Path, rest: &Path) -> Result<PathBuf, Refusal> {
    let mut stack: Vec<OsString> = Vec::new();
    let mut todo: VecDeque<Part> = parts(rest).into();
    let mut links = 0usize;
    while let Some(part) = todo.pop_front() {
        match part {
            Part::Current => {}
            Part::Parent => {
                if stack.pop().is_none() {
                    return Err(Refusal::Outside);
                }
            }
            Part::Name(name) => {
                let mut here = root.to_path_buf();
                here.extend(&stack);
                here.push(&name);
                let meta =
                    std::fs::symlink_metadata(&here).map_err(|e| {
                        match Errno::from_io_error(&e) {
                            Some(Errno::NOENT | Errno::NOTDIR) => Refusal::Missing,
                            _ => Refusal::Unreadable,
                        }
                    })?;
                if meta.file_type().is_symlink() {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(Refusal::Outside);
                    }
                    let target = std::fs::read_link(&here).map_err(|_| Refusal::Unreadable)?;
                    if target.is_absolute() {
                        return Err(Refusal::Outside);
                    }
                    for p in parts(&target).into_iter().rev() {
                        todo.push_front(p);
                    }
                } else {
                    if !todo.is_empty() && !meta.is_dir() {
                        return Err(Refusal::Missing);
                    }
                    stack.push(name);
                }
            }
        }
    }
    let mut out = root.to_path_buf();
    out.extend(&stack);
    Ok(out)
}

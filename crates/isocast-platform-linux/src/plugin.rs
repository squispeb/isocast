//! Running process plugins on Linux (L.6g): starting them under a Landlock
//! sandbox, and telling which process is at the other end of a socket
//! connection (`SO_PEERCRED`).
//!
//! Landlock is the unprivileged, per-process access control the kernel
//! ships (5.13 and later; `CONFIG_SECURITY_LANDLOCK` and `lsm=landlock`).
//! Where macOS's Seatbelt profile says "allow everything, deny the home
//! folder", Landlock only allows: the ruleset names what the plugin may
//! reach, and everything else is refused with `EACCES`. The syscalls are
//! called directly (three of them, and three small structs): the `landlock`
//! crate would be a new dependency and trails the kernel's ABI.
//!
//! Every right is masked to what the running kernel's ABI knows, so an old
//! kernel gives a weaker sandbox rather than an error; [`gaps`] says what is
//! missing, and [`sandbox_available`] is false below ABI 4 (no network
//! rules), where plugins run unsandboxed and `doctor` says so.
//!
//! The ruleset is built in the parent, and the child only makes two raw
//! syscalls between fork and exec (`PR_SET_NO_NEW_PRIVS` and
//! `landlock_restrict_self`), so nothing allocates or locks after the fork.

use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Filesystem rights (`LANDLOCK_ACCESS_FS_*`) and the ABI that added each.
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
/// ABI 2: moving or linking a file into another directory.
const FS_REFER: u64 = 1 << 13;
/// ABI 3.
const FS_TRUNCATE: u64 = 1 << 14;
/// ABI 5: ioctl on device files.
const FS_IOCTL_DEV: u64 = 1 << 15;
/// ABI 9: connecting to a pathname unix socket.
const FS_RESOLVE_UNIX: u64 = 1 << 16;

/// The rights a rule on a file (not a folder) may hold.
const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
const READ: u64 = FS_READ_FILE | FS_READ_DIR;
const READ_EXEC: u64 = READ | FS_EXECUTE;
/// Everything to do with changing files, in a folder the plugin owns.
const WRITE_TREE: u64 = FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_SYM
    | FS_REFER
    | FS_TRUNCATE;
/// What a `write` recordings grant adds to reading (see [`Sandbox::recordings`]).
/// Without `WRITE_FILE` a file it makes can't be written (tried: the file is
/// created empty and the open fails), so existing files are writable too.
const CREATE: u64 = FS_MAKE_REG | FS_MAKE_DIR | FS_WRITE_FILE | FS_TRUNCATE;
/// Rights this module can ask for: devices' file rights aside, the rest.
const ALL_FS: u64 = FS_EXECUTE
    | FS_WRITE_FILE
    | FS_READ_FILE
    | FS_READ_DIR
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM
    | FS_REFER
    | FS_TRUNCATE
    | FS_IOCTL_DEV
    | FS_RESOLVE_UNIX;

/// ABI 4: TCP connect (and bind).
const NET_CONNECT_TCP: u64 = 1 << 1;
const ALL_NET: u64 = NET_CONNECT_TCP;

/// ABI 6: abstract unix sockets and signals outside the sandbox.
const SCOPE_ABSTRACT_UNIX: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
const ALL_SCOPE: u64 = SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL;

/// `landlock_create_ruleset` flag: return the ABI version.
const CREATE_RULESET_VERSION: u32 = 1;
/// `landlock_add_rule` type for a path rule.
const RULE_PATH_BENEATH: c_int = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The newest Landlock ABI this kernel speaks; 0 when Landlock is off
/// (not built in, not in the `lsm=` list, or blocked by a container).
pub fn abi() -> u32 {
    static ABI: OnceLock<u32> = OnceLock::new();
    *ABI.get_or_init(|| {
        // SAFETY: with a null attribute and the version flag the syscall
        // only returns a number.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        abi.max(0) as u32
    })
}

/// The least ABI a sandbox is made with: the network rules (4) keep a
/// plugin off the network unless it was granted that.
const MIN_ABI: u32 = 4;

/// Whether plugins can be sandboxed on this machine.
pub fn sandbox_available() -> bool {
    abi() >= MIN_ABI
}

/// What this kernel's sandbox doesn't hold back, for `doctor`: what an
/// older ABI lacks, and what Landlock never covers.
pub fn gaps() -> Vec<&'static str> {
    let abi = abi();
    let mut gaps = Vec::new();
    if abi < 5 {
        gaps.push("ioctl on devices isn't limited (Landlock ABI 5)");
    }
    if abi < 6 {
        gaps.push("abstract unix sockets and signals aren't limited (Landlock ABI 6)");
    }
    if abi < 9 {
        gaps.push("it can connect to any unix socket its files allow (Landlock ABI 9)");
    }
    gaps.push("UDP, DNS and raw sockets aren't limited, so a `network` denial is TCP only");
    gaps
}

/// What a plugin's process may touch. Paths are canonical (resolved);
/// one that doesn't exist is left out of the ruleset.
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    /// Files and folders it may read and run programs from: its program,
    /// its own files, its interpreter's installation. It may run nothing
    /// else, as on macOS, but for the dynamic loader the program needs
    /// (the system's `/usr`, `/lib`, `/etc` ... are readable, not
    /// executable).
    pub read_exec: Vec<PathBuf>,
    /// Files and folders it may only read: its own plugin folder and the
    /// files it was granted.
    pub read: Vec<PathBuf>,
    /// Folders it may change (its data folder).
    pub write: Vec<PathBuf>,
    /// Folders in which it may connect to unix sockets: the folder of its
    /// socket to isocastd, and so no other socket (the main one, the
    /// compositor's, PipeWire's, D-Bus).
    pub sockets: Vec<PathBuf>,
    /// It may connect out over TCP. Without it, it can only serve.
    pub network: bool,
    /// The recordings folders and what it may do in them. Landlock has no
    /// patterns, so `Write` is a rule on each folder that lets it make
    /// files and folders in any of its subfolders (and write existing
    /// ones: neither the engine's own `.session.json` and `.partial` files
    /// nor existing media are shielded as they are on macOS), without
    /// removing, renaming out or linking.
    pub recordings: Vec<PathBuf>,
    pub recordings_access: Recordings,
}

/// What a plugin may do in the recordings folders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Recordings {
    /// Nothing: not even read.
    #[default]
    None,
    Read,
    /// Read, and make files and folders.
    Write,
}

/// Folders every plugin may read: libraries, interpreters' libraries,
/// certificates, `ld.so.cache`.
const SYSTEM: &[&str] = &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/opt", "/etc"];
/// Device files: a plugin's output and entropy, never the real hardware.
/// (`/proc` is left out on purpose: another process's `environ` and
/// `cmdline` hold isocastd's secrets, such as the stream key.)
const DEVICES: &[(&str, u64)] = &[
    (
        "/dev/null",
        FS_READ_FILE | FS_WRITE_FILE | FS_TRUNCATE | FS_IOCTL_DEV,
    ),
    ("/dev/zero", FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV),
    ("/dev/tty", FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV),
    ("/dev/urandom", FS_READ_FILE),
    ("/dev/random", FS_READ_FILE),
];

/// The dynamic loaders (`ld-linux-x86-64.so.2`, `ld-musl-*.so.1`) in the
/// system's library folders.
fn loaders() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for folder in ["/lib64", "/lib", "/usr/lib64", "/usr/lib"] {
        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.starts_with("ld-") || name.starts_with("ld64.") || name == "ld.so.1")
                && name.contains(".so")
            {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// A ruleset as the kernel will have it: what it handles (and so denies
/// unless a rule allows it), and the rules, each masked to that ABI.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    handled_fs: u64,
    handled_net: u64,
    scoped: u64,
    rules: Vec<(PathBuf, u64)>,
}

impl Sandbox {
    fn plan(&self, abi: u32) -> Plan {
        let handled_fs = ALL_FS
            & match abi {
                0 => 0,
                1 => (1 << 13) - 1,
                2 => (1 << 14) - 1,
                3 | 4 => (1 << 15) - 1,
                5..=8 => (1 << 16) - 1,
                _ => (1 << 17) - 1,
            };
        // Serving stays possible: bind isn't handled, so a plugin binds
        // 127.0.0.1 itself (Landlock can't hold a bind to loopback).
        let handled_net = if abi >= 4 && !self.network {
            ALL_NET
        } else {
            0
        };
        let scoped = if abi >= 6 { ALL_SCOPE } else { 0 };
        let mut rules: Vec<(PathBuf, u64)> = Vec::new();
        let mut allow = |path: &Path, access: u64| {
            let access = access & handled_fs;
            if access != 0 {
                rules.push((path.to_path_buf(), access));
            }
        };
        for path in SYSTEM {
            allow(Path::new(path), READ);
        }
        // The dynamic loader is what the kernel runs for every dynamically
        // linked program, so running one needs the right to run it.
        for path in loaders() {
            allow(&path, FS_EXECUTE);
        }
        for (path, access) in DEVICES {
            allow(Path::new(path), *access);
        }
        for path in &self.read_exec {
            allow(path, READ_EXEC);
        }
        for path in &self.read {
            allow(path, READ);
        }
        for path in &self.write {
            allow(path, READ | WRITE_TREE);
        }
        for path in &self.sockets {
            allow(path, FS_RESOLVE_UNIX);
        }
        let access = match self.recordings_access {
            Recordings::None => 0,
            Recordings::Read => READ,
            Recordings::Write => READ | CREATE,
        };
        for path in &self.recordings {
            allow(path, access);
        }
        Plan {
            handled_fs,
            handled_net,
            scoped,
            rules,
        }
    }

    /// The ruleset for this sandbox, as a file descriptor to restrict a
    /// process with (close-on-exec).
    fn ruleset(&self) -> io::Result<OwnedFd> {
        let plan = self.plan(abi());
        let attr = RulesetAttr {
            handled_access_fs: plan.handled_fs,
            handled_access_net: plan.handled_net,
            scoped: plan.scoped,
        };
        // The size the kernel's own struct has at this ABI.
        let size = match abi() {
            0..=3 => 8,
            4..=5 => 16,
            _ => std::mem::size_of::<RulesetAttr>(),
        };
        // SAFETY: `attr` is a live `repr(C)` struct of at least `size`
        // bytes.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new descriptor the syscall returned.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        for (path, access) in &plan.rules {
            let Some(parent) = open_path(path) else {
                continue;
            };
            let mut allowed = *access;
            if !is_dir(&parent) {
                allowed &= FILE_RIGHTS;
            }
            if allowed == 0 {
                continue;
            }
            let rule = PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: parent.as_raw_fd(),
            };
            // SAFETY: a ruleset fd, and a live packed struct the kernel
            // reads while the call runs.
            let code = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneathAttr,
                    0u32,
                )
            };
            if code != 0 {
                return Err(io::Error::other(format!(
                    "Landlock refused the rule for {}: {}",
                    path.display(),
                    io::Error::last_os_error()
                )));
            }
        }
        Ok(ruleset)
    }
}

/// `O_PATH` on a path, None when it can't be reached (not there, or not
/// searchable: the plugin couldn't reach it either).
fn open_path(path: &Path) -> Option<OwnedFd> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    // SAFETY: a new descriptor open returned.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn is_dir(fd: &OwnedFd) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat fills `stat` when it returns 0.
    unsafe {
        libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) == 0
            && stat.assume_init().st_mode & libc::S_IFMT == libc::S_IFDIR
    }
}

/// How to start a plugin's process.
pub struct Spawn<'a> {
    /// The program, as an absolute path.
    pub program: &'a Path,
    /// Its arguments (not the program itself).
    pub args: &'a [String],
    /// Its whole environment.
    pub env: &'a [(String, String)],
    /// The folder it runs in.
    pub cwd: &'a Path,
    /// Where its output goes (appended).
    pub log: &'a Path,
    /// Run it in this sandbox; unsandboxed if absent.
    pub sandbox: Option<&'a Sandbox>,
}

/// Start a plugin's process in its own process group, with only stdin
/// (/dev/null), stdout and stderr (the log) open, and the sandbox applied
/// to it before it runs its program. Returns its process id.
pub fn spawn(spawn: &Spawn) -> io::Result<u32> {
    let log = isocast_diagnostics::output::plugin_pipe(spawn.log)?;
    let ruleset = spawn.sandbox.map(Sandbox::ruleset).transpose()?;
    let mut command = std::process::Command::new(spawn.program);
    command
        .args(spawn.args)
        .env_clear()
        .envs(spawn.env.iter().map(|(k, v)| (k, v)))
        .current_dir(spawn.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    if let Some(ruleset) = &ruleset {
        let fd = ruleset.as_raw_fd();
        // SAFETY: between fork and exec the closure makes only raw
        // syscalls (no allocation, no locks): `fd` is a descriptor made
        // before the fork, and errors are `errno` values.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // The child is waited for by pid, by the registry's supervisor.
    let child = command.spawn()?;
    drop(ruleset);
    Ok(child.id())
}

/// The credentials of the process at the other end of a Unix socket
/// connection, as the kernel recorded them when it connected.
pub fn peer_cred(fd: RawFd) -> Option<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `size` bytes into `cred`.
    let code = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    (code == 0).then_some(cred)
}

/// The process at the other end of a Unix socket connection.
pub fn peer_pid(fd: RawFd) -> Option<u32> {
    peer_cred(fd).and_then(|cred| (cred.pid > 0).then_some(cred.pid as u32))
}

/// The user that owns the process at the other end of a Unix socket
/// connection.
pub fn peer_uid(fd: RawFd) -> Option<u32> {
    peer_cred(fd).map(|cred| cred.uid)
}

/// A process's parent and when it started (clock ticks since boot, which
/// tell a process from a later one given the same id), from
/// `/proc/<pid>/stat`; the second number of the pair is 0.
pub fn process_info(pid: u32) -> Option<(u32, (u64, u64))> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

/// The fields after the process's name, which is in parentheses and may
/// itself hold spaces and parentheses (so the last `)` ends it): field 4 is
/// the parent, field 22 the start time.
fn parse_stat(text: &str) -> Option<(u32, (u64, u64))> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;
    Some((parent, (started, 0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn sandbox() -> Sandbox {
        Sandbox {
            read_exec: vec!["/home/me/plugin".into()],
            read: vec!["/home/me/plugins/deck".into(), "/home/me/notes.txt".into()],
            write: vec!["/home/me/plugins/deck/data".into()],
            sockets: vec!["/home/me/plugins/deck".into()],
            network: false,
            recordings: vec!["/media/rec".into()],
            recordings_access: Recordings::None,
        }
    }

    fn rights(plan: &Plan, path: &str) -> Vec<u64> {
        plan.rules
            .iter()
            .filter(|(p, _)| p == Path::new(path))
            .map(|(_, access)| *access)
            .collect()
    }

    #[test]
    fn the_ruleset_grants_only_what_is_given() {
        let plan = sandbox().plan(10);
        // Nothing for the home folder, /run, /tmp or /proc; recordings
        // aren't granted, so the folder has no rule at all.
        for path in ["/home/me", "/run", "/tmp", "/proc", "/media/rec", "/sys"] {
            assert!(rights(&plan, path).is_empty(), "{path}");
        }
        // The system is readable, not executable (but for its loader).
        assert_eq!(rights(&plan, "/usr"), [READ]);
        assert_eq!(rights(&plan, "/home/me/plugin"), [READ_EXEC]);
        assert_eq!(
            rights(&plan, "/home/me/plugins/deck"),
            [READ, FS_RESOLVE_UNIX]
        );
        let data = rights(&plan, "/home/me/plugins/deck/data")[0];
        assert_eq!(data & FS_EXECUTE, 0);
        assert_ne!(data & FS_MAKE_REG, 0);
        // Connecting to unix sockets, TCP connecting, abstract sockets and
        // signals are all denied by default.
        assert_ne!(plan.handled_fs & FS_RESOLVE_UNIX, 0);
        assert_eq!(plan.handled_net, NET_CONNECT_TCP);
        assert_eq!(plan.scoped, ALL_SCOPE);
        assert_eq!(rights(&plan, "/dev/null").len(), 1);
    }

    #[test]
    fn grants_widen_the_ruleset() {
        let network = Sandbox {
            network: true,
            ..sandbox()
        };
        assert_eq!(network.plan(10).handled_net, 0);
        let read = Sandbox {
            recordings_access: Recordings::Read,
            ..sandbox()
        };
        assert_eq!(rights(&read.plan(10), "/media/rec"), [READ]);
        let write = Sandbox {
            recordings_access: Recordings::Write,
            ..sandbox()
        };
        let access = rights(&write.plan(10), "/media/rec")[0];
        assert_ne!(access & FS_MAKE_REG, 0);
        // Never removing, renaming out, linking or making symlinks.
        for denied in [
            FS_REMOVE_FILE,
            FS_REMOVE_DIR,
            FS_REFER,
            FS_MAKE_SYM,
            FS_EXECUTE,
        ] {
            assert_eq!(access & denied, 0);
        }
    }

    #[test]
    fn older_kernels_get_a_weaker_ruleset_not_an_invalid_one() {
        let old = sandbox().plan(1);
        assert_eq!(old.handled_net, 0);
        assert_eq!(old.scoped, 0);
        assert_eq!(
            old.handled_fs & (FS_REFER | FS_TRUNCATE | FS_RESOLVE_UNIX),
            0
        );
        for (_, access) in &old.rules {
            assert_eq!(access & !old.handled_fs, 0);
        }
        let v5 = sandbox().plan(5);
        assert_ne!(v5.handled_fs & FS_IOCTL_DEV, 0);
        assert_eq!(v5.handled_fs & FS_RESOLVE_UNIX, 0);
        assert_eq!(v5.scoped, 0);
        assert_eq!(sandbox().plan(0).handled_fs, 0);
    }

    #[test]
    fn stat_lines_parse_around_names_with_parentheses() {
        let line = "4242 (a b) c) S 77 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 987654 \
                    1000 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(parse_stat(line), Some((77, (987654, 0))));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn a_process_knows_its_parent() {
        let me = std::process::id();
        let (parent, started) = process_info(me).unwrap();
        // SAFETY: getppid has no preconditions.
        assert_eq!(parent, unsafe { libc::getppid() } as u32);
        assert!(started.0 > 0);
        assert!(process_info(u32::MAX - 1).is_none());
    }

    #[test]
    fn a_socket_peer_is_this_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let cred = peer_cred(a.as_raw_fd()).unwrap();
        assert_eq!(cred.pid as u32, std::process::id());
        // SAFETY: geteuid has no preconditions.
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(peer_pid(a.as_raw_fd()), Some(std::process::id()));
    }

    /// A child under the sandbox reads a file it was granted and is
    /// refused one it wasn't.
    #[test]
    fn a_sandboxed_child_is_denied_what_was_not_granted() {
        if !sandbox_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("isocast-landlock-{}", std::process::id()));
        let allowed = root.join("allowed");
        let denied = root.join("denied");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::write(allowed.join("a.txt"), "yes").unwrap();
        std::fs::write(denied.join("d.txt"), "no").unwrap();
        let sandbox = Sandbox {
            read_exec: vec!["/usr/bin/cat".into()],
            read: vec![allowed.canonicalize().unwrap()],
            ..Default::default()
        };
        let run = |file: &Path, sandbox: Option<&Sandbox>| {
            let args = [file.to_string_lossy().into_owned()];
            let pid = spawn(&Spawn {
                program: Path::new("/usr/bin/cat"),
                args: &args,
                env: &[],
                cwd: &root,
                log: &root.join("log"),
                sandbox,
            })
            .unwrap();
            let mut status = 0;
            // SAFETY: waits for the child just started.
            unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
            libc::WEXITSTATUS(status)
        };
        assert_eq!(run(&allowed.join("a.txt"), Some(&sandbox)), 0);
        assert_ne!(run(&denied.join("d.txt"), Some(&sandbox)), 0);
        // Unsandboxed, the same file reads.
        assert_eq!(run(&denied.join("d.txt"), None), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}

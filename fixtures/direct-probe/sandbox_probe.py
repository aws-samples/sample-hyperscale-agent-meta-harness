# ham engine-isolation probe, report schema hamh-sandbox-probe/1.
# It runs as the engine (inside the Landlock domain, or as the engine user) and tries what the engine must not be able to do:
# read a private file, read the environment or memory of the process that holds the secrets, signal it, inherit a descriptor,
# see a canary value, connect to agentd's own port. It also tries the doors around the connect rule: TCP Fast Open, a multipath TCP socket
# and io_uring. It also runs the controls that must keep working (a temp file in the run's folder, in /tmp and in /var/tmp, a multiprocessing
# lock, loopback, an exec). It prints no secret.
# Run it through the launcher that starts the engine: agentd sandbox-exec --policy-json <json> -- python3 sandbox_probe.py <options>
import argparse
import ctypes
import hashlib
import json
import os
import signal
import socket
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import tempfile

SCHEMA = "hamh-sandbox-probe/1"
DENIED = ("EACCES", "EPERM")
# The kernel does not have the feature: the door is closed by the kernel itself.
ABSENT = ("ENOSYS", "EAFNOSUPPORT", "EPROTONOSUPPORT", "ESOCKTNOSUPPORT", "EOPNOTSUPP", "ENOPROTOOPT")
# The attempt got as far as the port: the connect rule was not applied to it.
REACHED = ("ECONNREFUSED", "ECONNRESET", "EHOSTUNREACH", "ENETUNREACH", "EINPROGRESS")
MSG_FASTOPEN = 0x20000000  # include/uapi/linux/socket.h
IPPROTO_MPTCP = 262  # include/uapi/linux/in.h
IO_URING_SETUP = 425  # the same number on x86_64 and aarch64


def errname(e):
    import errno

    return errno.errorcode.get(e.errno, str(e.errno)) if getattr(e, "errno", None) else type(e).__name__


def check(status, detail):
    return {"status": status, "detail": detail}


def must_be_denied(action):
    """pass when the action is refused with EACCES or EPERM; fail when it works; unknown for any other error."""
    try:
        action()
    except OSError as e:
        name = errname(e)
        if name in DENIED:
            return check("pass", "refused: " + name)
        return check("unknown", "failed with " + name)
    return check("fail", "it worked")


def read_first(path):
    def go():
        with open(path, "rb") as f:
            f.read(1)

    return go


def private_file(path):
    return must_be_denied(read_first(path))


def open_only(path):
    def go():
        with open(path, "rb"):
            pass

    return go


def secret_holder(pid):
    out = {}
    for name in ("environ", "maps"):
        out["proc-" + name] = must_be_denied(read_first("/proc/%d/%s" % (pid, name)))
    # The kernel decides at open time whether this process may read the memory of another; a read at offset 0 would fail with EIO either way.
    out["proc-mem"] = must_be_denied(open_only("/proc/%d/mem" % pid))
    return out


def signal_denied(pid):
    # SIGURG is ignored by default: a signal that gets through changes nothing, and one that is refused says the scope holds.
    return must_be_denied(lambda: os.kill(pid, signal.SIGURG))


def inherited_fds():
    own = os.path.realpath("/proc/self/fd")
    extra = []
    # The listing's own handle is still open while it is read, so its entry points back at the folder; every other descriptor above 2 counts.
    with os.scandir("/proc/self/fd") as entries:
        for entry in entries:
            try:
                if os.readlink(entry.path) == own:
                    continue
            except OSError:
                continue
            if entry.name.isdigit() and int(entry.name) > 2:
                extra.append(int(entry.name))
    return check("pass" if not extra else "fail", "descriptors above 2: %s" % sorted(extra))


def visible_tokens():
    """Every word of this process's environment and of the command line and environment of every process it can read."""
    words = set()
    for value in list(os.environ.values()):
        words.update(value.split())
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        for part in ("cmdline", "environ"):
            try:
                with open("/proc/%s/%s" % (name, part), "rb") as f:
                    data = f.read(1 << 20)
            except OSError:
                continue
            for chunk in data.split(b"\0"):
                text = chunk.decode("utf-8", "replace")
                words.add(text.split("=", 1)[-1])
                words.update(text.split())
    return words


def canary_absent(digest):
    for word in visible_tokens():
        if hashlib.sha256(word.encode("utf-8")).hexdigest() == digest:
            return check("fail", "the canary value is visible in an environment or command line")
    return check("pass", "the canary value is in no environment or command line this process can read")


def launcher_fails_closed(launcher):
    policy = {
        "version": 1, "minAbi": 99, "readWrite": [], "readExec": [], "readOnly": [], "devices": [], "private": [],
        "denyConnect": [], "denyBind": [],
    }
    marker = "SHOULD-NOT-RUN"
    done = subprocess.run(  # nosec B603: argument list, no shell  # nosemgrep: dangerous-subprocess-use-audit -- argument list, no shell
        [launcher, "sandbox-exec", "--policy-json", json.dumps(policy), "--", "/bin/echo", marker],
        capture_output=True, text=True, timeout=30,
    )
    ran = marker in done.stdout
    if ran or done.returncode != 125:
        return check("fail", "exit %d, program ran: %s" % (done.returncode, ran))
    return check("pass", "exit 125 and the program did not run")


def sysv_result(worked, name):
    """pass when the SysV call was refused with EPERM or EACCES; fail when it worked; unknown for any other error."""
    if worked:
        return check("fail", "it worked")
    if name in DENIED:
        return check("pass", "refused: " + name)
    return check("unknown", "failed with " + name)


def sysv_shm(segment_id):
    """Attach a shared-memory segment that another process made. The filter of the launcher answers EPERM to every SysV call."""
    if not segment_id:
        return check("info", "no segment id was given, not looked at")
    lib = ctypes.CDLL(None, use_errno=True)
    lib.shmat.restype = ctypes.c_void_p
    lib.shmat.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    ctypes.set_errno(0)
    address = lib.shmat(int(segment_id), None, 0o10000)
    return sysv_result(address != ctypes.c_void_p(-1).value, errname(OSError(ctypes.get_errno(), "")))


def sysv_msg(queue_id):
    """Send to a message queue that another process made."""
    if not queue_id:
        return check("info", "no queue id was given, not looked at")
    lib = ctypes.CDLL(None, use_errno=True)
    lib.msgsnd.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_int]
    body = ctypes.create_string_buffer(b"\x01" + b"\x00" * 7 + b"x", 9)
    ctypes.set_errno(0)
    sent = lib.msgsnd(int(queue_id), body, 1, 0o4000)
    return sysv_result(sent == 0, errname(OSError(ctypes.get_errno(), "")))


def connect(host, port):
    s = socket.socket()
    s.settimeout(3)
    try:
        s.connect((host, port))
    finally:
        s.close()


def port_check(mode, host, port):
    if mode == "landlock":
        return must_be_denied(lambda: connect(host, port))
    try:
        connect(host, port)
        return check("info", "reachable (the uid mode does not close it)")
    except OSError as e:
        return check("info", "not reachable: " + errname(e))


def closed_door(action, reached):
    """pass when the action is refused or the kernel does not have the feature; fail when it worked or got as far as the port."""
    try:
        action()
    except OSError as e:
        name = errname(e)
        if name in DENIED:
            return check("pass", "refused: " + name)
        if name in ABSENT:
            return check("pass", "the kernel does not have it: " + name)
        if name in REACHED:
            return check("fail", "it " + reached + ": " + name)
        return check("unknown", "failed with " + name)
    return check("fail", "it worked")


def fast_open(host, port):
    """Landlock's connect rule is not asked when sendto opens the connection itself (TCP Fast Open): the filter of the launcher closes it."""

    def go():
        s = socket.socket()
        s.settimeout(3)
        try:
            s.sendto(b"x", MSG_FASTOPEN, (host, port))
        finally:
            s.close()

    return closed_door(go, "reached the port")


def mptcp(host, port):
    """A multipath TCP socket is not a TCP socket to Landlock, and it falls back to plain TCP when the peer does not speak multipath."""

    def go():
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM, IPPROTO_MPTCP)
        try:
            s.settimeout(3)
            s.connect((host, port))
        finally:
            s.close()

    return closed_door(go, "reached the port")


def io_uring():
    """A send queued on io_uring does not pass the filter's argument rules, so the filter closes io_uring itself."""

    def go():
        lib = ctypes.CDLL(None, use_errno=True)
        params = ctypes.create_string_buffer(120)
        ctypes.set_errno(0)
        fd = lib.syscall(ctypes.c_long(IO_URING_SETUP), ctypes.c_uint(4), params)
        if fd >= 0:
            os.close(fd)
            return
        raise OSError(ctypes.get_errno(), "io_uring_setup")

    return closed_door(go, "worked")


def shm_lock():
    """Python's multiprocessing makes its locks and queues with POSIX semaphores, which are files under /dev/shm."""
    try:
        import multiprocessing

        multiprocessing.Lock()
        return check("pass", "a multiprocessing lock was made")
    except (OSError, ImportError) as e:
        name = errname(e) if isinstance(e, OSError) else type(e).__name__
        return check("fail" if name in DENIED else "unknown", "multiprocessing.Lock failed with " + name)


def controls():
    out = {}
    try:
        with tempfile.NamedTemporaryFile(dir=os.environ.get("TMPDIR") or None) as f:
            f.write(b"x")
        out["control-tmp-write"] = check("pass", "a file was written and removed")
    except OSError as e:
        out["control-tmp-write"] = check("fail", errname(e))
    # The conventional scratch folders, named by their parts: the rules tell the engine to use them by name.
    scratch = (("control-tmp-literal", os.path.join(os.sep, "tmp")), ("control-var-tmp", os.path.join(os.sep, "var", "tmp")))
    for name, folder in scratch:
        try:
            with tempfile.NamedTemporaryFile(dir=folder):
                pass
            out[name] = check("pass", "a file was made and removed in " + folder)
        except OSError as e:
            out[name] = check("fail", folder + ": " + errname(e))
    out["control-dev-shm-lock"] = shm_lock()
    try:
        server = socket.socket()
        server.bind(("127.0.0.1", 0))
        server.listen(1)
        connect("127.0.0.1", server.getsockname()[1])
        server.close()
        out["control-loopback"] = check("pass", "bind to port 0 and connect on loopback")
    except OSError as e:
        out["control-loopback"] = check("fail", errname(e))
    try:
        done = subprocess.run(["/bin/true"], timeout=10)  # nosec B603: argument list, no shell
        out["control-exec"] = check("pass" if done.returncode == 0 else "fail", "exit %d" % done.returncode)
    except OSError as e:
        out["control-exec"] = check("fail", errname(e))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=("uid", "landlock"), required=True)
    ap.add_argument("--private-file", required=True)
    ap.add_argument("--secret-pid", type=int, required=True)
    ap.add_argument("--signal-pid", type=int, required=True)
    ap.add_argument("--canary-sha256", required=True)
    ap.add_argument("--launcher", required=True)
    ap.add_argument("--agentd-port", type=int, default=8080)
    ap.add_argument("--sysv-shm-id", default="", help="id of a shared-memory segment another process made")
    ap.add_argument("--sysv-msg-id", default="", help="id of a message queue another process made")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    checks = {"private-file": private_file(a.private_file)}
    checks.update(secret_holder(a.secret_pid))
    checks["signal-to-secret-holder"] = signal_denied(a.signal_pid)
    checks["no-inherited-descriptor"] = inherited_fds()
    checks["canary-absent"] = canary_absent(a.canary_sha256)
    checks["sysv-shm"] = sysv_shm(a.sysv_shm_id)
    checks["sysv-msg"] = sysv_msg(a.sysv_msg_id)
    checks["launcher-fails-closed"] = launcher_fails_closed(a.launcher)
    checks["agentd-port"] = port_check(a.mode, "127.0.0.1", a.agentd_port)
    checks["metadata-address"] = port_check(a.mode, "169.254.169.254", 80)
    checks["fast-open-agentd-port"] = fast_open("127.0.0.1", a.agentd_port)
    checks["fast-open-metadata-address"] = fast_open("169.254.169.254", 80)
    checks["mptcp-agentd-port"] = mptcp("127.0.0.1", a.agentd_port)
    checks["io-uring"] = io_uring()
    checks.update(controls())
    failed = sorted(k for k, v in checks.items() if v["status"] == "fail")
    unknown = sorted(k for k, v in checks.items() if v["status"] == "unknown")
    report = {"schema": SCHEMA, "mode": a.mode, "uid": os.getuid(), "failed": failed, "unknown": unknown, "checks": checks}
    text = json.dumps(report, indent=1, sort_keys=True)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            f.write(text + "\n")
    print(text)
    raise SystemExit(1 if failed or unknown else 0)


if __name__ == "__main__":
    main()

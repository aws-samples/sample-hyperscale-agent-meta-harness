"""Waits of the setup tests: one scale, waits for events, and a program in its own process group that cleanup always signals.

The setup tests start `setup.sh` against a stand-in AWS CLI and wait for it: for a prompt, for the first look at a stand-in build, for the process to end
after Ctrl-C. A fixed limit on such a wait fails on a busy host while the program is fine. So:

  limit(seconds)       the limit of a wait on a free host, times HAMH_TEST_WAIT_SCALE (1 to 8, see scripts/lib/wait_scale.py). Every limit in the
                       setup tests goes through it, once, at the place that names the number; `test_b732_no_fixed_waits.py` holds the files to that.
                       `wait_for`, `GroupProcess.wait_ready` and `GroupProcess.interrupt` scale the seconds they are given themselves: pass them the
                       limit of a free host, not `limit(...)`.
  wait_for(...)        polls an event until it happens; the limit only bounds a hang. It reads the event before the clock, so an event that came
                       while the host stalled in the last sleep is not a failure.
  GroupProcess         a program in a session of its own whose output a thread reads. When the test ends, whatever the outcome, SIGKILL goes to its
                       process group and to the processes that left the group but stayed in its session; a process that called setsid is out of reach.
                       A wait that runs out names its stage (ready, signal, exit, eof) and prints the output, the processes of the group with their
                       state and wait channel, and the load and pressure of the host, so a stalled program can be told from a slow host.
                       The leader is collected only after the group was signalled (its number cannot be reused before), where `os.waitid` exists.
"""

from __future__ import annotations

import math
import os
import signal
import subprocess  # nosec B404: the program under test is started with an argument list, never through a shell
import sys
import threading
import time
from pathlib import Path
from typing import Callable, List, NamedTuple, Optional, Sequence

_LIB = Path(__file__).resolve().parents[1] / "lib"
sys.path.insert(0, str(_LIB))
try:
    import wait_scale  # noqa: E402  (scripts/lib, the one place that holds the rule of the scale)
finally:
    sys.path.pop(0)

SCALE = wait_scale.from_environment()  # a bad value raises here, with the message, when the first test file imports this module


def _default_sigint() -> None:
    """A runner that was itself started in the background (a CI job, nohup) hands its children SIGINT as ignored, and a shell cannot trap a signal it inherited as
    ignored, so a Ctrl-C case would fail there and pass at a terminal. A signal this process handles is reset to the default by `exec`, an ignored one is not:
    so a process that finds SIGINT ignored handles it (the default handler of Python), and every program it starts gets the default. No `preexec_fn` is needed,
    which a process with threads cannot use safely."""
    if threading.current_thread() is threading.main_thread() and signal.getsignal(signal.SIGINT) == signal.SIG_IGN:
        signal.signal(signal.SIGINT, signal.default_int_handler)


_default_sigint()


def limit(seconds: float, scale: Optional[float] = None) -> float:
    """`seconds` (the limit on a free host) times the scale; `scale` is for the tests of this module, the others take the process's own."""
    if not math.isfinite(seconds) or seconds < 0:
        raise ValueError(f"a limit must be a finite number of seconds from 0, got {seconds!r}")
    value = seconds * (SCALE if scale is None else scale)
    if not math.isfinite(value):
        raise ValueError(f"the limit of {seconds!r} s times the scale is not a finite number")
    return value


def wait_for(predicate: Callable[[], bool], seconds: float, what: str, *, scale: Optional[float] = None, interval: float = 0.1,
             clock: Callable[[], float] = time.monotonic, sleep: Callable[[float], None] = time.sleep) -> float:
    """Wait until `predicate()` is true and return the seconds it took. After limit(seconds) it raises AssertionError that names `what`.

    `seconds` is the limit on a free host; the scale is applied here. The predicate is read before the time is, so an event that happened while the host stalled
    in the last sleep still counts. A predicate that blocks is not bounded by this function.
    """
    if not math.isfinite(interval) or interval <= 0:
        raise ValueError(f"the polling interval must be a positive number, got {interval!r}")
    ceiling = limit(seconds, scale)
    shown = SCALE if scale is None else scale
    start = clock()
    while True:
        done = predicate()
        elapsed = clock() - start
        if done:
            return elapsed
        if elapsed >= ceiling:
            raise AssertionError(f"{what} did not happen within {ceiling:g} s ({seconds:g} s on a free host x {shown:g}, set {wait_scale.ENV} for a busy host)")
        sleep(min(interval, ceiling - elapsed))


def _read(path: str) -> str:
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        return ""


class Row(NamedTuple):
    pid: int
    pgrp: int
    session: int
    ppid: int
    state: str
    wchan: str
    command: str


def process_table(group: int) -> List[Row]:
    """The processes of the process group or the session `group`, zombies included; none without /proc."""
    rows: List[Row] = []
    try:
        names = os.listdir("/proc")
    except OSError:
        return rows
    for name in sorted((n for n in names if n.isdigit()), key=int):
        stat = _read(f"/proc/{name}/stat")
        if ")" not in stat:
            continue
        fields = stat.rsplit(")", 1)[1].split()
        if len(fields) < 4:
            continue
        pgrp, session = int(fields[2]), int(fields[3])
        if group not in (pgrp, session):
            continue
        command = _read(f"/proc/{name}/cmdline").replace("\0", " ")[:160]
        rows.append(Row(int(name), pgrp, session, int(fields[1]), fields[0], _read(f"/proc/{name}/wchan") or "-", command))
    return rows[:60]


def process_rows(group: int) -> List[str]:
    """One line for each live process of `group` (pid, parent, group, state, wait channel, command); a zombie is named as one and does not count as running."""
    return [f"  pid={r.pid} ppid={r.ppid} pgrp={r.pgrp} state={r.state} wchan={r.wchan} cmd={r.command}" for r in process_table(group)]


def running(group: int) -> List[Row]:
    return [r for r in process_table(group) if r.state not in ("Z", "X")]


def host_pressure() -> List[str]:
    """The load of the host and, where the kernel offers them, its pressure stall figures and the CPU throttling of the control group."""
    lines: List[str] = []
    load = _read("/proc/loadavg")
    if load:
        lines.append("loadavg: " + load)
    for kind in ("cpu", "io", "memory"):
        text = _read(f"/proc/pressure/{kind}")
        if text:
            lines.append(f"pressure {kind}: " + text.replace("\n", " | "))
    for line in _read("/sys/fs/cgroup/cpu.stat").splitlines():
        if line.startswith(("nr_throttled", "throttled_usec")):
            lines.append("cgroup " + line)
    return lines


class Ended(NamedTuple):
    returncode: int
    output: str
    to_exit: float  # seconds from the signal until the wait saw the program end
    to_eof: float  # seconds from the signal until the wait saw its output end (a process it started can hold the pipe longer than it lives)


class GroupProcess:
    """`argv` in a session of its own, with its output read by a thread. `case.addCleanup` ends the whole process group when the test ends."""

    SLOW_NOTE = 2.0  # a pass that took longer than this after a signal says so on stderr, to collect how long it takes on a busy host
    DRAIN = 2.0  # seconds on a free host that the reader thread gets to take the last output of a program that has ended

    def __init__(self, argv: Sequence[str], *, env: dict, cwd: str, case, name: str = "the program") -> None:
        self.name = name
        self._chunks: List[bytes] = []
        self._lock = threading.Lock()
        self._eof = threading.Event()  # the pipe returned end-of-file: all the output is in
        self._stopped = threading.Event()  # the reader thread ended, by end-of-file or by an error
        self._reader_error: Optional[BaseException] = None
        self._closed = False
        self.proc = subprocess.Popen(list(argv), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, cwd=cwd,  # nosec B603: argument list, no shell, the program under test
                                     start_new_session=True)
        self.group = self.proc.pid
        self._fd = self.proc.stdout.fileno()
        self._reader = threading.Thread(target=self._pump, daemon=True)
        self._reader.start()
        case.addCleanup(self.close)

    def _pump(self) -> None:
        try:
            while True:
                data = os.read(self._fd, 65536)
                if not data:
                    self._eof.set()
                    break
                with self._lock:
                    self._chunks.append(data)
        except OSError as error:
            self._reader_error = error
        finally:
            self._stopped.set()

    def text(self) -> str:
        with self._lock:
            return b"".join(self._chunks).decode("utf-8", "replace")

    def status(self) -> Optional[int]:
        """The exit status of the leader once it has ended (minus the number of the signal that ended it), None while it runs. Where `os.waitid` exists the leader is
        not collected, so its number stays reserved until `close` has signalled the group; elsewhere it is collected here."""
        if self.proc.returncode is not None:
            return self.proc.returncode
        waitid = getattr(os, "waitid", None)
        if waitid is None:
            return self.proc.poll()
        try:
            info = waitid(os.P_PID, self.group, os.WEXITED | os.WNOWAIT | os.WNOHANG)
        except ChildProcessError:
            return self.proc.poll()
        if info is None:
            return None
        return info.si_status if info.si_code == os.CLD_EXITED else -info.si_status

    @property
    def returncode(self) -> Optional[int]:
        return self.status()

    def _ended(self) -> bool:
        return self.status() is not None

    # ---- waiting -----------------------------------------------------------------------------------
    def wait_ready(self, ready: Callable[[], bool], seconds: float, what: str) -> float:
        """Wait until `ready()` is true. A program that ended first, or a wait that runs out, fails the test and ends the group. `seconds` is the limit of a free host."""
        try:
            took = wait_for(lambda: ready() or self._ended(), seconds, what)
        except AssertionError as error:
            raise self._fail(f"stage=ready: {self.name} did not get there: {error}") from None
        if not ready():
            # The program ended, but what it printed last may still be on its way from the pipe to the reader thread: wait for the output to stop (or for a late ready).
            try:
                wait_for(lambda: ready() or self._stopped.is_set(), self.DRAIN, what)
            except AssertionError:
                pass
        if not ready():
            raise self._fail(f"stage=ready: {self.name} ended with exit status {self.status()} before {what} (output {'complete' if self._eof.is_set() else 'not complete'})")
        return took

    def interrupt(self, sig: int = signal.SIGINT, exit_seconds: float = 30.0, eof_seconds: float = 5.0) -> Ended:
        """Send `sig` to the whole process group; wait for the program to end, then for its output to end. Each wait fails the test, and ends the group, when it runs out.
        A program that had already ended fails the test too: the signal would reach nothing, and the end it shows would be its own."""
        label = signal.Signals(sig).name
        if self._ended():
            raise self._fail(f"stage=signal: {self.name} had already ended with exit status {self.status()} before {label} was sent")
        sent = time.monotonic()
        try:
            os.killpg(self.group, sig)
        except ProcessLookupError:
            raise self._fail(f"stage=signal: the process group of {self.name} was gone when {label} was sent") from None
        try:
            wait_for(self._ended, exit_seconds, f"{self.name} to end after {label}")
        except AssertionError as error:
            raise self._fail(f"stage=exit: {error}") from None
        exited = time.monotonic() - sent
        try:
            wait_for(lambda: self._eof.is_set() or self._reader_error is not None, eof_seconds,
                     f"the output of {self.name} to end (it ended about {exited:.1f} s after {label}; a process it started may still hold the pipe)")
        except AssertionError as error:
            raise self._fail(f"stage=eof: {error}") from None
        if self._reader_error is not None:
            raise self._fail(f"stage=eof: the thread that reads the output of {self.name} failed: {self._reader_error!r}")
        done = Ended(self.status(), self.text(), exited, time.monotonic() - sent)
        if max(done.to_exit, done.to_eof) > self.SLOW_NOTE:
            sys.stderr.write(f"note: {self.name} took {done.to_exit:.1f} s to end and {done.to_eof:.1f} s to close its output after {label}\n")
        return done

    # ---- failure and cleanup -----------------------------------------------------------------------
    def diagnostics(self) -> List[str]:
        state = self.status()
        status = "still running" if state is None else f"ended with exit status {state}"
        reader = f", the reader thread failed: {self._reader_error!r}" if self._reader_error is not None else ""
        lines = [f"{self.name}: pid {self.group}, {status}, output complete: {self._eof.is_set()}{reader}, limits scaled by {SCALE:g}",
                 "output so far:", self.text()[-2000:] or "  (none)", "processes of its group or session:"]
        lines += process_rows(self.group) or ["  none, or this host has no /proc"]
        return lines + host_pressure()

    def _fail(self, headline: str) -> AssertionError:
        report = "\n".join([headline, *self.diagnostics()])  # read before the group is ended: afterwards its state is gone
        try:
            self.close()
        except AssertionError as problem:
            report += "\n" + str(problem)
        return AssertionError(report)

    def close(self) -> None:
        """Signal every process of the group, and those that left it but stayed in its session, then collect the leader. Each step is bounded. It fails the test (and says
        which processes) when something is still running afterwards; a process that called setsid is not found."""
        if self._closed:
            return
        self._closed = True
        rows = process_table(self.group)
        if not self._ended() or any(r.state not in ("Z", "X") for r in rows) or not os.path.isdir("/proc"):
            try:
                os.killpg(self.group, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        for row in rows:
            if row.pgrp != self.group and row.session == self.group and row.state not in ("Z", "X"):
                try:
                    os.kill(row.pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
        try:
            self.proc.wait(timeout=limit(10))
        except subprocess.TimeoutExpired:
            pass
        self._stopped.wait(timeout=limit(5))
        survivors = running(self.group)
        if not self._stopped.is_set() or survivors:
            holders = "\n".join(process_rows(self.group)) or "  (none found)"
            raise AssertionError(f"cleanup of {self.name} left work behind: reader thread ended: {self._stopped.is_set()}, processes of its group or session still running: {len(survivors)}\n{holders}")
        self.proc.stdout.close()

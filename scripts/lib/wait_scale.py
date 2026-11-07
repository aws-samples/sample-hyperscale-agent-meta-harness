"""The scale of the wait limits in the Python tests of the setup scripts.

`HAMH_TEST_WAIT_SCALE` multiplies every limit that a setup test puts on a wait: a prompt, a process that must end, a stand-in build to follow. A limit is a
ceiling. The wait ends when its event comes, so a bigger scale costs nothing when the test passes, and a hang fails later, never sooner and never as a pass.

  unset, empty or blank   no choice: 1 when a test file is run directly, and the runner's own default (below) for the files it starts.
  1 to 8           the factor. Anything else (zero, below 1, above 8, not a number, nan, inf) is refused with a message, because a scale above 8 turns a
                   300 s limit into 40 minutes and a hang would outlast the step.
                   value the caller exported always wins, and a bad one stops the run before a file starts.

The tests read it through `scripts/tests/setup_waits.py`; this module holds the rule once for that file and for the runner.
"""

from __future__ import annotations

import math
import os
from typing import Mapping, Optional, Tuple

ENV = "HAMH_TEST_WAIT_SCALE"
CAP = 8.0
PARALLEL_DEFAULT = "4"


def parse(text: Optional[str]) -> float:
    """The scale a value of the variable stands for: 1.0 for no value, the number for 1 to CAP, else ValueError."""
    if text is None or not text.strip():
        return 1.0
    refused = ValueError(f"{ENV} must be a number from 1 to {CAP:g}, got {text!r}")
    try:
        value = float(text)
    except ValueError:
        raise refused from None
    if not math.isfinite(value) or value < 1.0 or value > CAP:
        raise refused
    return value


def from_environment(environ: Optional[Mapping[str, str]] = None) -> float:
    return parse((os.environ if environ is None else environ).get(ENV))


def choose(jobs: int, environ: Optional[Mapping[str, str]] = None) -> Tuple[Optional[str], str]:
    """(the value to export to every test file, where it came from) for a run of `jobs` files at once.

    The caller's value wins and is checked here (ValueError when it is bad); an unset, empty or blank variable is no value. With none, a parallel run exports
    PARALLEL_DEFAULT and the serial run exports nothing, so a single `unittest` process keeps the limits of a free host. The text is exported without surrounding
    blanks, so that it fits the one line the runner prints.
    """
    env = os.environ if environ is None else environ
    text = env.get(ENV)
    if text is not None and text.strip():
        parse(text)
        return text.strip(), "caller"
    if jobs > 1:
        return PARALLEL_DEFAULT, "parallel-default"
    return None, "serial"

"""Oracle tests of load-kata, owned by the E2E harness: they never enter a bundle, a
prompt or the hidden-test set. Each card module present in the checkout (loadkata/c<nnn>.py) is
checked against its CARDS.md parameters from cards.json with its family's checker; a checkout with no
card module fails. Run from the repository root (`python3 -m pytest tests <oracle dir>`).
"""

import importlib
import importlib.util
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import load_kata_checks as checks  # noqa: E402

CARDS = json.loads((HERE / "cards.json").read_text())


def present():
    return [cid for cid in sorted(CARDS) if importlib.util.find_spec("loadkata." + cid) is not None]


def run_family(family):
    failures = []
    for cid in present():
        spec = CARDS[cid]
        if spec["family"] != family:
            continue
        try:
            getattr(checks, "check_" + family)(importlib.import_module("loadkata." + cid), **spec["params"])  # nosemgrep: non-literal-import -- module names come from the oracle's own cards.json, importing the card modules is the check
        except Exception as e:  # every failure is reported, not only the first
            failures.append(f"{cid}: {type(e).__name__}: {e}")
    assert not failures, failures[:20]


def test_some_card_is_present():
    assert present(), "no card module (loadkata/c<nnn>.py) is present"


def test_ring_cards():
    run_family('ring')


def test_radix_cards():
    run_family('radix')


def test_intervals_cards():
    run_family('intervals')


def test_rle_cards():
    run_family('rle')


def test_calendar_cards():
    run_family('calendar')


def test_lru_cards():
    run_family('lru')


def test_numerals_cards():
    run_family('numerals')


def test_csvline_cards():
    run_family('csvline')

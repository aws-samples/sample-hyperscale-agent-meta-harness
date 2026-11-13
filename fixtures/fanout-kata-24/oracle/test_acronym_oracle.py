"""Oracle tests for fanout-kata-24 R22, `kata.acronym.acronym`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.acronym import acronym


def test_words() -> None:
    assert acronym("Portable Network Graphics") == "PNG"


def test_hyphens() -> None:
    assert acronym("complementary metal-oxide semiconductor") == "CMOS"

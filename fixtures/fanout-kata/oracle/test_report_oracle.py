"""Oracle tests for fanout-kata R5; see test_strings_oracle.py. The report depends on the
four helpers, so its task runs after they merge."""
from kata.report import summary


def test_summary_uses_all_four_helpers() -> None:
    assert summary("Hello hello World", 2, 2) == {
        "slug": "hello-hello-world",
        "counts": {"hello": 2, "world": 1},
        "chunks": [["Hello", "hello"], ["World"]],
        "total": 2,
    }

"""Oracle tests for fanout-kata-24 R23, `kata.snake.to_snake`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.snake import to_snake


def test_camel_and_pascal() -> None:
    assert to_snake("helloWorld") == "hello_world"
    assert to_snake("HelloWorld") == "hello_world"


def test_digits_and_snake() -> None:
    assert to_snake("version2Update") == "version2_update"
    assert to_snake("already_snake") == "already_snake"

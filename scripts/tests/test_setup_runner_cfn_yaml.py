"""Tests for scripts/tests/cfn_yaml.py, the small YAML loader that the setup runner tests read the template and the buildspec with.

The loader is test support with no third-party dependency. These cases pin the constructs the two files use, the refusals that keep the
files inside that style, and (where PyYAML is installed) that the loader and PyYAML read the two real files to the same value.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import cfn_yaml  # noqa: E402

TEMPLATE = REPO / "infra" / "setup-runner.yaml"
BUILDSPEC = REPO / "images" / "codebuild" / "setup-runner.yml"


class ScalarCase(unittest.TestCase):
    def test_plain_scalars_resolve_like_yaml(self) -> None:
        doc = cfn_yaml.load("a: true\nb: False\nc: null\nd: ~\ne: 12\nf: -3\ng: text\nh:\ni: 1.5\nj: 0123abc\n")
        self.assertEqual(doc, {"a": True, "b": False, "c": None, "d": None, "e": 12, "f": -3, "g": "text", "h": None, "i": 1.5, "j": "0123abc"})

    def test_quoted_scalars_stay_strings(self) -> None:
        doc = cfn_yaml.load("a: 'true'\nb: \"12\"\nc: 'it''s'\nd: \"say \\\"hi\\\"\"\ne: '2010-09-09'\n")
        self.assertEqual(doc, {"a": "true", "b": "12", "c": "it's", "d": 'say "hi"', "e": "2010-09-09"})

    def test_keys_with_colons_and_slashes(self) -> None:
        doc = cfn_yaml.load("Fn::Sub: x\naws:SourceArn: y\ns3:prefix: z\nurl: https://example.com/a:b\n'quoted key': 1\n")
        self.assertEqual(doc, {"Fn::Sub": "x", "aws:SourceArn": "y", "s3:prefix": "z", "url": "https://example.com/a:b", "quoted key": 1})

    def test_comments_are_dropped_and_hash_in_text_stays(self) -> None:
        doc = cfn_yaml.load("# head\na: 1 # trailing\nb: 'x # not a comment'\nc: a#b\n\n# tail\n")
        self.assertEqual(doc, {"a": 1, "b": "x # not a comment", "c": "a#b"})


class StructureCase(unittest.TestCase):
    def test_nested_mapping_and_both_sequence_layouts(self) -> None:
        text = (
            "Top:\n"
            "  Inner:\n"
            "    - one\n"
            "    - two\n"
            "  Same:\n"
            "  - a\n"
            "  - b\n"
            "  Empty:\n"
            "Next: ok\n"
        )
        self.assertEqual(cfn_yaml.load(text), {"Top": {"Inner": ["one", "two"], "Same": ["a", "b"], "Empty": None}, "Next": "ok"})

    def test_sequence_of_mappings(self) -> None:
        text = (
            "Statements:\n"
            "  - Effect: Allow\n"
            "    Action:\n"
            "      - s3:GetObject\n"
            "      - s3:PutObject\n"
            "    Resource: '*'\n"
            "  - Effect: Deny\n"
            "    Action: iam:*\n"
        )
        self.assertEqual(cfn_yaml.load(text), {"Statements": [
            {"Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject"], "Resource": "*"},
            {"Effect": "Deny", "Action": "iam:*"}]})

    def test_flow_collections(self) -> None:
        doc = cfn_yaml.load("a: [x, 'y z', 3]\nb: {k: v, n: [1, 2]}\nc: []\nd: {}\n")
        self.assertEqual(doc, {"a": ["x", "y z", 3], "b": {"k": "v", "n": [1, 2]}, "c": [], "d": {}})

    def test_block_scalars(self) -> None:
        text = "a: |\n  line one\n  line two\nb: |-\n  keep\nc: >\n  folded\n  text\n\n  next\nd: end\n"
        doc = cfn_yaml.load(text)
        self.assertEqual(doc["a"], "line one\nline two\n")
        self.assertEqual(doc["b"], "keep")
        self.assertEqual(doc["c"], "folded text\nnext\n")
        self.assertEqual(doc["d"], "end")

    def test_block_scalar_in_a_sequence_keeps_its_indent_relative_text(self) -> None:
        text = "commands:\n  - |\n    set -e\n    if true; then\n      echo hi\n    fi\n  - echo done\n"
        self.assertEqual(cfn_yaml.load(text), {"commands": ["set -e\nif true; then\n  echo hi\nfi\n", "echo done"]})


class TagCase(unittest.TestCase):
    def test_short_tags_load_as_the_long_form(self) -> None:
        text = (
            "A: !Ref Bucket\n"
            "B: !Sub 'arn:${AWS::Partition}:s3:::x'\n"
            "C: !GetAtt Role.Arn\n"
            "D: !Join ['', [a, !Ref B]]\n"
            "E: !If [Cond, yes, !Ref AWS::NoValue]\n"
            "F: !Equals [!Ref P, x]\n"
        )
        self.assertEqual(cfn_yaml.load(text), {
            "A": {"Ref": "Bucket"},
            "B": {"Fn::Sub": "arn:${AWS::Partition}:s3:::x"},
            "C": {"Fn::GetAtt": ["Role", "Arn"]},
            "D": {"Fn::Join": ["", ["a", {"Ref": "B"}]]},
            "E": {"Fn::If": ["Cond", "yes", {"Ref": "AWS::NoValue"}]},
            "F": {"Fn::Equals": [{"Ref": "P"}, "x"]},
        })

    def test_tag_over_a_block_scalar(self) -> None:
        text = "Body: !Sub |\n  line ${A}\n  line b\nAfter: 1\n"
        self.assertEqual(cfn_yaml.load(text), {"Body": {"Fn::Sub": "line ${A}\nline b\n"}, "After": 1})

    def test_tag_in_a_sequence_item(self) -> None:
        text = "Items:\n  - !Ref One\n  - !Sub 'x-${AWS::Region}'\n"
        self.assertEqual(cfn_yaml.load(text), {"Items": [{"Ref": "One"}, {"Fn::Sub": "x-${AWS::Region}"}]})


class RefusalCase(unittest.TestCase):
    def test_duplicate_key(self) -> None:
        with self.assertRaises(cfn_yaml.YamlError):
            cfn_yaml.load("a: 1\na: 2\n")

    def test_tab_indentation(self) -> None:
        with self.assertRaises(cfn_yaml.YamlError):
            cfn_yaml.load("a:\n\tb: 1\n")

    def test_anchor_and_alias(self) -> None:
        with self.assertRaises(cfn_yaml.YamlError):
            cfn_yaml.load("a: &x 1\nb: *x\n")

    def test_unterminated_quote(self) -> None:
        with self.assertRaises(cfn_yaml.YamlError):
            cfn_yaml.load("a: [x, 'y]\n")

    def test_second_document(self) -> None:
        with self.assertRaises(cfn_yaml.YamlError):
            cfn_yaml.load("a: 1\n---\nb: 2\n")


def _pyyaml():
    try:
        import yaml  # noqa: PLC0415
    except ImportError:
        return None
    return yaml


class PyYamlAgreementCase(unittest.TestCase):
    """Where PyYAML is installed, it and the loader read the two real files to the same value."""

    def read_with_pyyaml(self, path: Path):
        yaml = _pyyaml()
        if yaml is None:
            self.skipTest("PyYAML is not installed on this machine")

        class Loader(yaml.SafeLoader):
            pass

        def tagged(loader, suffix, node):
            key = "Ref" if suffix == "Ref" else "Condition" if suffix == "Condition" else "Fn::" + suffix
            if isinstance(node, yaml.ScalarNode):
                value = loader.construct_scalar(node)
                if suffix == "GetAtt":
                    value = value.split(".", 1)
            elif isinstance(node, yaml.SequenceNode):
                value = loader.construct_sequence(node, deep=True)
            else:
                value = loader.construct_mapping(node, deep=True)
            return {key: value}

        Loader.add_multi_constructor("!", tagged)
        loader = Loader(path.read_text(encoding="utf-8"))
        try:
            return loader.get_single_data()
        finally:
            loader.dispose()

    def test_template(self) -> None:
        self.assertEqual(cfn_yaml.load(TEMPLATE.read_text(encoding="utf-8")), self.read_with_pyyaml(TEMPLATE))

    def test_buildspec(self) -> None:
        self.assertEqual(cfn_yaml.load(BUILDSPEC.read_text(encoding="utf-8")), self.read_with_pyyaml(BUILDSPEC))


if __name__ == "__main__":
    unittest.main()

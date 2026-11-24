import runpy
import unittest
from pathlib import Path

gate = runpy.run_path(str(Path(__file__).with_name("docs_drift.py")), run_name="docs_drift")
drift = gate["drift"]

CLI = '''
const COMMANDS_HELP: &str = concat!(
    "\\
Commands:
  setup [--model <M>]    Pick a default model
  doctor                 Check
"
);
pub struct Cli {
    /// Shown
    #[arg(long)]
    pub max_turns: Option<u32>,
    #[arg(long, hide = true)]
    pub secret: bool,
    #[arg(skip)]
    pub internal: bool,
    /// Positional
    pub prompt: Vec<String>,
    #[arg(
        long,
        value_name = "X"
    )]
    pub wrapped: bool,
}
'''
COMMANDS = 'CommandInfo {\n        name: "think",\n args: "",\n}'
CONFIG = """pub struct Config {
    /// Doc
    pub model: Option<String>,
    #[serde(skip)]
    pub runtime: bool,
}
"""
TOOLS = ['fn name(&self) -> String {\n        "Read".into()\n    }']
FULL = "--max-turns --wrapped setup doctor /think model Read"


def run(docs, code=""):
    return drift(docs, CLI, COMMANDS, CONFIG, TOOLS, code)


class Drift(unittest.TestCase):
    def test_nothing_when_everything_is_documented(self):
        self.assertEqual(run(FULL), [])

    def test_each_kind_of_missing_name(self):
        for word, item in (
            ("--max-turns", "flag --max-turns"),
            ("--wrapped", "flag --wrapped"),
            ("doctor", "subcommand doctor"),
            ("/think", "slash think"),
            (" model", "key Config.model"),
            ("Read", "tool Read"),
        ):
            self.assertIn(item, run(FULL.replace(word, "")), word)

    def test_a_name_inside_a_longer_one_does_not_count(self):
        self.assertIn("flag --max-turns", run(FULL.replace("--max-turns", "--max-turns-x")))

    def test_hidden_and_internal_fields_are_left_alone(self):
        found = " ".join(run(FULL))
        self.assertNotIn("secret", found)
        self.assertNotIn("runtime", found)

    def test_a_flag_the_code_no_longer_has(self):
        docs = FULL + "\n`chanto run --gone`\n"
        self.assertIn("gone-flag --gone", run(docs))
        self.assertEqual(run(docs, 'args.contains("--gone")'), [])
        self.assertEqual(run(docs, "pub gone: bool,"), [])
        self.assertEqual(run(FULL + "\ncargo build --locked\n"), [])
        self.assertEqual(run(FULL + "\npython3 run.py --agents chanto --other\n"), [])


class Allow(unittest.TestCase):
    def test_entries_need_a_reason(self):
        self.assertEqual(gate["allowed"]("# note\nflag --x  # why\n\n"), {"flag --x"})
        with self.assertRaises(SystemExit):
            gate["allowed"]("flag --x\n")


if __name__ == "__main__":
    unittest.main()

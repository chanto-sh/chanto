import runpy
import unittest
from pathlib import Path

check = runpy.run_path(str(Path(__file__).with_name("inline_tests.py")), run_name="inline_tests")
found = lambda source: list(check["inline_test_modules"](source))


class InlineTests(unittest.TestCase):
    def test_an_inline_test_module_is_refused(self):
        self.assertEqual(found("fn a() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n"), [(4, "tests")])

    def test_a_declaration_is_fine(self):
        self.assertEqual(found("#[cfg(test)]\nmod tests;\n"), [])
        self.assertEqual(found('#[cfg(test)]\n#[path = "x_tests.rs"]\nmod tests;\n'), [])

    def test_other_attributes_names_and_cfgs(self):
        source = (
            "#[cfg(all(test, unix))]\n/// Docs.\n#[allow(dead_code)]\npub(crate) mod linux_tests {\n}\n"
        )
        self.assertEqual(found(source), [(4, "linux_tests")])

    def test_modules_that_are_not_test_only_are_left_alone(self):
        self.assertEqual(found("mod iokit {\n}\n"), [])
        self.assertEqual(found("#[cfg(not(test))]\nmod real {\n}\n"), [])
        self.assertEqual(found("#[cfg(test)]\nfn helper() {}\nmod later {\n}\n"), [])

    def test_where_the_body_goes(self):
        where = check["test_file_for"]
        self.assertEqual(where("src/foo.rs", "tests"), "src/foo/tests.rs")
        self.assertEqual(where("src/x/mod.rs", "tests"), "src/x/tests.rs")
        self.assertEqual(where("src/lib.rs", "version_tests"), "src/version_tests.rs")

    def test_test_files_may_hold_modules(self):
        product = check["is_product"]
        self.assertTrue(product("src/foo.rs"))
        for path in ("src/foo/tests.rs", "src/a_tests.rs", "tests/e2e.rs", "fuzz/fuzz_targets/x.rs"):
            self.assertFalse(product(path), path)


if __name__ == "__main__":
    unittest.main()

import unittest

from tests.release import load

notes = load("notes")

CHANGELOG = (
    "# Changelog\n\n"
    "## [Unreleased]\n\n"
    "- pending\n\n"
    "## [0.2.0] - 2026-10-30\n\n"
    "### Added\n\n"
    "- release workflow\n\n"
    "## [0.2.0-rc.1] - 2026-10-29\n\n"
    "- candidate\n\n"
    "## [0.1.0] - 2026-10-12\n\n"
    "First tagged release.\n"
)


class NotesFor(unittest.TestCase):
    def test_returns_only_the_requested_section(self):
        body = notes.notes_for(CHANGELOG, "0.2.0")
        self.assertIn("release workflow", body)
        self.assertNotIn("First tagged release", body)
        self.assertNotIn("candidate", body)

    def test_release_candidate_has_its_own_section(self):
        self.assertEqual(notes.notes_for(CHANGELOG, "0.2.0-rc.1"), "- candidate")

    def test_last_section_runs_to_the_end(self):
        self.assertEqual(notes.notes_for(CHANGELOG, "0.1.0"), "First tagged release.")

    def test_missing_section_fails(self):
        with self.assertRaisesRegex(SystemExit, r"no ## \[9.9.9\] section"):
            notes.notes_for(CHANGELOG, "9.9.9")

    def test_empty_section_fails(self):
        with self.assertRaisesRegex(SystemExit, "empty CHANGELOG section"):
            notes.notes_for("# Changelog\n\n## [0.1.0]\n\n", "0.1.0")


if __name__ == "__main__":
    unittest.main()

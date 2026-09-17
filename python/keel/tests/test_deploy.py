import os
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._deploy import ephemeral_journal_warning, sqlite_journal_path

FLOWS = {"flows": {"entrypoints": ["py:app:main"]}}


class EphemeralJournalWarningTest(unittest.TestCase):
    def test_serverless_marker_with_flows_and_sqlite_warns(self) -> None:
        with TemporaryDirectory() as d:
            got = ephemeral_journal_warning(FLOWS, {"K_SERVICE": "render"}, Path(d), dockerenv=Path(d, "nope"))
        self.assertIsNotNone(got)
        text, obj = got
        self.assertTrue(text.startswith("keel ▸ warning: durable flows are configured but the journal is SQLite at "))
        self.assertIn("(K_SERVICE)", text)
        self.assertEqual(obj["code"], "journal-ephemeral-storage")
        self.assertEqual(obj["marker"], "K_SERVICE")
        self.assertTrue(obj["journal"].endswith("/.keel/journal.db"))
        # #130: a durable-flows-on-ephemeral-storage misconfiguration is
        # exactly the operator-visible pathology a severity-filtered view
        # exists to surface.
        self.assertEqual(obj["severity"], "WARNING")

    def test_dockerenv_and_read_only_cwd_are_markers(self) -> None:
        with TemporaryDirectory() as d:
            marker = Path(d, ".dockerenv"); marker.write_text("")
            _, obj = ephemeral_journal_warning(FLOWS, {}, Path(d), dockerenv=marker)
            self.assertEqual(obj["marker"], "/.dockerenv")
        if hasattr(os, "geteuid") and os.geteuid() == 0:
            # POSIX access(2)'s write check is bypassed for the superuser on a
            # normal filesystem — chmod(0o500) does not stop root writing, so
            # the probe below would (correctly) report writable and this
            # assertion is meaningless under a root CI container.
            return
        with TemporaryDirectory() as d:
            ro = Path(d, "ro"); ro.mkdir(); ro.chmod(0o500)
            try:
                got = ephemeral_journal_warning(FLOWS, {}, ro, dockerenv=Path(d, "nope"))
                self.assertEqual(got[1]["marker"], "read-only cwd")
            finally:
                ro.chmod(0o700)

    def test_no_flows_or_postgres_or_no_marker_is_silent(self) -> None:
        with TemporaryDirectory() as d:
            self.assertIsNone(ephemeral_journal_warning({}, {"K_SERVICE": "x"}, Path(d), dockerenv=Path(d, "nope")))
            pg = {**FLOWS, "journal": "postgres://u:p@h/db"}
            self.assertIsNone(ephemeral_journal_warning(pg, {"K_SERVICE": "x"}, Path(d), dockerenv=Path(d, "nope")))
            self.assertIsNone(ephemeral_journal_warning(FLOWS, {}, Path(d), dockerenv=Path(d, "nope")))

    def test_cmd_flows_count_as_configured(self) -> None:
        with TemporaryDirectory() as d:
            pol = {"flows": {"entrypoints": ["cmd:etl"], "match": {"cmd:etl": {"argv": ["run_etl.sh"]}}}}
            self.assertIsNotNone(ephemeral_journal_warning(pol, {"K_SERVICE": "x"}, Path(d), dockerenv=Path(d, "nope")))

    def test_file_journal_is_resolved_lexically_not_against_the_filesystem(self) -> None:
        # Issue caught in review: `Path.resolve()` also resolves symlinks,
        # which diverges from Node's purely-lexical `path.resolve()` — this
        # must stay lexical-only on both sides. The `..` segment is the case
        # where lexical vs. filesystem normalization differ.
        with TemporaryDirectory() as d:
            pol = {**FLOWS, "journal": "file:sub/../other/journal.db"}
            _, obj = ephemeral_journal_warning(pol, {"K_SERVICE": "x"}, Path(d), dockerenv=Path(d, "nope"))
            self.assertEqual(obj["journal"], str(Path(d) / "other" / "journal.db"))

            pol_abs = {**FLOWS, "journal": "file:/var/data/../other/journal.db"}
            _, obj_abs = ephemeral_journal_warning(pol_abs, {"K_SERVICE": "x"}, Path(d), dockerenv=Path(d, "nope"))
            self.assertEqual(obj_abs["journal"], "/var/other/journal.db")

    @unittest.skipIf(sys.platform == "win32", "symlink creation needs SeCreateSymbolicLinkPrivilege on Windows")
    def test_file_journal_path_is_lexical_not_symlink_resolved(self) -> None:
        # #99: create the symlink ourselves so this is a real regression guard
        # on every platform, not only where /tmp happens to be one.
        with TemporaryDirectory() as d:
            real = Path(d, "real"); real.mkdir()
            link = Path(d, "link"); link.symlink_to(real, target_is_directory=True)
            policy = {**FLOWS, "journal": "file:.keel/journal.db"}
            got = sqlite_journal_path(policy, link)
            self.assertEqual(got, link / ".keel" / "journal.db")
            self.assertNotEqual(got, real / ".keel" / "journal.db", "lexical, not resolved")

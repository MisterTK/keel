import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._deploy import ephemeral_journal_warning

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

    def test_dockerenv_and_read_only_cwd_are_markers(self) -> None:
        with TemporaryDirectory() as d:
            marker = Path(d, ".dockerenv"); marker.write_text("")
            _, obj = ephemeral_journal_warning(FLOWS, {}, Path(d), dockerenv=marker)
            self.assertEqual(obj["marker"], "/.dockerenv")
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

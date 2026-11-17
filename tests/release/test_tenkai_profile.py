"""tenkai_profile.py against this tree's job document and deploy/tenkai.toml.

crates/kako/tests/release_packaging.rs owns the stale-product refusals and the
parity with the Rust job contract; this keeps the check in the release run.
"""

import unittest

from tests.release import load

profile = load("tenkai_profile")


class TenkaiProfile(unittest.TestCase):
    def setUp(self):
        self.body = profile.PRODUCT.read_text()
        self.contract_id, self.version = profile.job_submission_contract()

    def test_committed_product_ends_with_the_generated_v2_profile(self):
        profile.check_tenkai_toml(self.body)
        generated = profile.generated_profile()
        self.assertIn(f"version = {profile.PROFILE_VERSION}\n", generated)
        self.assertIn(f'"{profile.CONTRACT_ID}" = "{self.version}"', generated)

    def test_an_unrecorded_job_document_field_set_is_refused(self):
        job, scoring = profile.job_document_fields()
        with self.assertRaisesRegex(SystemExit, "record a contract version"):
            profile.contract_version(job + ("extra",), scoring)


if __name__ == "__main__":
    unittest.main()

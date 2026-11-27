"""Release invariants no workflow linter expresses, asserted on the parsed YAML.

The scripts enforce these at run time; these tests keep the workflows from
bypassing them. Assert on structure (triggers, permissions, job graph, step
order, which script a step runs), never on the wording of a step.
"""

import unittest

import yaml

from tests.release import ROOT

WORKFLOWS = ROOT / ".github" / "workflows"
VERIFIED_SHA = "${{ needs.verify-tag.outputs.sha }}"
VERIFIED_TAG = "needs.verify-tag.outputs.tag != ''"
HEAD_CHECK = 'test "$(git rev-parse HEAD)" = "$SHA"'


def load(name: str) -> dict:
    doc = yaml.safe_load((WORKFLOWS / name).read_text())
    doc["on"] = doc.pop(True, doc.get("on"))  # YAML 1.1 reads a bare `on` key as True.
    return doc


def runs(step: dict) -> str:
    return step.get("run", "")


def uses(step: dict) -> str:
    return step.get("uses", "")


def is_mutation(step: dict) -> bool:
    return (
        uses(step).startswith("actions/attest")
        or "scripts/release/push-image.sh" in runs(step)
        or "github-release.sh draft" in runs(step)
        or "github-release.sh publish" in runs(step)
    )


def is_guard(step: dict) -> bool:
    return "github-release.sh guard" in runs(step)


def needs(job: dict) -> set:
    value = job.get("needs", [])
    return {value} if isinstance(value, str) else set(value)


def strings(node):
    if isinstance(node, dict):
        for value in node.values():
            yield from strings(value)
    elif isinstance(node, list):
        for value in node:
            yield from strings(value)
    elif isinstance(node, str):
        yield node


class Dispatcher(unittest.TestCase):
    """A v* tag push only starts release.yml from main; it can do nothing else."""

    def setUp(self):
        self.wf = load("release-dispatch.yml")

    def test_triggers_only_on_v_tags(self):
        self.assertEqual(self.wf["on"], {"push": {"tags": ["v*"]}})

    def test_can_only_dispatch_workflows(self):
        self.assertEqual(self.wf["permissions"], {"actions": "write", "contents": "read"})

    def test_runs_one_step_that_dispatches_release_on_main(self):
        [job] = self.wf["jobs"].values()
        [step] = job["steps"]
        self.assertNotIn("uses", step)
        self.assertEqual(step["run"].strip(), 'gh workflow run release.yml --ref main -f tag="$TAG"')


class Release(unittest.TestCase):
    def setUp(self):
        self.wf = load("release.yml")
        self.jobs = self.wf["jobs"]

    def test_never_runs_on_push(self):
        self.assertEqual(set(self.wf["on"]), {"pull_request", "workflow_dispatch"})
        self.assertIn("tag", self.wf["on"]["workflow_dispatch"]["inputs"])

    def test_one_run_per_tag_and_a_tagged_run_is_never_cancelled(self):
        self.assertIn("inputs.tag", self.wf["concurrency"]["group"])
        self.assertEqual(self.wf["concurrency"]["cancel-in-progress"], "${{ !inputs.tag }}")

    def test_a_tagged_run_must_start_from_main(self):
        first = self.jobs["verify-tag"]["steps"][0]
        self.assertEqual(first.get("if"), "inputs.tag != ''")
        self.assertEqual(first["env"]["WORKFLOW_REF"], "${{ github.ref }}")
        self.assertIn('[[ "$WORKFLOW_REF" == refs/heads/main ]]', runs(first))

    def test_verifies_the_tag_with_mains_verifier(self):
        steps = self.jobs["verify-tag"]["steps"]
        main_checkout = next(s for s in steps if s.get("with", {}).get("path") == "main-tree")
        self.assertNotIn("ref", main_checkout["with"], "main's verifier must come from the workflow ref")
        verify = next(s for s in steps if "verify-tag.sh" in runs(s))
        self.assertIn('bash "${MAIN_TREE}/scripts/release/verify-tag.sh" verify "$TAG" "$sha"', verify["run"])
        self.assertTrue(verify["env"]["MAIN_TREE"].endswith("/main-tree"))
        guard = next(s for s in steps if is_guard(s))
        self.assertIn("main-tree/scripts/release/github-release.sh guard", guard["run"])

    def test_every_later_job_builds_the_verified_commit(self):
        for name, job in self.jobs.items():
            if name == "verify-tag":
                continue
            with self.subTest(job=name):
                self.assertIn("verify-tag", needs(job))
                steps = job["steps"]
                checkouts = [i for i, s in enumerate(steps) if uses(s).startswith("actions/checkout@")]
                self.assertTrue(checkouts, f"{name} must check out the release commit")
                for i in checkouts:
                    self.assertEqual(steps[i]["with"].get("ref"), VERIFIED_SHA)
                    self.assertIn(HEAD_CHECK, runs(steps[i + 1]))

    def test_never_uses_the_event_commit_or_ref_name(self):
        for value in strings(self.jobs):
            for banned in ("github.sha", "github.ref_name", "github.event_name", "GITHUB_SHA", "GITHUB_REF"):
                self.assertNotIn(banned, value)

    def test_release_and_image_changes_go_through_the_release_scripts(self):
        for value in strings(self.jobs):
            self.assertNotIn("gh release ", value)
            self.assertNotIn("docker push", value)

    def test_every_mutating_job_refuses_a_published_release_first(self):
        mutating = set()
        for name, job in self.jobs.items():
            steps = job["steps"]
            first = next((i for i, s in enumerate(steps) if is_mutation(s)), None)
            if first is None:
                continue
            mutating.add(name)
            with self.subTest(job=name):
                guards = [s for s in steps[:first] if is_guard(s)]
                self.assertTrue(guards, f"{name} mutates before the guard")
                self.assertTrue(all("if" not in s for s in guards), f"{name} guard must always run")
                condition = job.get("if", "")
                self.assertTrue(condition.startswith(VERIFIED_TAG), f"{name} must run only for a verified tag")
                self.assertNotIn("||", condition)
        self.assertEqual(mutating, {"push-image", "attest", "publish"})

    def test_sql_image_push_covers_amd64_and_arm64(self):
        arm = self.jobs["sql-image-arm64"]
        self.assertEqual(arm["runs-on"], "ubuntu-24.04-arm")
        self.assertLessEqual({"verify-tag", "tests"}, needs(arm))
        writers = {k for k, v in arm.get("permissions", {}).items() if v == "write"}
        self.assertEqual(writers, set())
        package = next(s for s in arm["steps"] if "scripts/release/package.sh" in runs(s))
        self.assertIn("--with-sql-image", runs(package))
        self.assertNotIn("--with-images", runs(package))
        push = self.jobs["push-image"]
        self.assertLessEqual({"package", "sql-image-arm64"}, needs(push))
        self.assertIn("needs.sql-image-arm64.result == 'success'", push["if"])
        run = next(runs(s) for s in push["steps"] if "scripts/release/push-image.sh" in runs(s))
        self.assertIn("dist/kako-sql-image.tar", run)
        self.assertIn("dist-arm64/kako-sql-image.tar", run)

    def test_publish_runs_last_after_every_asset_job(self):
        publish = self.jobs["publish"]
        self.assertLessEqual({"package", "attest", "push-image"}, needs(publish))
        self.assertFalse(any("publish" in needs(job) for job in self.jobs.values()))
        self.assertNotIn("always()", publish["if"])

    def test_the_manifest_records_the_verified_commit_and_is_attested_before_drafting(self):
        publish = self.jobs["publish"]
        self.assertEqual(publish["env"]["SHA"], VERIFIED_SHA)
        steps = publish["steps"]
        write = next(i for i, s in enumerate(steps) if "scripts/release/manifest.py" in runs(s))
        self.assertIn('--git-sha "$SHA"', runs(steps[write]))
        self.assertIn("--labels-from-history", runs(steps[write]))
        sign = next(
            i for i, s in enumerate(steps)
            if uses(s).startswith("actions/attest") and s["with"]["subject-path"] == "dist/kako-release.json"
        )
        draft = next(i for i, s in enumerate(steps) if "github-release.sh draft" in runs(s))
        self.assertLess(write, sign)
        self.assertLess(sign, draft)

    def test_prerelease_state_comes_from_the_verified_tag(self):
        self.assertEqual(self.jobs["publish"]["env"]["PRERELEASE"], "${{ needs.verify-tag.outputs.prerelease }}")

    def test_attests_provenance_and_sbom(self):
        actions = {uses(s).split("@")[0] for s in self.jobs["attest"]["steps"]}
        self.assertLessEqual({"actions/attest-build-provenance", "actions/attest-sbom"}, actions)

    def test_write_permissions_are_job_scoped(self):
        self.assertEqual(self.wf["permissions"], {"contents": "read"})
        writers = {
            name: {k for k, v in job.get("permissions", {}).items() if v == "write"}
            for name, job in self.jobs.items()
        }
        self.assertEqual(
            {name: perms for name, perms in writers.items() if perms},
            {
                "push-image": {"packages"},
                "attest": {"id-token", "attestations"},
                "publish": {"contents", "id-token", "attestations"},
            },
        )


class EngineBar(unittest.TestCase):
    """Scale-factor and sqllogictest jobs stay on the named harness scripts."""

    def setUp(self):
        self.wf = load("engine-bar.yml")
        self.jobs = self.wf["jobs"]

    def test_pr_runs_when_the_slt_harness_changes(self):
        paths = self.wf["on"]["pull_request"]["paths"]
        self.assertIn("scripts/engine-bar/slt/**", paths)
        self.assertIn("scripts/engine-bar/sf/**", paths)
        self.assertIn(".github/workflows/engine-bar.yml", paths)

    def test_slt_job_runs_both_engines_through_the_harness(self):
        job = self.jobs["slt"]
        run = next(s for s in job["steps"] if "scripts/engine-bar/slt/run.sh" in runs(s))
        self.assertIn("scripts/ci-cgroup-delegate.sh", runs(run))
        self.assertIn("both", runs(run))
        self.assertIn("scripts/engine-bar/slt/run.sh", runs(run))

    def test_scale_jobs_compare_the_new_run_to_the_committed_snapshot(self):
        report = next(
            step
            for step in self.jobs["engine-bar"]["steps"]
            if "scripts/engine-bar/sf/report.py" in runs(step)
        )
        self.assertEqual(report.get("if"), "always()")
        self.assertIn("--previous", runs(report))
        self.assertIn("docs/engine-bar/sf${{ matrix.scale }}-runs.jsonl", runs(report))
        self.assertIn("docs/engine-bar/sf${{ matrix.scale }}-host.json", runs(report))

    def test_does_not_write_the_repository(self):
        self.assertEqual(self.wf["permissions"], {"contents": "read"})


if __name__ == "__main__":
    unittest.main()


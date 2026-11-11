#!/usr/bin/env python3
"""Generate and check the Tenkai compatibility profile kako publishes.

    scripts/tenkai-profile.py --check
    scripts/tenkai-profile.py --check --file PATH
    scripts/tenkai-profile.py --print-profile
    scripts/tenkai-profile.py --print-contract
    scripts/tenkai-profile.py --print-fields
    scripts/tenkai-profile.py --self-test

The contract id is the job-submission surface dependents require. The
semantic version is derived from the JSON job document in
`crates/kako/src/local.rs` (`JobFile` and `ScoringFile`). `--check` refuses
a product file that does not end with the generated profile v2 block.
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import sys

CONTRACT_ID = "kako.jobs"
PROFILE_VERSION = 2
MAX_EVIDENCE_AGE_MS = 86_400_000
COMPONENT = "kako"
ROOT = pathlib.Path(__file__).resolve().parent.parent
LOCAL_RS = ROOT / "crates" / "kako" / "src" / "local.rs"
PRODUCT = ROOT / "deploy" / "tenkai.toml"
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")

# Semantic versions of the job-document field set. Adding an optional field
# is a minor; removing, renaming, or changing meaning is a major.
CONTRACT_VERSIONS = {
    (
        (
            "id",
            "engine",
            "sql",
            "code",
            "inputs",
            "input_markings",
            "input_column_markings",
            "outputs",
            "output_markings",
            "declassify",
            "timeout_ms",
            "cpu_time_secs",
            "memory_bytes",
            "tenant_id",
            "namespace",
            "runtime_image",
            "isolation",
            "write_mode",
            "scoring",
        ),
        ("descriptor", "trusted_keys", "artifact"),
    ): "1.0.0",
}


def fail(message: str) -> None:
    raise SystemExit(f"tenkai-profile: {message}")


def rust_struct_fields(source: str, name: str) -> tuple[str, ...]:
    marker = f"struct {name} {{"
    start = source.find(marker)
    if start < 0:
        fail(f"missing struct {name}")
    i = start + len(marker)
    depth = 1
    while i < len(source) and depth:
        char = source[i]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        i += 1
    body = source[start + len(marker) : i - 1]
    fields: list[str] = []
    for raw in body.splitlines():
        line = raw.strip()
        if not line or line.startswith("//") or line.startswith("#["):
            continue
        ident, sep, _rest = line.partition(":")
        if sep != ":":
            continue
        ident = ident.strip()
        if ident.isidentifier():
            fields.append(ident)
    if not fields:
        fail(f"struct {name} has no fields")
    return tuple(fields)


def job_document_fields(source: str | None = None) -> tuple[tuple[str, ...], tuple[str, ...]]:
    text = LOCAL_RS.read_text() if source is None else source
    return rust_struct_fields(text, "JobFile"), rust_struct_fields(text, "ScoringFile")


def contract_version(
    job_fields: tuple[str, ...], scoring_fields: tuple[str, ...]
) -> str:
    try:
        version = CONTRACT_VERSIONS[(job_fields, scoring_fields)]
    except KeyError:
        fail(
            "unknown job-submission surface "
            f"job={job_fields!r} scoring={scoring_fields!r}; record a contract version"
        )
    if not SEMVER.fullmatch(version):
        fail(f"job-submission contract version must be stable MAJOR.MINOR.PATCH: {version}")
    return version


def product_version(body: str) -> str:
    match = re.search(
        r"(?ms)^\[product\].*?^version = \"([^\"]+)\"",
        body,
    )
    if not match:
        fail("product file is missing [product].version")
    return match.group(1)


def pin_digest(version: str) -> str:
    return "sha256:" + hashlib.sha256(f"kako@{version}".encode()).hexdigest()


def compatibility_profile(product_ver: str, contract_ver: str, contract_id: str = CONTRACT_ID) -> str:
    pin = pin_digest(product_ver)
    return (
        "[compatibility]\n"
        f"version = {PROFILE_VERSION}\n"
        "# Evidence binds one release digest; one day is Tenkai's maximum.\n"
        f"max_evidence_age_ms = {MAX_EVIDENCE_AGE_MS}\n"
        "\n"
        f"[compatibility.components.{COMPONENT}.pin]\n"
        'kind = "digest"\n'
        f'value = "{pin}"\n'
        "\n"
        f"[compatibility.components.{COMPONENT}.provides]\n"
        f'"{contract_id}" = "{contract_ver}"\n'
    )


def job_submission_contract(source: str | None = None) -> tuple[str, str]:
    job_fields, scoring_fields = job_document_fields(source)
    return CONTRACT_ID, contract_version(job_fields, scoring_fields)


def check_tenkai_toml(body: str, source: str | None = None) -> None:
    contract_id, version = job_submission_contract(source)
    profile = compatibility_profile(product_version(body), version, contract_id)
    if not body.endswith(profile):
        fail("deploy/tenkai.toml must end with this compatibility profile:\n\n" + profile)


def generated_profile() -> str:
    body = PRODUCT.read_text()
    contract_id, version = job_submission_contract()
    return compatibility_profile(product_version(body), version, contract_id)


def self_test() -> None:
    source = LOCAL_RS.read_text()
    job_fields, scoring_fields = job_document_fields(source)
    version = contract_version(job_fields, scoring_fields)
    if not SEMVER.fullmatch(version):
        fail(f"self-test: unstable contract version {version}")
    body = PRODUCT.read_text()
    check_tenkai_toml(body, source)
    profile = compatibility_profile(product_version(body), version)
    if f'version = {PROFILE_VERSION}\n' not in profile:
        fail("self-test: generated profile is not v2")
    if f'"{CONTRACT_ID}" = "{version}"' not in profile:
        fail("self-test: generated profile missing the job-submission contract")

    try:
        contract_version(job_fields + ("extra",), scoring_fields)
    except SystemExit:
        pass
    else:
        fail("self-test: extra job document field must be unknown")

    stand_in = body
    mutated = [
        stand_in.replace(f'"{CONTRACT_ID}" = "{version}"', f'"{CONTRACT_ID}" = "0.0.1"'),
        stand_in.replace(f'"{CONTRACT_ID}"', '"kako.other"'),
        stand_in.replace("version = 2", "version = 1"),
    ]
    for stale in mutated:
        if stale == stand_in:
            fail("self-test: mutation did not change the product")
        try:
            check_tenkai_toml(stale, source)
        except SystemExit:
            continue
        fail("self-test: stale product must be refused")


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--print-profile", action="store_true")
    mode.add_argument("--print-contract", action="store_true")
    mode.add_argument("--print-fields", action="store_true")
    mode.add_argument("--self-test", action="store_true")
    parser.add_argument("--file", type=pathlib.Path)
    args = parser.parse_args(argv)
    if args.self_test:
        if args.file:
            parser.error("--self-test does not take --file")
        self_test()
        contract_id, version = job_submission_contract()
        print(f"tenkai-profile: {contract_id} {version} ok")
        return
    if args.print_fields:
        job_fields, scoring_fields = job_document_fields()
        print("\n".join(job_fields))
        print("---")
        print("\n".join(scoring_fields))
        return
    if args.print_contract:
        contract_id, version = job_submission_contract()
        print(f"{contract_id} {version}")
        return
    if args.print_profile:
        sys.stdout.write(generated_profile())
        return
    path = args.file if args.file is not None else PRODUCT
    check_tenkai_toml(path.read_text())
    contract_id, version = job_submission_contract()
    print(f"tenkai-profile: {contract_id} {version} ok")


if __name__ == "__main__":
    main(sys.argv[1:])

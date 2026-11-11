//! Job-submission contract the Tenkai product provides.
//!
//! Dependents require [`CONTRACT_ID`]. The semantic version is derived from
//! the JSON job document (`local::JobFile`) this tree ships. The release path
//! generates the Tenkai profile v2 block from that pair and refuses a
//! `deploy/tenkai.toml` that does not end with it.

use kako_core::digest;

/// Contract id dependents declare on `requires`.
pub const CONTRACT_ID: &str = "kako.jobs";

/// JSON keys of the job document dependents hand to kako (`local::JobFile`).
/// Adding an optional field is a minor; removing, renaming, or changing
/// meaning is a major.
pub const JOB_DOCUMENT_FIELDS: &[&str] = &[
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
];

/// JSON keys of the job document's `scoring` object (`local::ScoringFile`).
pub const SCORING_DOCUMENT_FIELDS: &[&str] = &["descriptor", "trusted_keys", "artifact"];

/// Id and stable semantic version of the job-submission contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobSubmissionContract {
    pub id: &'static str,
    pub version: &'static str,
}

/// Contract this tree provides, derived from the shipped job document.
pub fn job_submission_contract() -> Result<JobSubmissionContract, String> {
    let version = contract_version(JOB_DOCUMENT_FIELDS, SCORING_DOCUMENT_FIELDS)?;
    if !is_stable_semver(version) {
        return Err(format!(
            "job-submission contract version must be stable MAJOR.MINOR.PATCH: {version}"
        ));
    }
    Ok(JobSubmissionContract {
        id: CONTRACT_ID,
        version,
    })
}

/// Tenkai profile v2 block a kako product must end with.
pub fn compatibility_profile(product_version: &str) -> Result<String, String> {
    let contract = job_submission_contract()?;
    let pin = digest::hash_bytes(format!("kako@{product_version}").as_bytes());
    Ok(format!(
        "[compatibility]\n\
         version = 2\n\
         # Evidence binds one release digest; one day is Tenkai's maximum.\n\
         max_evidence_age_ms = 86400000\n\
         \n\
         [compatibility.components.kako.pin]\n\
         kind = \"digest\"\n\
         value = \"{pin}\"\n\
         \n\
         [compatibility.components.kako.provides]\n\
         \"{id}\" = \"{version}\"\n",
        pin = pin.0,
        id = contract.id,
        version = contract.version,
    ))
}

/// Fail unless `body` ends with [`compatibility_profile`].
pub fn check_tenkai_toml(body: &str, product_version: &str) -> Result<(), String> {
    let profile = compatibility_profile(product_version)?;
    if body.ends_with(&profile) {
        return Ok(());
    }
    Err(format!(
        "deploy/tenkai.toml must end with this compatibility profile:\n\n{profile}"
    ))
}

fn contract_version(job_fields: &[&str], scoring_fields: &[&str]) -> Result<&'static str, String> {
    match (job_fields, scoring_fields) {
        (
            [
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
            ],
            ["descriptor", "trusted_keys", "artifact"],
        ) => Ok("1.0.0"),
        _ => Err(format!(
            "unknown job-submission surface job={job_fields:?} scoring={scoring_fields:?}; record a contract version"
        )),
    }
}

fn is_stable_semver(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    [major, minor, patch].into_iter().all(|part| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_job_document_is_kako_jobs_1_0_0() {
        let contract = job_submission_contract().unwrap();
        assert_eq!(contract.id, "kako.jobs");
        assert_eq!(contract.version, "1.0.0");
        assert!(is_stable_semver(contract.version));
    }

    #[test]
    fn unknown_job_document_has_no_contract_version() {
        let error = contract_version(&["id"], SCORING_DOCUMENT_FIELDS).unwrap_err();
        assert!(error.contains("unknown job-submission surface"), "{error}");
    }

    #[test]
    fn prerelease_contract_version_is_refused() {
        assert!(!is_stable_semver("1.0.0-rc.1"));
        assert!(!is_stable_semver("1.0.0+build"));
        assert!(!is_stable_semver("01.0.0"));
        assert!(is_stable_semver("1.0.0"));
        assert!(is_stable_semver("0.1.0"));
    }
}

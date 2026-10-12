//! Isolation-tier floor on a job, and the backend kako actually runs.

use std::fmt;
use std::str::FromStr;

use crate::Failure;

/// Isolation kako understands. The job source sets a minimum; the receipt
/// records the tier that ran ([ADR 0009](../../../docs/decisions/0009-isolation-tier.md)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum IsolationTier {
    /// kekkai `linux_native`: Landlock, seccomp, cgroup in a child that
    /// shares the host kernel and the runner's OS user.
    #[default]
    LinuxNative,
    /// kekkai `Backend::Userspace`. Spawn is Issue #3; the pin is Issue #33.
    Userspace,
    /// KVM/microVM, only after kekkai ships that backend.
    Kvm,
}

impl IsolationTier {
    /// JSON / receipt spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinuxNative => "linux_native",
            Self::Userspace => "userspace",
            Self::Kvm => "kvm",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::LinuxNative => 0,
            Self::Userspace => 1,
            Self::Kvm => 2,
        }
    }

    fn meets(self, required: Self) -> bool {
        self.rank() >= required.rank()
    }
}

impl fmt::Display for IsolationTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IsolationTier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "linux_native" => Ok(Self::LinuxNative),
            "userspace" => Ok(Self::Userspace),
            "kvm" => Ok(Self::Kvm),
            other => Err(format!(
                "unknown isolation `{other}`: expected linux_native, userspace, or kvm"
            )),
        }
    }
}

/// Pick the weakest available tier that still meets `required`.
///
/// An empty overlap is a runner failure: never return a weaker tier.
pub fn select_tier(
    required: IsolationTier,
    available: &[IsolationTier],
) -> Result<IsolationTier, Failure> {
    available
        .iter()
        .copied()
        .filter(|tier| tier.meets(required))
        .min_by_key(IsolationTier::rank)
        .ok_or_else(|| Failure::Runner(format!("isolation {required} cannot be applied")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_floor_is_linux_native() {
        assert_eq!(IsolationTier::default(), IsolationTier::LinuxNative);
        assert_eq!(
            "linux_native".parse::<IsolationTier>().unwrap(),
            IsolationTier::LinuxNative
        );
    }

    #[test]
    fn parse_rejects_unknown_names() {
        let err = "rlimit".parse::<IsolationTier>().unwrap_err();
        assert!(err.contains("rlimit"), "{err}");
        assert!(err.contains("linux_native"), "{err}");
    }

    #[test]
    fn weakest_meeting_linux_native_stays_linux_native() {
        let got = select_tier(
            IsolationTier::LinuxNative,
            &[IsolationTier::LinuxNative, IsolationTier::Userspace],
        )
        .unwrap();
        assert_eq!(got, IsolationTier::LinuxNative);
    }

    #[test]
    fn userspace_floor_refuses_a_linux_native_only_host() {
        let err = select_tier(IsolationTier::Userspace, &[IsolationTier::LinuxNative]).unwrap_err();
        match err {
            Failure::Runner(message) => assert!(message.contains("userspace"), "{message}"),
            other => panic!("expected runner failure, got {other}"),
        }
    }

    #[test]
    fn userspace_floor_picks_userspace_when_kvm_is_also_present() {
        let got = select_tier(
            IsolationTier::Userspace,
            &[IsolationTier::Userspace, IsolationTier::Kvm],
        )
        .unwrap();
        assert_eq!(got, IsolationTier::Userspace);
    }

    #[test]
    fn kvm_floor_refuses_userspace() {
        let err = select_tier(
            IsolationTier::Kvm,
            &[IsolationTier::LinuxNative, IsolationTier::Userspace],
        )
        .unwrap_err();
        match err {
            Failure::Runner(message) => assert!(message.contains("kvm"), "{message}"),
            other => panic!("expected runner failure, got {other}"),
        }
    }

    #[test]
    fn empty_available_is_a_runner_failure() {
        let err = select_tier(IsolationTier::LinuxNative, &[]).unwrap_err();
        match err {
            Failure::Runner(message) => assert!(message.contains("linux_native"), "{message}"),
            other => panic!("expected runner failure, got {other}"),
        }
    }

    #[test]
    fn adr_states_the_job_receipt_and_fail_closed_contract() {
        let text = include_str!("../../../docs/decisions/0009-isolation-tier.md");
        assert!(text.contains("`isolation`"), "{text}");
        assert!(text.contains("linux_native"), "{text}");
        assert!(text.contains("userspace"), "{text}");
        assert!(text.contains("`kvm`") || text.contains("kvm"), "{text}");
        assert!(
            text.contains("Fail closed") || text.contains("fail closed"),
            "{text}"
        );
        assert!(text.contains("Always guest"), "{text}");
        assert!(text.contains("Always in-process"), "{text}");
        assert!(text.contains("kekkai `Settings`"), "{text}");
        let index = include_str!("../../../docs/decisions/README.md");
        assert!(
            index.contains("0009-isolation-tier.md"),
            "ADR 0009 missing from the index:\n{index}"
        );
    }
}

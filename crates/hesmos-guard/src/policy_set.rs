//! PolicySet loading — discipline thresholds as data, never hard-coded at gate sites
//! (PT-4, SS-11 rule 4).
//!
//! Charter §6 fixes the defaults (20 / 8 / 2 / 8K / 80 / 100 — already owned by
//! [`hesmos_core::PolicySet::default`], pinned by a core test). Loading adds exactly one
//! capability: a plan/policy YAML may OVERRIDE knobs via [`hesmos_core::PolicyOverrides`]
//! — the only sanctioned mutation path, because it cannot unset a cap, only set one. A
//! file therefore can never invent a "second set of defaults"; it starts from the charter
//! numbers and changes what it names.

use std::path::Path;

use hesmos_core::{PolicyOverrides, PolicySet};

/// Policy load/validate errors. No `PartialEq`: the source-bearing variants carry
/// `io::Error`/`serde_yaml_ng::Error`, which have no value equality — match on the
/// variant instead. `InconsistentThresholds` guards the one ordering invariant the
/// numbers must satisfy: the warn line may not sit above the suspend line (a "warning"
/// after suspension would be an after-the-fact warn — SS-15 rule 3 forbids exactly that
/// shape).
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("cannot read policy file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("policy file `{path}` is not valid policy YAML: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error(
        "policy is inconsistent: budget_warn_pct ({warn}) must be <= budget_suspend_pct ({suspend})"
    )]
    InconsistentThresholds { warn: u8, suspend: u8 },
}

/// Parses a YAML file into overrides and applies them over the charter defaults.
pub fn load(path: &Path) -> Result<PolicySet, PolicyError> {
    let text = std::fs::read_to_string(path).map_err(|source| PolicyError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let overrides: PolicyOverrides =
        serde_yaml_ng::from_str(&text).map_err(|source| PolicyError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    effective(&overrides)
}

/// The parse half of [`load`], separable so callers holding a string (plan-embedded
/// policy blocks, tests) skip the filesystem entirely.
pub fn parse_str(yaml: &str) -> Result<PolicySet, PolicyError> {
    let overrides: PolicyOverrides =
        serde_yaml_ng::from_str(yaml).map_err(|source| PolicyError::Parse {
            // A caller-supplied string has no meaningful path; the error text still
            // carries the YAML failure.
            path: "<inline>".into(),
            source,
        })?;
    effective(&overrides)
}

/// Charter defaults + overrides — the single composition point.
pub fn effective(overrides: &PolicyOverrides) -> Result<PolicySet, PolicyError> {
    let set = PolicySet::default().apply(overrides);
    check_consistency(&set)?;
    Ok(set)
}

/// The invariants a composed PolicySet must satisfy. Kept public: any OTHER composition
/// path (a future API surface) must route through the same check.
pub fn check_consistency(set: &PolicySet) -> Result<(), PolicyError> {
    if set.budget_warn_pct > set.budget_suspend_pct {
        return Err(PolicyError::InconsistentThresholds {
            warn: set.budget_warn_pct,
            suspend: set.budget_suspend_pct,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No policy file at all → the charter numbers, byte for byte (§3 수치 상한:
    /// defaults outside 20/8/2/8K/80/100 are forbidden).
    #[test]
    fn no_overrides_yields_charter_defaults() {
        let set = effective(&PolicyOverrides::default()).expect("valid");
        assert_eq!(set, PolicySet::default());
    }

    /// A partial file overrides only what it names; everything else stays charter.
    #[test]
    fn partial_file_overrides_only_named_knobs() {
        let set = parse_str("max_handoffs: 5\nping_pong_window: 4\n").expect("valid");
        assert_eq!(set.max_handoffs, 5);
        assert_eq!(set.ping_pong_window, 4);
        assert_eq!(
            (
                set.bounded_retry,
                set.max_transfer_tokens,
                set.budget_warn_pct,
                set.budget_suspend_pct
            ),
            (2, 8 * 1024, 80, 100),
            "untouched knobs keep charter values"
        );
    }

    /// Unknown keys are a schema violation, not silently dropped data — the same shape
    /// rule as every other wire schema in the stack.
    #[test]
    fn unknown_key_is_rejected() {
        let err = parse_str("max_handoffs: 5\nevil_bypass: true\n").expect_err("must reject");
        assert!(
            err.to_string().contains("unknown field"),
            "deny_unknown_fields surfaces: {err}"
        );
    }

    /// warn above suspend would warn after suspension — structurally impossible.
    #[test]
    fn warn_above_suspend_is_rejected() {
        let err = parse_str("budget_warn_pct: 90\nbudget_suspend_pct: 80\n").expect_err("invalid");
        assert!(
            matches!(
                err,
                PolicyError::InconsistentThresholds {
                    warn: 90,
                    suspend: 80
                }
            ),
            "got: {err}"
        );
        // Equal lines are legal (warn fires once in-band, suspend takes over at 100%).
        assert!(parse_str("budget_warn_pct: 100\nbudget_suspend_pct: 100\n").is_ok());
    }
}

//! The capability policy engine (architecture §3.1, §3.4).
//!
//! This is the security heart of the daemon: it decides, independently of the
//! caller, whether an [`ActRequest`] may run. The orchestrator cannot bypass it
//! by under-declaring a capability, because the required capability is
//! *recomputed from the op here*, never taken from the caller.

use oracle_ipc::actd::{ActRequest, Capability, ShellTier};
use std::collections::HashSet;

/// The tier a shell command actually warrants: the stricter of what the caller
/// declared and what [`crate::sandbox::classify`] reads out of the command text.
///
/// Every other op's capability follows from its shape, so a caller cannot
/// under-declare it. `ShellExec` is the exception — its tier travels *inside
/// the request*, written by the planner, which this daemon treats as untrusted
/// by design. Taken at face value, `ShellExec { cmd: "rm -rf ~", tier:
/// ReadOnly }` demanded only `Observe`, which is granted by default: allowed
/// outright, no confirmation. The classifier that catches exactly this ran
/// afterwards, at execution, where its verdict was reported rather than
/// enforced. Recomputing here is what makes the daemon's promise true.
fn effective_shell_tier(cmd: &str, declared: ShellTier) -> ShellTier {
    declared.max(crate::sandbox::classify(cmd).tier)
}

/// A standing grant: the set of capabilities the user has authorized for the
/// session, plus a lockdown flag that hard-disables actuation.
#[derive(Debug, Clone)]
pub struct PolicyState {
    granted: HashSet<Capability>,
    lockdown: bool,
    /// Processes whose windows must never receive injected input (§3.2).
    injection_denylist: Vec<String>,
}

impl Default for PolicyState {
    fn default() -> Self {
        let mut granted = HashSet::new();
        // Observe and benign actuation are granted by default; sensitive is not
        // (it requires per-action confirmation or an explicit standing grant).
        granted.insert(Capability::Observe);
        granted.insert(Capability::BenignAct);
        PolicyState {
            granted,
            lockdown: false,
            injection_denylist: vec![
                "keepassxc".into(),
                "1password".into(),
                "bitwarden".into(),
                "gnome-keyring".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Allowed outright.
    Allow,
    /// Allowed but requires spoken confirmation first (T2 / irreversible).
    NeedsConfirmation,
    /// Refused; carries a human-readable reason.
    Deny(String),
}

impl PolicyState {
    pub fn grant(&mut self, cap: Capability) {
        self.granted.insert(cap);
    }
    pub fn revoke(&mut self, cap: Capability) {
        self.granted.remove(&cap);
    }
    pub fn set_lockdown(&mut self, active: bool) {
        self.lockdown = active;
    }
    pub fn is_locked_down(&self) -> bool {
        self.lockdown
    }

    /// The core decision function. Ordering of checks matters: lockdown first
    /// (it overrides everything except un-lockdown), then capability, then
    /// confirmation for irreversible ops.
    pub fn evaluate(&self, req: &ActRequest) -> Decision {
        // Un-lockdown is always permitted (so the user can recover).
        if let ActRequest::SetLockdown { active: false } = req {
            return Decision::Allow;
        }
        if self.lockdown {
            return Decision::Deny("system is in lockdown; say 'unlock' to re-arm".into());
        }

        // Classified once, and used for both checks below: a shell command
        // under-declared as read-only must not slip past the capability gate
        // *or* the irreversibility gate.
        let shell_tier = match req {
            ActRequest::ShellExec { cmd, tier, .. } => Some(effective_shell_tier(cmd, *tier)),
            _ => None,
        };

        let required = match shell_tier {
            Some(t) => t.capability(),
            None => req.required_capability(),
        };
        if !self.granted.contains(&required) {
            // Sensitive ops that aren't standing-granted still get a path via
            // confirmation rather than a flat deny.
            if required == Capability::Sensitive {
                return Decision::NeedsConfirmation;
            }
            return Decision::Deny(format!("capability {required:?} not granted"));
        }

        // Even a granted sensitive op confirms when irreversible.
        let irreversible = match shell_tier {
            Some(t) => t == ShellTier::FullUser,
            None => req.is_irreversible(),
        };
        if irreversible {
            return Decision::NeedsConfirmation;
        }
        Decision::Allow
    }

    /// Whether input injection into a window owned by `proc_name` is permitted.
    pub fn injection_allowed(&self, proc_name: &str) -> bool {
        if self.lockdown {
            return false;
        }
        let lower = proc_name.to_lowercase();
        !self
            .injection_denylist
            .iter()
            .any(|deny| lower.contains(deny))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_ipc::actd::ShellTier;

    #[test]
    fn observe_is_allowed_by_default() {
        let p = PolicyState::default();
        assert_eq!(p.evaluate(&ActRequest::ListWindows), Decision::Allow);
    }

    #[test]
    fn sensitive_needs_confirmation_without_grant() {
        let p = PolicyState::default();
        assert_eq!(
            p.evaluate(&ActRequest::TypeText { text: "x".into() }),
            Decision::NeedsConfirmation
        );
    }

    #[test]
    fn irreversible_confirms_even_when_granted() {
        let mut p = PolicyState::default();
        p.grant(Capability::Sensitive);
        // kill process is sensitive AND irreversible → still confirms
        assert_eq!(
            p.evaluate(&ActRequest::KillProcess { pid: 100 }),
            Decision::NeedsConfirmation
        );
    }

    #[test]
    fn granted_reversible_sensitive_allows() {
        let mut p = PolicyState::default();
        p.grant(Capability::Sensitive);
        // TypeText is sensitive but reversible → allowed once granted
        assert_eq!(
            p.evaluate(&ActRequest::TypeText { text: "hi".into() }),
            Decision::Allow
        );
    }

    #[test]
    fn lockdown_denies_everything_except_unlock() {
        let mut p = PolicyState::default();
        p.set_lockdown(true);
        assert!(matches!(
            p.evaluate(&ActRequest::ListWindows),
            Decision::Deny(_)
        ));
        // un-lockdown still allowed
        assert_eq!(
            p.evaluate(&ActRequest::SetLockdown { active: false }),
            Decision::Allow
        );
    }

    #[test]
    fn readonly_shell_is_observe_tier() {
        let p = PolicyState::default();
        assert_eq!(
            p.evaluate(&ActRequest::ShellExec {
                cmd: "ls".into(),
                tier: ShellTier::ReadOnly,
                timeout_ms: 1000
            }),
            Decision::Allow
        );
    }

    #[test]
    fn full_user_shell_confirms() {
        let mut p = PolicyState::default();
        p.grant(Capability::Sensitive);
        assert_eq!(
            p.evaluate(&ActRequest::ShellExec {
                cmd: "rm -rf build".into(),
                tier: ShellTier::FullUser,
                timeout_ms: 1000
            }),
            Decision::NeedsConfirmation
        );
    }

    #[test]
    fn an_under_declared_shell_command_is_classified_not_believed() {
        // The planner is untrusted, and the tier travels inside its request.
        // Labelling a destructive command read-only used to map it to
        // `Capability::Observe` -- granted by default -- and clear it outright.
        let p = PolicyState::default();
        for cmd in [
            "rm -rf ~",
            "sudo systemctl stop firewalld",
            "mkfs.ext4 /dev/sda1",
        ] {
            assert_eq!(
                p.evaluate(&ActRequest::ShellExec {
                    cmd: cmd.into(),
                    tier: ShellTier::ReadOnly,
                    timeout_ms: 1000,
                }),
                Decision::NeedsConfirmation,
                "`{cmd}` declared read-only"
            );
        }
    }

    #[test]
    fn an_unknown_command_declared_readonly_still_needs_benign_act() {
        // Not every escalation is dramatic: the classifier's default for an
        // unrecognized command is workspace-write, and that has to hold too.
        let mut p = PolicyState::default();
        p.revoke(Capability::BenignAct);
        assert!(matches!(
            p.evaluate(&ActRequest::ShellExec {
                cmd: "make install".into(),
                tier: ShellTier::ReadOnly,
                timeout_ms: 1000,
            }),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn an_over_declared_command_keeps_the_stricter_tier() {
        // The reconciliation is `max`, not "trust the classifier": a caller
        // that voluntarily asks for a higher tier than `ls` needs is taken at
        // its word rather than quietly downgraded.
        let mut p = PolicyState::default();
        p.grant(Capability::Sensitive);
        assert_eq!(
            p.evaluate(&ActRequest::ShellExec {
                cmd: "ls -la".into(),
                tier: ShellTier::FullUser,
                timeout_ms: 1000,
            }),
            Decision::NeedsConfirmation
        );
    }

    #[test]
    fn injection_denied_for_password_managers() {
        let p = PolicyState::default();
        assert!(!p.injection_allowed("keepassxc"));
        assert!(!p.injection_allowed("1Password 8"));
        assert!(p.injection_allowed("firefox"));
    }
}

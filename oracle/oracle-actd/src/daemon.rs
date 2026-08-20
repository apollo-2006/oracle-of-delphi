//! The actuator daemon core: request handling that composes policy → PAL →
//! audit, plus the anti-replay nonce guard (architecture §3.1).
//!
//! The transport (UDS with SO_PEERCRED / named pipe) is a thin shell in
//! `main.rs`; this module is transport-agnostic and fully unit-tested so the
//! security-critical decision path is exercised without a socket.

use crate::audit::AuditJournal;
use crate::pal::Platform;
use crate::policy::{Decision, PolicyState};
use oracle_ipc::actd::{ActEnvelope, ActRequest, ActResponse};
use serde_json::json;
use std::sync::Mutex;
use uuid::Uuid;

pub struct Daemon<P: Platform> {
    platform: P,
    policy: Mutex<PolicyState>,
    audit: AuditJournal,
    /// Monotonic nonce high-water mark; replays (nonce <= seen) are rejected.
    last_nonce: Mutex<u64>,
    /// Pending confirmations: request_id → the envelope awaiting a yes/no.
    pending: Mutex<std::collections::HashMap<Uuid, ActRequest>>,
}

impl<P: Platform> Daemon<P> {
    pub fn new(platform: P, audit: AuditJournal) -> Self {
        Daemon {
            platform,
            policy: Mutex::new(PolicyState::default()),
            audit,
            last_nonce: Mutex::new(0),
            pending: Mutex::new(Default::default()),
        }
    }

    pub fn policy_mut(&self) -> std::sync::MutexGuard<'_, PolicyState> {
        self.policy.lock().unwrap()
    }

    /// Anti-replay: nonces must strictly increase within a connection.
    fn check_nonce(&self, nonce: u64) -> bool {
        let mut last = self.last_nonce.lock().unwrap();
        if nonce <= *last {
            return false;
        }
        *last = nonce;
        true
    }

    /// Handle one RPC envelope. This is the whole decision path in one place.
    pub fn handle(&self, env: ActEnvelope) -> ActResponse {
        if !self.check_nonce(env.nonce) {
            self.audit
                .log(&env.turn_id, "*", "denied", "replayed or stale nonce");
            return ActResponse::Denied {
                reason: "stale nonce (replay protection)".into(),
            };
        }

        // A Confirm RPC resolves a parked confirmation — route it straight to
        // the confirmation handler, bypassing the normal policy path.
        if let ActRequest::Confirm { request_id, allow } = env.request {
            return self.confirm(&env.turn_id, request_id, allow);
        }

        let op_name = op_name(&env.request);
        let decision = self.policy.lock().unwrap().evaluate(&env.request);

        match decision {
            Decision::Deny(reason) => {
                self.audit.log(&env.turn_id, op_name, "denied", &reason);
                ActResponse::Denied { reason }
            }
            Decision::NeedsConfirmation => {
                // Park the request; the confirmation arrives out-of-band from the
                // user (spoken "yes"/HUD button) and calls `confirm`.
                let rid = Uuid::new_v4();
                self.pending
                    .lock()
                    .unwrap()
                    .insert(rid, env.request.clone());
                self.audit.log(
                    &env.turn_id,
                    op_name,
                    "awaiting_confirmation",
                    &rid.to_string(),
                );
                ActResponse::Ok {
                    data: json!({ "needs_confirmation": true, "request_id": rid }),
                }
            }
            Decision::Allow => self.execute(&env.turn_id, env.request),
        }
    }

    /// Resolve a parked confirmation. `allow=false` discards it.
    pub fn confirm(&self, turn_id: &Uuid, request_id: Uuid, allow: bool) -> ActResponse {
        let req = self.pending.lock().unwrap().remove(&request_id);
        let Some(req) = req else {
            return ActResponse::Error {
                reason: "no such pending confirmation".into(),
            };
        };
        if !allow {
            self.audit.log(turn_id, op_name(&req), "denied_by_user", "");
            return ActResponse::Denied {
                reason: "user declined".into(),
            };
        }
        self.audit.log(turn_id, op_name(&req), "confirmed", "");
        self.execute(turn_id, req)
    }

    /// Actually perform the op via the PAL, after policy has cleared it.
    fn execute(&self, turn_id: &Uuid, req: ActRequest) -> ActResponse {
        let op = op_name(&req);
        let result = match req {
            ActRequest::ListWindows => self
                .platform
                .list_windows()
                .map(|w| json!({ "windows": w })),
            ActRequest::ListProcesses => self
                .platform
                .list_processes()
                .map(|p| json!({ "processes": p })),
            ActRequest::FocusWindow { window_id } => self
                .platform
                .focus_window(window_id)
                .map(|_| json!({ "ok": true })),
            ActRequest::KillProcess { pid } => self
                .platform
                .kill_process(pid)
                .map(|_| json!({ "ok": true })),
            ActRequest::TypeText { text } => {
                // Post-focus denylist re-check (§3.2): even if policy allowed
                // injection, refuse if the focused app is sensitive.
                match self.platform.focused_process_name() {
                    Ok(name) => {
                        if !self.policy.lock().unwrap().injection_allowed(&name) {
                            self.audit
                                .log(turn_id, op, "denied", &format!("focused={name}"));
                            return ActResponse::Denied {
                                reason: format!("injection into '{name}' is blocked"),
                            };
                        }
                        self.platform
                            .type_text(&text)
                            .map(|_| json!({ "typed": text.len() }))
                    }
                    Err(e) => Err(e),
                }
            }
            ActRequest::ShellExec { cmd, tier, .. } => {
                // Reference build does not spawn real shells; report the plan.
                let class = crate::sandbox::classify(&cmd);
                Ok(json!({
                    "planned": true,
                    "cmd": cmd,
                    "requested_tier": format!("{tier:?}"),
                    "classified_tier": format!("{:?}", class.tier),
                    "risks": format!("{:?}", class.risks),
                }))
            }
            ActRequest::SetLockdown { active } => {
                self.policy.lock().unwrap().set_lockdown(active);
                Ok(json!({ "lockdown": active }))
            }
            // Confirm is intercepted before execute() is ever reached.
            ActRequest::Confirm { .. } => Ok(json!({ "ignored": "confirm" })),
        };

        match result {
            Ok(data) => {
                self.audit.log(turn_id, op, "ok", "");
                ActResponse::Ok { data }
            }
            Err(e) => {
                self.audit.log(turn_id, op, "error", &e.to_string());
                ActResponse::Error {
                    reason: e.to_string(),
                }
            }
        }
    }
}

fn op_name(req: &ActRequest) -> &'static str {
    match req {
        ActRequest::ListWindows => "list_windows",
        ActRequest::ListProcesses => "list_processes",
        ActRequest::FocusWindow { .. } => "focus_window",
        ActRequest::KillProcess { .. } => "kill_process",
        ActRequest::TypeText { .. } => "type_text",
        ActRequest::ShellExec { .. } => "shell_exec",
        ActRequest::SetLockdown { .. } => "set_lockdown",
        ActRequest::Confirm { .. } => "confirm",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pal::MockPlatform;
    use oracle_ipc::actd::{Capability, ShellTier};

    fn daemon() -> Daemon<MockPlatform> {
        Daemon::new(
            MockPlatform::new(),
            AuditJournal::new(Box::new(std::io::sink())),
        )
    }

    fn env(nonce: u64, request: ActRequest) -> ActEnvelope {
        ActEnvelope {
            turn_id: Uuid::new_v4(),
            nonce,
            request,
        }
    }

    #[test]
    fn observe_op_executes() {
        let d = daemon();
        let r = d.handle(env(1, ActRequest::ListWindows));
        assert!(matches!(r, ActResponse::Ok { .. }));
    }

    #[test]
    fn replayed_nonce_is_denied() {
        let d = daemon();
        assert!(matches!(
            d.handle(env(5, ActRequest::ListWindows)),
            ActResponse::Ok { .. }
        ));
        // same nonce again → denied
        assert!(matches!(
            d.handle(env(5, ActRequest::ListProcesses)),
            ActResponse::Denied { .. }
        ));
        // lower nonce → denied
        assert!(matches!(
            d.handle(env(3, ActRequest::ListProcesses)),
            ActResponse::Denied { .. }
        ));
        // higher nonce → ok
        assert!(matches!(
            d.handle(env(6, ActRequest::ListProcesses)),
            ActResponse::Ok { .. }
        ));
    }

    #[test]
    fn kill_requires_confirmation_then_executes() {
        let d = daemon();
        d.policy_mut().grant(Capability::Sensitive);
        let r = d.handle(env(1, ActRequest::KillProcess { pid: 1002 }));
        let rid = match r {
            ActResponse::Ok { data } => {
                assert_eq!(data["needs_confirmation"], true);
                serde_json::from_value::<Uuid>(data["request_id"].clone()).unwrap()
            }
            other => panic!("expected confirmation, got {other:?}"),
        };
        // Confirm it.
        let turn = Uuid::new_v4();
        let done = d.confirm(&turn, rid, true);
        assert!(matches!(done, ActResponse::Ok { .. }));
        // Process is gone.
        match d.handle(env(2, ActRequest::ListProcesses)) {
            ActResponse::Ok { data } => {
                let procs = data["processes"].as_array().unwrap();
                assert!(procs.iter().all(|p| p["pid"] != 1002));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn declined_confirmation_does_not_execute() {
        let d = daemon();
        d.policy_mut().grant(Capability::Sensitive);
        let rid = match d.handle(env(1, ActRequest::KillProcess { pid: 1002 })) {
            ActResponse::Ok { data } => {
                serde_json::from_value::<Uuid>(data["request_id"].clone()).unwrap()
            }
            _ => panic!(),
        };
        let turn = Uuid::new_v4();
        assert!(matches!(
            d.confirm(&turn, rid, false),
            ActResponse::Denied { .. }
        ));
        // still there
        match d.handle(env(2, ActRequest::ListProcesses)) {
            ActResponse::Ok { data } => assert!(data["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == 1002)),
            _ => panic!(),
        }
    }

    #[test]
    fn injection_into_password_manager_is_denied() {
        let d = daemon();
        d.policy_mut().grant(Capability::Sensitive);
        // Focus the KeePassXC window (id 3).
        d.handle(env(1, ActRequest::FocusWindow { window_id: 3 }));
        // TypeText is sensitive+reversible → with the grant it's Allow and
        // executes directly, but the post-focus denylist re-check must refuse.
        match d.handle(env(
            2,
            ActRequest::TypeText {
                text: "secret".into(),
            },
        )) {
            ActResponse::Denied { reason } => assert!(reason.to_lowercase().contains("keepassxc")),
            other => panic!("expected denial on password manager, got {other:?}"),
        }
    }

    #[test]
    fn lockdown_blocks_then_unlocks() {
        let d = daemon();
        d.handle(env(1, ActRequest::SetLockdown { active: true }));
        assert!(matches!(
            d.handle(env(2, ActRequest::ListWindows)),
            ActResponse::Denied { .. }
        ));
        // unlock always allowed
        assert!(matches!(
            d.handle(env(3, ActRequest::SetLockdown { active: false })),
            ActResponse::Ok { .. }
        ));
        assert!(matches!(
            d.handle(env(4, ActRequest::ListWindows)),
            ActResponse::Ok { .. }
        ));
    }

    #[test]
    fn shell_exec_reports_classification() {
        let d = daemon();
        match d.handle(env(
            1,
            ActRequest::ShellExec {
                cmd: "ls -la".into(),
                tier: ShellTier::ReadOnly,
                timeout_ms: 1000,
            },
        )) {
            ActResponse::Ok { data } => assert_eq!(data["classified_tier"], "ReadOnly"),
            _ => panic!(),
        }
    }
}

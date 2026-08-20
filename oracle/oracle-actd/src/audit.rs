//! Append-only audit journal (architecture §3.1).
//!
//! Every actuation — allowed, denied, or confirmed — is recorded with its turn
//! id, so the agent can answer "what did you do while I was gone?" and a human
//! can review. The journal is append-only by discipline (open with append) and
//! records are single-line JSON for trivial tailing.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    pub t_unix: i64,
    pub turn_id: String,
    pub op: String,
    pub decision: String,
    pub detail: String,
}

/// A journal that writes to any `Write` sink (file in prod, buffer in tests).
pub struct AuditJournal {
    sink: Mutex<Box<dyn Write + Send>>,
}

impl AuditJournal {
    pub fn new(sink: Box<dyn Write + Send>) -> Self {
        AuditJournal {
            sink: Mutex::new(sink),
        }
    }

    pub fn record(&self, rec: &AuditRecord) {
        if let Ok(mut sink) = self.sink.lock() {
            let line = serde_json::to_string(rec).unwrap_or_default();
            let _ = writeln!(sink, "{line}");
            let _ = sink.flush();
        }
    }

    pub fn log(&self, turn_id: &uuid::Uuid, op: &str, decision: &str, detail: &str) {
        self.record(&AuditRecord {
            t_unix: chrono::Utc::now().timestamp(),
            turn_id: turn_id.to_string(),
            op: op.to_string(),
            decision: decision.to_string(),
            detail: detail.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// A Write sink that captures into a shared Vec for assertions.
    #[derive(Clone)]
    struct SharedBuf(Arc<StdMutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn records_are_appended_as_jsonl() {
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let j = AuditJournal::new(Box::new(SharedBuf(buf.clone())));
        let turn = uuid::Uuid::new_v4();
        j.log(&turn, "kill_process", "confirmed", "pid=1002");
        j.log(&turn, "list_windows", "allow", "");
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: AuditRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.op, "kill_process");
        assert_eq!(first.decision, "confirmed");
        assert!(first.detail.contains("1002"));
    }
}

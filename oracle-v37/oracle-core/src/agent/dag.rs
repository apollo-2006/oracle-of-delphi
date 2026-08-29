//! Dependency DAG for a tool batch (architecture §2.4).
//!
//! The model proposes a batch of tool calls whose arguments may reference the
//! results of earlier calls via `$result.N.path`. We do NOT trust the model's
//! declared ordering: we parse the *actual* `$result` references out of each
//! call's arguments and build the dependency edges ourselves. Independent
//! calls then fan out in parallel; dependent calls unlock as their inputs land.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// One requested tool call within a batch.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Batch-local id (index the model assigned, e.g. plan step id).
    pub id: u32,
    pub name: String,
    /// Raw arguments, possibly containing `$result.N.path` reference strings.
    pub args: Value,
}

/// A resolved dependency graph over a batch.
#[derive(Debug)]
pub struct DependencyDag {
    calls: HashMap<u32, ToolCall>,
    /// id -> set of ids it depends on.
    deps: HashMap<u32, HashSet<u32>>,
}

impl DependencyDag {
    /// Build the DAG by scanning every call's arguments for `$result.N`
    /// references. Self-references and references to absent ids are dropped
    /// (they'll surface as a resolution error at substitution time).
    pub fn from(calls: &[ToolCall]) -> Self {
        let ids: HashSet<u32> = calls.iter().map(|c| c.id).collect();
        let mut deps: HashMap<u32, HashSet<u32>> = HashMap::new();
        let mut map = HashMap::new();
        for c in calls {
            let mut d = HashSet::new();
            collect_refs(&c.args, &mut d);
            d.retain(|r| *r != c.id && ids.contains(r));
            deps.insert(c.id, d);
            map.insert(c.id, c.clone());
        }
        DependencyDag { calls: map, deps }
    }

    /// True if the graph has a cycle (would deadlock the dispatcher).
    pub fn has_cycle(&self) -> bool {
        let mut state: HashMap<u32, u8> = HashMap::new(); // 0=unseen 1=on-stack 2=done
        for &id in self.calls.keys() {
            if self.dfs_cycle(id, &mut state) {
                return true;
            }
        }
        false
    }

    fn dfs_cycle(&self, id: u32, state: &mut HashMap<u32, u8>) -> bool {
        match state.get(&id) {
            Some(2) => return false,
            Some(1) => return true,
            _ => {}
        }
        state.insert(id, 1);
        if let Some(ds) = self.deps.get(&id) {
            for &d in ds {
                if self.dfs_cycle(d, state) {
                    return true;
                }
            }
        }
        state.insert(id, 2);
        false
    }

    /// Calls with no outstanding dependencies given a set of completed ids.
    pub fn ready(&self, completed: &HashSet<u32>) -> Vec<ToolCall> {
        self.calls
            .values()
            .filter(|c| !completed.contains(&c.id))
            .filter(|c| {
                self.deps
                    .get(&c.id)
                    .map(|d| d.is_subset(completed))
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn get(&self, id: u32) -> Option<&ToolCall> {
        self.calls.get(&id)
    }
}

/// Recursively collect `$result.N` references from a JSON value.
fn collect_refs(v: &Value, out: &mut HashSet<u32>) {
    match v {
        Value::String(s) => {
            if let Some(id) = parse_result_ref(s) {
                out.insert(id);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

/// Parse the leading `$result.N` of a reference string, returning N.
/// Accepts `$result.3`, `$result.3.slots[0]`, etc.
pub fn parse_result_ref(s: &str) -> Option<u32> {
    let rest = s.strip_prefix("$result.")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Substitute `$result.N.path` references in `args` with concrete values from
/// the completed-results table. Returns an error naming the first unresolved
/// reference (so the model gets a precise repair hint).
pub fn substitute(args: &Value, results: &HashMap<u32, Value>) -> Result<Value, String> {
    match args {
        Value::String(s) => {
            if let Some(id) = parse_result_ref(s) {
                let base = results.get(&id).ok_or_else(|| {
                    format!("unresolved reference {s}: result {id} not available")
                })?;
                // Path after "$result.N."
                let after = s
                    .strip_prefix(&format!("$result.{id}"))
                    .unwrap_or("")
                    .trim_start_matches('.');
                resolve_path(base, after)
                    .ok_or_else(|| format!("path not found in result {id}: {after}"))
            } else {
                Ok(args.clone())
            }
        }
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                out.push(substitute(x, results)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, val) in o {
                m.insert(k.clone(), substitute(val, results)?);
            }
            Ok(Value::Object(m))
        }
        other => Ok(other.clone()),
    }
}

/// Navigate a dotted path with optional `[index]` segments, e.g.
/// `slots[0].start` on a JSON value.
fn resolve_path(base: &Value, path: &str) -> Option<Value> {
    if path.is_empty() {
        return Some(base.clone());
    }
    let mut cur = base;
    for seg in path.split('.') {
        // handle name[idx] and name[idx][idx2]
        let mut name = seg;
        let mut indices = Vec::new();
        if let Some(br) = seg.find('[') {
            name = &seg[..br];
            let mut rest = &seg[br..];
            while let Some(close) = rest.find(']') {
                let idx: usize = rest[1..close].parse().ok()?;
                indices.push(idx);
                rest = &rest[close + 1..];
            }
        }
        if !name.is_empty() {
            cur = cur.get(name)?;
        }
        for idx in indices {
            cur = cur.get(idx)?;
        }
    }
    Some(cur.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: u32, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id,
            name: name.into(),
            args,
        }
    }

    #[test]
    fn independent_calls_all_ready() {
        let dag = DependencyDag::from(&[
            call(1, "a", json!({})),
            call(2, "b", json!({})),
            call(4, "c", json!({})),
        ]);
        let ready = dag.ready(&HashSet::new());
        assert_eq!(ready.len(), 3);
        assert!(!dag.has_cycle());
    }

    #[test]
    fn dependency_edges_from_result_refs() {
        // step 3 depends on 1 and 2 via arg references
        let dag = DependencyDag::from(&[
            call(1, "gmail.search", json!({"q": "advisor"})),
            call(2, "cal.free", json!({"date": "tomorrow"})),
            call(
                3,
                "gmail.draft",
                json!({"thread": "$result.1.thread_id", "time": "$result.2.slots[0]"}),
            ),
        ]);
        let ready = dag.ready(&HashSet::new());
        let ready_ids: HashSet<u32> = ready.iter().map(|c| c.id).collect();
        assert_eq!(ready_ids, HashSet::from([1, 2])); // 3 is gated
        let after = dag.ready(&HashSet::from([1, 2]));
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, 3);
    }

    #[test]
    fn cycles_are_detected() {
        let dag = DependencyDag::from(&[
            call(1, "a", json!({"x": "$result.2"})),
            call(2, "b", json!({"y": "$result.1"})),
        ]);
        assert!(dag.has_cycle());
    }

    #[test]
    fn substitution_resolves_nested_paths() {
        let mut results = HashMap::new();
        results.insert(1, json!({"thread_id": "abc"}));
        results.insert(
            2,
            json!({"slots": [{"start": "14:00"}, {"start": "15:00"}]}),
        );
        let args = json!({
            "thread": "$result.1.thread_id",
            "time": "$result.2.slots[1].start",
            "literal": "unchanged"
        });
        let out = substitute(&args, &results).unwrap();
        assert_eq!(out["thread"], json!("abc"));
        assert_eq!(out["time"], json!("15:00"));
        assert_eq!(out["literal"], json!("unchanged"));
    }

    #[test]
    fn unresolved_reference_errors() {
        let results = HashMap::new();
        let args = json!({"x": "$result.9.foo"});
        assert!(substitute(&args, &results).is_err());
    }
}

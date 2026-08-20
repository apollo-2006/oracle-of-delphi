//! Tool registry + the validation ladder (architecture §2.3).
//!
//! A tool is defined once as a Rust type implementing [`Tool`]. Its JSON Schema
//! (shown to the model), its typed argument struct, and its dispatch glue all
//! derive from that single definition, so they can never drift apart.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub mod builtin;
pub mod os_tools;

/// Side-effect classification drives confirmation policy in the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffect {
    None,
    Reversible,
    Irreversible,
}

/// A structured tool result. `ok` results feed back into the model as
/// observations; `err` results are *also* observations (never exceptions) so
/// the agent can recover — see [`ToolError`].
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Ok(Value),
    Err(ToolError),
}

/// Errors are data. Every field is chosen to give the model a repair path.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolError {
    pub status: ToolErrorKind,
    pub field: Option<String>,
    pub reason: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    InvalidArgs,
    Transient,
    Denied,
    NotFound,
    Internal,
}

impl ToolError {
    pub fn invalid(field: &str, reason: &str, hint: &str) -> Self {
        ToolError {
            status: ToolErrorKind::InvalidArgs,
            field: Some(field.into()),
            reason: reason.into(),
            hint: Some(hint.into()),
        }
    }
    pub fn transient(reason: &str) -> Self {
        ToolError {
            status: ToolErrorKind::Transient,
            field: None,
            reason: reason.into(),
            hint: Some("retry may succeed".into()),
        }
    }
    /// Is this class of failure worth an automatic (below-model) retry?
    pub fn is_retryable(&self) -> bool {
        self.status == ToolErrorKind::Transient
    }
}

/// Context handed to every tool invocation. Holds shared handles (db, http,
/// connectors) behind `Arc` so tools stay cheap to construct.
#[derive(Clone)]
pub struct ToolCtx {
    pub turn_id: uuid::Uuid,
    pub shared: Arc<crate::Shared>,
}

/// The object-safe tool interface the registry stores.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the argument object, embedded into the model prompt.
    fn schema(&self) -> Value;
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }
    /// The whole ladder: parse → validate → execute. Implemented by the blanket
    /// impl below so concrete tools only write typed `run`.
    async fn dispatch(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome;
}

/// Ergonomic typed tool: implement this, get [`Tool`] for free via the adapter.
#[async_trait]
pub trait TypedTool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send;
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const SIDE_EFFECT: SideEffect = SideEffect::None;

    async fn run(&self, args: Self::Args, ctx: &ToolCtx) -> ToolOutcome;

    /// Optional post-parse semantic validation (ranges, cross-field rules).
    /// Return `Ok(())` to proceed. Default: accept.
    fn validate(_args: &Self::Args) -> Result<(), ToolError> {
        Ok(())
    }
}

/// Adapter that lifts a [`TypedTool`] into an object-safe [`Tool`], running the
/// full validation ladder around the typed `run`.
pub struct Typed<T: TypedTool>(pub T);

#[async_trait]
impl<T: TypedTool> Tool for Typed<T> {
    fn name(&self) -> &'static str {
        T::NAME
    }
    fn description(&self) -> &'static str {
        T::DESCRIPTION
    }
    fn schema(&self) -> Value {
        let schema = schemars::schema_for!(T::Args);
        serde_json::to_value(schema).unwrap_or(Value::Null)
    }
    fn side_effect(&self) -> SideEffect {
        T::SIDE_EFFECT
    }
    async fn dispatch(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        // Rung 2: typed parse (rungs 1/grammar happen at decode time).
        let parsed: T::Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                let field = extract_field(&e);
                return ToolOutcome::Err(ToolError::invalid(
                    field.as_deref().unwrap_or("?"),
                    &e.to_string(),
                    "check argument names and types against the schema",
                ));
            }
        };
        // Rung 3: semantic validation.
        if let Err(err) = T::validate(&parsed) {
            return ToolOutcome::Err(err);
        }
        // Rung 4 (capability/confirmation) is enforced by the orchestrator
        // before dispatch is ever called. Here we just run.
        self.0.run(parsed, ctx).await
    }
}

/// Best-effort extraction of the offending field name from a serde error, so
/// the model gets a targeted repair hint rather than a wall of text.
fn extract_field(e: &serde_json::Error) -> Option<String> {
    let s = e.to_string();
    // serde phrases: "missing field `foo`" / "unknown field `bar`"
    for marker in ["missing field `", "unknown field `"] {
        if let Some(i) = s.find(marker) {
            let rest = &s[i + marker.len()..];
            if let Some(end) = rest.find('`') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// The registry. Owns all tools; produces the schema block for the prompt and
/// dispatches by name.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: TypedTool>(&mut self, t: T) -> &mut Self {
        let boxed: Arc<dyn Tool> = Arc::new(Typed(t));
        self.tools.insert(boxed.name(), boxed);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.tools.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// The tool manifest injected into the system prompt.
    pub fn manifest(&self) -> Value {
        let mut arr = Vec::new();
        let mut names: Vec<_> = self.tools.keys().copied().collect();
        names.sort_unstable();
        for name in names {
            let t = &self.tools[name];
            arr.push(serde_json::json!({
                "name": t.name(),
                "description": t.description(),
                "parameters": t.schema(),
            }));
        }
        Value::Array(arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }
    struct Add;
    #[async_trait]
    impl TypedTool for Add {
        type Args = AddArgs;
        const NAME: &'static str = "math.add";
        const DESCRIPTION: &'static str = "Add two integers";
        async fn run(&self, args: AddArgs, _ctx: &ToolCtx) -> ToolOutcome {
            ToolOutcome::Ok(serde_json::json!({ "sum": args.a + args.b }))
        }
        fn validate(args: &AddArgs) -> Result<(), ToolError> {
            if args.a.checked_add(args.b).is_none() {
                return Err(ToolError::invalid("b", "overflow", "use smaller values"));
            }
            Ok(())
        }
    }

    #[test]
    fn registry_registers_and_lists() {
        let mut r = ToolRegistry::new();
        r.register(Add);
        assert_eq!(r.names(), vec!["math.add"]);
        assert!(r.get("math.add").is_some());
        // manifest carries the derived schema
        let m = r.manifest();
        assert!(m.to_string().contains("math.add"));
    }

    #[test]
    fn field_extraction_from_serde_error() {
        let e = serde_json::from_str::<AddArgs>("{\"a\": 1}").unwrap_err();
        assert_eq!(extract_field(&e).as_deref(), Some("b"));
    }
}

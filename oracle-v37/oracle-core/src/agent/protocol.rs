//! The model I/O protocol — grammar-constrained tool use.
//!
//! Instead of trusting the model to format tool calls (Qwen on llama.cpp drifts
//! between `<tool_call>` tags, bare JSON, `name(args)`, garbled braces…), we hand
//! llama.cpp a GBNF **grammar** that makes its output physically incapable of
//! being anything other than ONE of two shapes:
//!
//!   * a tool call:  `{"tool":"os.window","args":{…}}`
//!   * a final line: `{"say":"Spotify is maximized."}`
//!
//! The tool name is constrained to the actual registered tools. Because the
//! sampler can only emit valid tokens for this grammar, there is nothing to
//! "recover" — the parse is deterministic. This module builds the grammar, the
//! matching tool docs for the system prompt, and parses the model's reply.

use serde_json::Value;

/// What the model chose to do this step.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelAction {
    /// Invoke one tool.
    Call { tool: String, args: Value },
    /// Speak to the user — a final answer or a clarifying question.
    Say(String),
}

/// Parse a grammar-constrained reply. With the grammar in force this always
/// succeeds; `None` is a defensive fallback (e.g. the mock or a mis-set backend).
pub fn parse_action(text: &str) -> Option<ModelAction> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    if let Some(tool) = v.get("tool").and_then(|t| t.as_str()) {
        let args = v
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Some(ModelAction::Call {
            tool: tool.to_string(),
            args,
        });
    }
    if let Some(say) = v.get("say").and_then(|s| s.as_str()) {
        return Some(ModelAction::Say(say.to_string()));
    }
    None
}

/// The instruction block appended to the system prompt telling the model what the
/// two shapes mean (the grammar enforces them; this explains them).
pub const INSTRUCTIONS: &str = "OUTPUT PROTOCOL — respond with EXACTLY ONE JSON \
object and nothing else. To perform an action, emit a tool call: \
{\"tool\":\"<tool name>\",\"args\":{<arguments>}}. To speak to the user — a final \
answer, or a question you need answered — emit: {\"say\":\"<your words>\"}. Call \
ONE tool per response; after you see its result you may call another or speak. \
Never put a tool call inside \"say\". Use a tool for anything you can act on; only \
\"say\" when you have the answer or must ask something.";

/// Render the registered tools as a compact list for the system prompt. The
/// grammar names the tools; this tells the model what each does and its args.
pub fn render_tool_docs(manifest: &Value) -> String {
    let mut out = String::from("AVAILABLE TOOLS:\n");
    if let Some(arr) = manifest.as_array() {
        for t in arr {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let args = t
                .get("parameters")
                .and_then(|p| p.get("properties"))
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            out.push_str(&format!("- {name}({args}): {desc}\n"));
        }
    }
    out
}

/// Collect the tool names from a manifest, for building the grammar.
pub fn tool_names(manifest: &Value) -> Vec<String> {
    manifest
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

/// Build the GBNF grammar constraining output to `{"tool":…,"args":…}` (with the
/// tool name limited to `names`) or `{"say":…}`. When there are no tools, only
/// `say` is producible.
pub fn build_grammar(names: &[String]) -> String {
    let alts = if names.is_empty() {
        // Unsatisfiable-in-practice placeholder; `call` still needs a rule.
        "\"\\\"\\\"\"".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("\"\\\"{n}\\\"\""))
            .collect::<Vec<_>>()
            .join(" | ")
    };

    let mut g = String::new();
    g.push_str("root ::= sp (call | final) sp\n");
    g.push_str(
        "call ::= \"{\" sp \"\\\"tool\\\"\" sp \":\" sp toolname sp \",\" sp \"\\\"args\\\"\" sp \":\" sp object sp \"}\"\n",
    );
    g.push_str("final ::= \"{\" sp \"\\\"say\\\"\" sp \":\" sp string sp \"}\"\n");
    g.push_str(&format!("toolname ::= {alts}\n"));
    g.push_str("object ::= \"{\" sp ( member ( sp \",\" sp member )* )? sp \"}\"\n");
    g.push_str("member ::= string sp \":\" sp value\n");
    g.push_str("array ::= \"[\" sp ( value ( sp \",\" sp value )* )? sp \"]\"\n");
    g.push_str("value ::= object | array | string | number | \"true\" | \"false\" | \"null\"\n");
    g.push_str(
        "string ::= \"\\\"\" ( [^\"\\\\] | \"\\\\\" [\"\\\\/bfnrt] | \"\\\\u\" hex hex hex hex )* \"\\\"\"\n",
    );
    g.push_str(
        "number ::= \"-\"? ( \"0\" | [1-9] [0-9]* ) ( \".\" [0-9]+ )? ( [eE] [-+]? [0-9]+ )?\n",
    );
    g.push_str("hex ::= [0-9a-fA-F]\n");
    g.push_str("sp ::= [ \\t\\n\\r]*\n");
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_call() {
        let a =
            parse_action(r#"{"tool":"os.window","args":{"action":"minimize","query":"spotify"}}"#);
        match a {
            Some(ModelAction::Call { tool, args }) => {
                assert_eq!(tool, "os.window");
                assert_eq!(args["action"], "minimize");
            }
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn parses_say() {
        let a = parse_action(r#"  {"say":"Spotify is maximized."}  "#);
        assert_eq!(a, Some(ModelAction::Say("Spotify is maximized.".into())));
    }

    #[test]
    fn tool_call_without_args_defaults_empty() {
        let a = parse_action(r#"{"tool":"os.lock_screen"}"#);
        match a {
            Some(ModelAction::Call { tool, args }) => {
                assert_eq!(tool, "os.lock_screen");
                assert!(args.as_object().unwrap().is_empty());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn non_protocol_json_is_none() {
        assert_eq!(parse_action(r#"{"foo":1}"#), None);
        assert_eq!(parse_action("not json"), None);
    }

    #[test]
    fn grammar_lists_the_tools_and_shapes() {
        let g = build_grammar(&["os.window".into(), "os.media".into()]);
        // Tool names are enumerated as quoted JSON tokens.
        assert!(g.contains(r#""\"os.window\"""#));
        assert!(g.contains(r#""\"os.media\"""#));
        // Both shapes present.
        assert!(g.contains("call"));
        assert!(g.contains("final"));
        assert!(g.contains("root ::="));
    }

    #[test]
    fn docs_are_compact_one_line_per_tool() {
        let manifest = serde_json::json!([
            {"name":"os.window","description":"control a window","parameters":{"properties":{"query":{},"action":{}}}},
            {"name":"os.media","description":"media keys","parameters":{"properties":{"key":{}}}}
        ]);
        let docs = render_tool_docs(&manifest);
        // serde_json orders object keys alphabetically.
        assert!(docs.contains("- os.window(action, query): control a window"));
        assert!(docs.contains("- os.media(key): media keys"));
    }
}

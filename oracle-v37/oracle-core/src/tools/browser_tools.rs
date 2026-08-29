//! Web tools — Delphi's browser (Chrome via CDP, see [`crate::browser`]).
//!
//! `web.open` navigates, `web.read` reads the page (title + text + links), and
//! `web.click` clicks a link/button by its visible text. These run in core (not
//! actd): they drive a real Chrome, so what she "sees" is the actual DOM, not a
//! flaky accessibility snapshot. The browser launches lazily on first use.

use super::{ToolCtx, ToolError, ToolOutcome, TypedTool};
use schemars::JsonSchema;
use serde::Deserialize;

fn page_result(page: crate::browser::PageView) -> ToolOutcome {
    match serde_json::to_value(&page) {
        Ok(v) => ToolOutcome::Ok(v),
        Err(e) => ToolOutcome::Err(ToolError::transient(&format!("serializing page: {e}"))),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct OpenArgs {
    /// The URL (or bare host, e.g. "youtube.com") to open. For a YouTube search
    /// use "https://www.youtube.com/results?search_query=<terms>"; for a Google
    /// search "https://www.google.com/search?q=<terms>".
    pub url: String,
}
pub struct WebOpen;
#[async_trait::async_trait]
impl TypedTool for WebOpen {
    type Args = OpenArgs;
    const NAME: &'static str = "web.open";
    const DESCRIPTION: &'static str =
        "Open a URL in the user's Chrome and return the loaded page (title, text, links). Use \
         this to browse the web — search engines, YouTube, any site. Returns what's actually on \
         the page so you can then read or click it.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: OpenArgs, ctx: &ToolCtx) -> ToolOutcome {
        match ctx.shared.browser.open(&a.url).await {
            Ok(page) => page_result(page),
            Err(e) => ToolOutcome::Err(ToolError::transient(&format!("web.open: {e}"))),
        }
    }
    fn validate(a: &OpenArgs) -> Result<(), ToolError> {
        if a.url.trim().is_empty() {
            return Err(ToolError::invalid("url", "no url given", "pass a URL to open"));
        }
        Ok(())
    }
}

pub struct WebRead;
#[async_trait::async_trait]
impl TypedTool for WebRead {
    type Args = super::os_tools::NoArgs;
    const NAME: &'static str = "web.read";
    const DESCRIPTION: &'static str =
        "Read the CURRENT browser page — its title, visible text, and links (each with its URL). \
         Use after web.open to see results (e.g. video titles on a search page) before clicking. \
         Ground your answer only in what's returned.";
    async fn run(&self, _a: super::os_tools::NoArgs, ctx: &ToolCtx) -> ToolOutcome {
        match ctx.shared.browser.read().await {
            Ok(page) => page_result(page),
            Err(e) => ToolOutcome::Err(ToolError::transient(&format!("web.read: {e}"))),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ClickArgs {
    /// Visible text (or part of it) of the link or button to click, e.g. a video
    /// title from web.read, or "Sign in".
    pub text: String,
}
pub struct WebClick;
#[async_trait::async_trait]
impl TypedTool for WebClick {
    type Args = ClickArgs;
    const NAME: &'static str = "web.click";
    const DESCRIPTION: &'static str =
        "Click a link or button on the current browser page by its visible text (find the exact \
         text with web.read first), then return the page it leads to.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: ClickArgs, ctx: &ToolCtx) -> ToolOutcome {
        match ctx.shared.browser.click(&a.text).await {
            Ok(page) => page_result(page),
            Err(e) => ToolOutcome::Err(ToolError::transient(&format!("web.click: {e}"))),
        }
    }
    fn validate(a: &ClickArgs) -> Result<(), ToolError> {
        if a.text.trim().is_empty() {
            return Err(ToolError::invalid(
                "text",
                "no text given",
                "pass the visible text of the link/button to click",
            ));
        }
        Ok(())
    }
}

/// Register the web/browser tools.
pub fn register_all(reg: &mut super::ToolRegistry) {
    reg.register(WebOpen).register(WebRead).register(WebClick);
}

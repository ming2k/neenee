//! Integration test: panicking on assertion failure is the desired
//! behaviour here, so the workspace `unwrap_used`/`expect_used` lints
//! are relaxed for this file. (Lib/bin code stays linted.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration test: the QUANT profile isolates quant tools from coding tools.
//!
//! Built with REAL tool instances (the quant tools from this crate + the real
//! coding tools from `neenee-tools`), not stubs. This proves the per-role
//! tool allocation holds end-to-end: a quant envoy receives exactly its
//! read-only quant + inspection set, and never receives a coding write/edit
//! tool or the live-trading `place_order`. This is the "separate tool allocation
//! per role" contract pinned with concrete instances.

#![cfg(test)]

use std::sync::Arc;

use neenee_core::{QUANT, Tool, ToolSelection, ToolSet, resolve_model};
use neenee_quant::{BacktestTool, ListPositionsTool, MarketDataTool, PlaceOrderTool};

#[test]
fn quant_profile_selects_only_quant_readonly_tools_from_a_mixed_set() {
    // A mixed parent pool: quant tools (read + trade) + coding tools (read +
    // write). A real assembled quant agent would carry quant tools; here we
    // also throw in coding tools to prove the profile filters them out.
    let mixed = ToolSet::from_tools(vec![
        // Quant domain.
        Arc::new(MarketDataTool::new()) as Arc<dyn Tool>,
        Arc::new(BacktestTool::new()),
        Arc::new(ListPositionsTool::new()),
        Arc::new(PlaceOrderTool::new()),
        // Coding domain (would come from neenee-tools in a real binary).
        Arc::new(neenee_tools::ReadTextTool),
        Arc::new(neenee_tools::WriteFileTool),
        Arc::new(neenee_tools::EditFileTool),
        Arc::new(neenee_tools::BashTool),
    ]);

    // Resolve the pool for the QUANT role on a vision-capable model with no
    // model-side overrides — isolates the role-scope contract.
    let model = resolve_model("claude-opus-4-8");
    let selected = QUANT.resolve_tools(&mixed, &model, &ToolSelection::unrestricted());
    let names: Vec<&str> = selected.iter().map(|t| t.name()).collect();

    // Admitted: read-only quant + read-only inspection.
    assert!(names.contains(&"market_data"), "got: {names:?}");
    assert!(names.contains(&"backtest"), "got: {names:?}");
    assert!(names.contains(&"list_positions"), "got: {names:?}");
    assert!(names.contains(&"read_text"), "got: {names:?}");

    // Excluded: live trading.
    assert!(
        !names.contains(&"place_order"),
        "quant analyst must not receive place_order, got: {names:?}"
    );
    // Excluded: coding write/edit/exec — domain isolation.
    assert!(
        !names.contains(&"write_file"),
        "quant agent must not receive coding write tools, got: {names:?}"
    );
    assert!(!names.contains(&"edit_file"), "got: {names:?}");
    assert!(!names.contains(&"bash"), "got: {names:?}");

    // Exactly four tools admitted — no more, no less.
    assert_eq!(selected.len(), 4, "got: {names:?}");
}

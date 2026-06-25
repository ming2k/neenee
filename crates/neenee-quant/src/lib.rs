//! Quantitative-trading application for neenee.
//!
//! An *application-layer* crate, a peer of `neenee-code`: it depends on
//! `neenee-agent` (so it reuses the full turn/round loop, pursuits, permission
//! broker) and layers on quantitative-trading domain tools — market data,
//! backtesting, order placement — plus a future GUI. Layering:
//!
//! ```text
//! neenee-core ← {providers, tools, store} ← neenee-agent ← {neenee-code, neenee-quant}
//! ```
//!
//! ## Why these tools are *not* self-registered
//!
//! The built-in coding tools in `neenee-tools` self-register via
//! [`neenee_core::register_tool!`] so the coding binary collects them with a
//! single `collect_tools` call. These quant tools deliberately do **not**
//! self-register: a coding agent should never see a `place_order` tool in its
//! schema list, and a quant agent should never see `write_file`. Mixing them
//! would bloat the model's context and invite wrong-domain calls (exactly the
//! "tools 分配应该不同,不要搞混" requirement).
//!
//! Instead, each tool is exposed as a plain struct with a constructor
//! ([`MarketDataTool::new`], …). The quant application instantiates exactly
//! the set it wants and hands them to [`Agent::new`]. Tool/role isolation is
//! therefore enforced at assembly time, not by runtime filtering. See the
//! [`QUANT`](neenee_core::QUANT) profile for the matching admission policy for
//! quant sub-agents.

pub mod market_data;
pub mod backtest;
pub mod orders;

pub use market_data::MarketDataTool;
pub use backtest::BacktestTool;
pub use orders::{PlaceOrderTool, ListPositionsTool};

/// Every quant tool, constructed with defaults. A convenience for a binary
/// assembling a quant agent: `neenee_quant::default_tools()` returns the full
/// quant toolset, and the caller decides which (if any) to hand to an agent.
///
/// Kept as an explicit function (not `register_tool!`) on purpose — see the
/// crate docs on why quant tools stay out of the global self-registry.
pub fn default_tools() -> Vec<std::sync::Arc<dyn neenee_core::Tool>> {
    use std::sync::Arc;
    vec![
        Arc::new(MarketDataTool::new()),
        Arc::new(BacktestTool::new()),
        Arc::new(PlaceOrderTool::new()),
        Arc::new(ListPositionsTool::new()),
    ]
}

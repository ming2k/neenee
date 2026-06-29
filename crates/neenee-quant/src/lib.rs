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
//! single `collect_toolset` call. These quant tools deliberately do **not**
//! self-register: a coding agent should never see a `place_order` tool in its
//! schema list, and a quant agent should never see `write_file`. Mixing them
//! would bloat the model's context and invite wrong-domain calls (exactly the
//! "separate tool allocation per role" requirement).
//!
//! Instead, each tool is exposed as a plain struct with a constructor
//! ([`MarketDataTool::new`], …). The quant application instantiates exactly
//! the set it wants and hands them to `Agent::new`. Tool/role isolation is
//! therefore enforced at assembly time, not by runtime filtering. See the
//! [`QUANT`](neenee_core::QUANT) profile for the matching admission policy for
//! quant envoys.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod backtest;
pub mod config;
pub mod market_data;
pub mod orders;
pub mod runtime;

pub use backtest::BacktestTool;
pub use config::{MarketDataConfig, PaperRuntimeConfig, QuantConfig};
pub use market_data::MarketDataTool;
pub use orders::{CancelOrderTool, ListPositionsTool, PlaceOrderTool};
pub use runtime::{
    AuditSink, BinanceMarketData, BrokerAdapter, CancelOrderRequest, DefaultRiskPolicy,
    JsonHttpTransport, JsonlAuditSink, MarketDataAdapter, NoopAuditSink, OrderDecision,
    OrderRequest, OrderSide, OrderType, PaperBroker, QuantRuntime, ReqwestJsonTransport,
    RiskLimits, RiskPolicy, SyntheticMarketData, default_paper_starting_cash,
};

/// Every quant tool, constructed with defaults. A convenience for a binary
/// assembling a quant agent: `neenee_quant::default_tools()` returns the full
/// quant toolset, and the caller decides which (if any) to hand to an agent.
///
/// Kept as an explicit function (not `register_tool!`) on purpose — see the
/// crate docs on why quant tools stay out of the global self-registry.
pub fn default_tools() -> Vec<std::sync::Arc<dyn neenee_core::Tool>> {
    use std::sync::Arc;
    let runtime = QuantRuntime::new();
    vec![
        Arc::new(MarketDataTool::with_runtime(runtime.clone())),
        Arc::new(BacktestTool::with_runtime(runtime.clone())),
        Arc::new(PlaceOrderTool::with_runtime(runtime.clone())),
        Arc::new(CancelOrderTool::with_runtime(runtime.clone())),
        Arc::new(ListPositionsTool::with_runtime(runtime)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tools_returns_all_quant_tools() {
        let tools = default_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "backtest",
                "cancel_order",
                "list_positions",
                "market_data",
                "place_order"
            ],
            "default_tools must expose the complete quant toolset"
        );
    }

    #[test]
    fn default_tools_has_no_duplicate_names() {
        let tools = default_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate tool names would shadow at registration"
        );
    }

    #[test]
    fn default_tools_includes_the_live_trading_tool() {
        // The full set deliberately includes account-mutating trading tools;
        // profile-based admission (not default_tools) hides them from analysts.
        let tools = default_tools();
        assert!(
            tools.iter().any(|t| t.name() == "place_order"),
            "default_tools is the unrestricted set"
        );
        assert!(
            tools.iter().any(|t| t.name() == "cancel_order"),
            "default_tools is the unrestricted set"
        );
    }
}

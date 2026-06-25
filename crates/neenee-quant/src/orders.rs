//! Order tools — place live orders and inspect open positions.
//!
//! Unlike [`crate::market_data`] and [`crate::backtest`] (read-only), these
//! tools take real, account-mutating actions: placing an order moves capital.
//! They are the quant domain's analogue of the coding domain's `write_file` /
//! `edit_file` (side-effecting, broker-gated). A quant agent that should only
//! *analyze* (not trade) must not receive them — the [`QUANT`](neenee_core::QUANT)
//! profile encodes exactly that split.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::{ScopeTarget, Tool};
use serde_json::json;

/// Place a live order on the configured exchange/broker.
///
/// Side-effecting and account-mutating. Reports a [`ScopeTarget::Command`]
/// style target so an operation-scope gate can restrain live trading if
/// desired (e.g. a "paper-only" sub-agent scope).
pub struct PlaceOrderTool {
    _private: (),
}

impl PlaceOrderTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for PlaceOrderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PlaceOrderTool {
    fn name(&self) -> &str {
        "place_order"
    }
    fn description(&self) -> &str {
        "Place a LIVE order on the configured exchange/broker. This moves real \
         capital — use only after a strategy has been backtested and the user \
         has approved trading. Prefer ask_user to confirm ambiguous size/side \
         decisions before placing. Returns the exchange order id and status."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Trading symbol, e.g. BTCUSDT" },
                "side": { "type": "string", "enum": ["buy", "sell"] },
                "type": {
                    "type": "string",
                    "enum": ["market", "limit"],
                    "description": "market = fill at best price; limit = fill at price or better"
                },
                "quantity": { "type": "number", "description": "Order size in base currency" },
                "price": { "type": "number", "description": "Required for limit orders; ignored for market" }
            },
            "required": ["symbol", "side", "type", "quantity"]
        })
    }
    /// Surfaced as a Command target so an operation scope can gate live
    /// trading (e.g. a paper-trading sub-agent). The command string carries
    /// the symbol/side so a permission prompt is informative.
    fn scope_target(&self, arguments: &str) -> ScopeTarget {
        let side = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["side"].as_str().map(str::to_string))
            .unwrap_or_else(|| "?".to_string());
        ScopeTarget::Command(format!("trade:{side}"))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let symbol = args["symbol"]
            .as_str()
            .ok_or("Missing 'symbol'")?
            .to_string();
        let side = args["side"].as_str().ok_or("Missing 'side'")?;
        let order_type = args["type"].as_str().ok_or("Missing 'type'")?;
        let quantity = args["quantity"]
            .as_f64()
            .ok_or("Missing or non-numeric 'quantity'")?;
        Ok(json!({
            "status": "stub",
            "order_id": "STUB-000000",
            "symbol": symbol,
            "side": side,
            "type": order_type,
            "quantity": quantity,
            "note": "stub place_order — wire a real exchange adapter before going live",
        })
        .to_string())
    }
}

/// List currently open positions / working orders on the account.
///
/// Read-only with respect to the market, but reads private account state. A
/// quant agent uses it to review exposure before adjusting positions.
pub struct ListPositionsTool {
    _private: (),
}

impl ListPositionsTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ListPositionsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListPositionsTool {
    fn name(&self) -> &str {
        "list_positions"
    }
    fn description(&self) -> &str {
        "List the account's currently open positions and working orders. \
         Read-only — does not place or cancel anything. Use this to review \
         current exposure before deciding to adjust positions."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Optional filter: only return positions for this symbol" }
            }
        })
    }
    async fn call(&self, _arguments: &str) -> Result<String, String> {
        Ok(json!({
            "positions": [],
            "open_orders": [],
            "note": "stub list_positions — wire a real exchange/broker adapter",
        })
        .to_string())
    }
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn place_order() -> Arc<dyn Tool> {
    Arc::new(PlaceOrderTool::new())
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn list_positions() -> Arc<dyn Tool> {
    Arc::new(ListPositionsTool::new())
}

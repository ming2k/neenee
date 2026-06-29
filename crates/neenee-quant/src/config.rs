use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    BinanceMarketData, DefaultRiskPolicy, JsonlAuditSink, LiveHttpBroker, PaperBroker,
    QuantRuntime, RiskLimits, SyntheticMarketData, default_paper_starting_cash,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuantConfig {
    #[serde(default)]
    pub market_data: MarketDataConfig,
    #[serde(default)]
    pub broker: BrokerRuntimeConfig,
    #[serde(default)]
    pub paper: PaperRuntimeConfig,
}

impl QuantConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read quant config {} failed: {e}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| format!("parse quant config {} failed: {e}", path.display()))
    }

    pub fn from_environment() -> Result<Self, String> {
        let mut config = match std::env::var("NEENEE_QUANT_CONFIG") {
            Ok(path) if !path.trim().is_empty() => Self::from_file(path)?,
            _ => Self::default(),
        };
        config.apply_env_lookup(|key| std::env::var(key).ok())?;
        Ok(config)
    }

    pub fn apply_env_lookup(
        &mut self,
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<(), String> {
        if let Some(source) = get("NEENEE_QUANT_MARKET_DATA") {
            self.market_data.source = source;
        }
        if let Some(base_url) = get("NEENEE_QUANT_BINANCE_BASE_URL") {
            self.market_data.binance_base_url = base_url;
        }
        if let Some(mode) = get("NEENEE_QUANT_BROKER") {
            self.broker.mode = mode;
        }
        if let Some(base_url) = get("NEENEE_QUANT_LIVE_BROKER_URL") {
            self.broker.live_http.base_url = base_url;
        }
        if let Some(token_env) = get("NEENEE_QUANT_LIVE_BROKER_TOKEN_ENV") {
            self.broker.live_http.token_env = token_env;
        }
        if let Some(token) = get("NEENEE_QUANT_LIVE_BROKER_TOKEN") {
            self.broker.live_http.token = non_empty_string(token);
        } else if !self.broker.live_http.token_env.trim().is_empty()
            && let Some(token) = get(&self.broker.live_http.token_env)
        {
            self.broker.live_http.token = non_empty_string(token);
        }
        if let Some(path) = get("NEENEE_QUANT_AUDIT_LOG") {
            self.paper.audit_log = non_empty_path(path);
        }
        if let Some(path) = get("NEENEE_QUANT_PAPER_STATE") {
            self.paper.state_path = non_empty_path(path);
        }
        if let Some(value) = get("NEENEE_QUANT_PAPER_STARTING_CASH") {
            self.paper.starting_cash =
                parse_positive_f64_env("NEENEE_QUANT_PAPER_STARTING_CASH", &value)?;
        }
        if let Some(value) = get("NEENEE_QUANT_PAPER_COMMISSION_BPS") {
            self.paper.commission_bps =
                parse_non_negative_f64_env("NEENEE_QUANT_PAPER_COMMISSION_BPS", &value)?;
        }
        if let Some(value) = get("NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL") {
            self.paper.risk.max_order_notional =
                parse_f64_env("NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL", &value)?;
        }
        if let Some(value) = get("NEENEE_QUANT_RISK_MAX_GROSS_EXPOSURE") {
            self.paper.risk.max_gross_exposure =
                parse_f64_env("NEENEE_QUANT_RISK_MAX_GROSS_EXPOSURE", &value)?;
        }
        if let Some(value) = get("NEENEE_QUANT_RISK_ALLOW_SHORT_SELLING") {
            self.paper.risk.allow_short_selling =
                parse_bool_env("NEENEE_QUANT_RISK_ALLOW_SHORT_SELLING", &value)?;
        }
        Ok(())
    }

    pub fn build_runtime(&self) -> Result<QuantRuntime, String> {
        if self.paper.starting_cash <= 0.0 || !self.paper.starting_cash.is_finite() {
            return Err("paper.starting_cash must be a positive finite number".to_string());
        }
        if self.paper.commission_bps < 0.0 || !self.paper.commission_bps.is_finite() {
            return Err("paper.commission_bps must be a non-negative finite number".to_string());
        }
        let market_data = match self.market_data.source.as_str() {
            "synthetic" | "synthetic-paper" => {
                Arc::new(SyntheticMarketData::default()) as Arc<dyn crate::MarketDataAdapter>
            }
            "binance" | "binance-http" => Arc::new(BinanceMarketData::with_base_url(
                self.market_data.binance_base_url.clone(),
            )) as Arc<dyn crate::MarketDataAdapter>,
            other => return Err(format!("unknown quant market data source '{other}'")),
        };

        let risk = Arc::new(DefaultRiskPolicy::new(self.paper.risk.clone()));
        let audit = self
            .paper
            .audit_log
            .as_ref()
            .map(|path| Arc::new(JsonlAuditSink::new(path.clone())) as Arc<dyn crate::AuditSink>)
            .unwrap_or_else(|| Arc::new(crate::NoopAuditSink::default()));
        let broker = match self.broker.mode.as_str() {
            "paper" | "paper-trading" => match &self.paper.state_path {
                Some(path) => Arc::new(
                    PaperBroker::new_with_audit_starting_cash_state_and_commission(
                        risk,
                        audit,
                        self.paper.starting_cash,
                        path.clone(),
                        self.paper.commission_bps,
                    )?,
                ) as Arc<dyn crate::BrokerAdapter>,
                None => Arc::new(PaperBroker::new_with_audit_starting_cash_and_commission(
                    risk,
                    audit,
                    self.paper.starting_cash,
                    self.paper.commission_bps,
                )) as Arc<dyn crate::BrokerAdapter>,
            },
            "live-http" => {
                let token = self
                    .broker
                    .live_http
                    .token
                    .as_deref()
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| "live broker token is required".to_string())?;
                Arc::new(LiveHttpBroker::new(
                    self.broker.live_http.base_url.clone(),
                    token.to_string(),
                    risk,
                    audit,
                )?) as Arc<dyn crate::BrokerAdapter>
            }
            other => return Err(format!("unknown quant broker mode '{other}'")),
        };
        Ok(QuantRuntime::with_adapters(market_data, broker))
    }

    pub fn market_data_source_label(&self) -> &'static str {
        match self.market_data.source.as_str() {
            "binance" | "binance-http" => "binance-http",
            _ => "synthetic-paper",
        }
    }

    pub fn summary(&self) -> String {
        let audit = self
            .paper
            .audit_log
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "disabled".to_string());
        let state = self
            .paper
            .state_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "memory".to_string());
        format!(
            "market={}, broker={}, paper_cash={}, commission_bps={}, state={}, audit={}, max_order_notional={}, max_gross_exposure={}, short_selling={}",
            self.market_data_source_label(),
            self.broker.mode,
            self.paper.starting_cash,
            self.paper.commission_bps,
            state,
            audit,
            self.paper.risk.max_order_notional,
            self.paper.risk.max_gross_exposure,
            self.paper.risk.allow_short_selling
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDataConfig {
    #[serde(default = "default_market_data_source")]
    pub source: String,
    #[serde(default = "default_binance_base_url")]
    pub binance_base_url: String,
}

impl Default for MarketDataConfig {
    fn default() -> Self {
        Self {
            source: default_market_data_source(),
            binance_base_url: default_binance_base_url(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerRuntimeConfig {
    #[serde(default = "default_broker_mode")]
    pub mode: String,
    #[serde(default)]
    pub live_http: LiveHttpBrokerConfig,
}

impl Default for BrokerRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: default_broker_mode(),
            live_http: LiveHttpBrokerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveHttpBrokerConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_live_broker_token_env")]
    pub token_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Default for LiveHttpBrokerConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            token_env: default_live_broker_token_env(),
            token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperRuntimeConfig {
    #[serde(default = "default_paper_starting_cash")]
    pub starting_cash: f64,
    #[serde(default)]
    pub commission_bps: f64,
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    #[serde(default)]
    pub audit_log: Option<PathBuf>,
    #[serde(default)]
    pub risk: RiskLimits,
}

impl Default for PaperRuntimeConfig {
    fn default() -> Self {
        Self {
            starting_cash: default_paper_starting_cash(),
            commission_bps: 0.0,
            state_path: None,
            audit_log: None,
            risk: RiskLimits::default(),
        }
    }
}

fn default_market_data_source() -> String {
    "synthetic".to_string()
}

fn default_broker_mode() -> String {
    "paper".to_string()
}

fn default_binance_base_url() -> String {
    "https://api.binance.com".to_string()
}

fn default_live_broker_token_env() -> String {
    "NEENEE_QUANT_LIVE_BROKER_TOKEN".to_string()
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_path(path: String) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn parse_f64_env(key: &str, value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .map_err(|e| format!("{key} must be numeric: {e}"))
}

fn parse_positive_f64_env(key: &str, value: &str) -> Result<f64, String> {
    let parsed = parse_f64_env(key, value)?;
    if parsed <= 0.0 || !parsed.is_finite() {
        Err(format!("{key} must be a positive finite number"))
    } else {
        Ok(parsed)
    }
}

fn parse_non_negative_f64_env(key: &str, value: &str) -> Result<f64, String> {
    let parsed = parse_f64_env(key, value)?;
    if parsed < 0.0 || !parsed.is_finite() {
        Err(format!("{key} must be a non-negative finite number"))
    } else {
        Ok(parsed)
    }
}

fn parse_bool_env(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{key} must be a boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_synthetic_paper_runtime() {
        let config = QuantConfig::default();

        assert_eq!(config.market_data.source, "synthetic");
        assert_eq!(config.market_data_source_label(), "synthetic-paper");
        assert_eq!(config.broker.mode, "paper");
        assert!(config.paper.audit_log.is_none());
        assert_eq!(config.paper.starting_cash, 100_000.0);
        assert_eq!(config.paper.commission_bps, 0.0);
        assert_eq!(config.paper.risk.max_order_notional, 50_000.0);
    }

    #[test]
    fn config_loads_from_json_file() {
        let path = std::env::temp_dir().join(format!(
            "neenee-quant-config-{}-load.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{
                "market_data": {
                    "source": "binance",
                    "binance_base_url": "https://example.test"
                },
                "paper": {
                    "starting_cash": 25000,
                    "commission_bps": 7.5,
                    "state_path": "/tmp/quant-state.json",
                    "audit_log": "/tmp/quant-audit.jsonl",
                    "risk": {
                        "max_order_notional": 10,
                        "max_gross_exposure": 20,
                        "allow_short_selling": true
                    }
                }
            }"#,
        )
        .expect("write config");

        let config = QuantConfig::from_file(&path).expect("load config");

        assert_eq!(config.market_data.source, "binance");
        assert_eq!(config.market_data.binance_base_url, "https://example.test");
        assert_eq!(
            config.paper.audit_log.as_deref(),
            Some(Path::new("/tmp/quant-audit.jsonl"))
        );
        assert_eq!(
            config.paper.state_path.as_deref(),
            Some(Path::new("/tmp/quant-state.json"))
        );
        assert_eq!(config.paper.starting_cash, 25_000.0);
        assert_eq!(config.paper.commission_bps, 7.5);
        assert_eq!(config.paper.risk.max_order_notional, 10.0);
        assert!(config.paper.risk.allow_short_selling);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn env_lookup_overrides_file_or_default_values() {
        let mut config = QuantConfig::default();

        config
            .apply_env_lookup(|key| match key {
                "NEENEE_QUANT_MARKET_DATA" => Some("binance".to_string()),
                "NEENEE_QUANT_BINANCE_BASE_URL" => Some("https://example.test".to_string()),
                "NEENEE_QUANT_BROKER" => Some("live-http".to_string()),
                "NEENEE_QUANT_LIVE_BROKER_URL" => Some("https://broker.test".to_string()),
                "NEENEE_QUANT_LIVE_BROKER_TOKEN" => Some("secret-token".to_string()),
                "NEENEE_QUANT_AUDIT_LOG" => Some("/tmp/audit.jsonl".to_string()),
                "NEENEE_QUANT_PAPER_STATE" => Some("/tmp/state.json".to_string()),
                "NEENEE_QUANT_PAPER_STARTING_CASH" => Some("789".to_string()),
                "NEENEE_QUANT_PAPER_COMMISSION_BPS" => Some("12.5".to_string()),
                "NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL" => Some("123".to_string()),
                "NEENEE_QUANT_RISK_MAX_GROSS_EXPOSURE" => Some("456".to_string()),
                "NEENEE_QUANT_RISK_ALLOW_SHORT_SELLING" => Some("true".to_string()),
                _ => None,
            })
            .expect("env override");

        assert_eq!(config.market_data.source, "binance");
        assert_eq!(config.market_data.binance_base_url, "https://example.test");
        assert_eq!(config.broker.mode, "live-http");
        assert_eq!(config.broker.live_http.base_url, "https://broker.test");
        assert_eq!(
            config.broker.live_http.token.as_deref(),
            Some("secret-token")
        );
        assert_eq!(
            config.paper.audit_log.as_deref(),
            Some(Path::new("/tmp/audit.jsonl"))
        );
        assert_eq!(
            config.paper.state_path.as_deref(),
            Some(Path::new("/tmp/state.json"))
        );
        assert_eq!(config.paper.starting_cash, 789.0);
        assert_eq!(config.paper.commission_bps, 12.5);
        assert_eq!(config.paper.risk.max_order_notional, 123.0);
        assert_eq!(config.paper.risk.max_gross_exposure, 456.0);
        assert!(config.paper.risk.allow_short_selling);
    }

    #[test]
    fn env_lookup_can_resolve_live_broker_token_from_custom_env_name() {
        let mut config = QuantConfig::default();
        config
            .apply_env_lookup(|key| match key {
                "NEENEE_QUANT_BROKER" => Some("live-http".to_string()),
                "NEENEE_QUANT_LIVE_BROKER_URL" => Some("https://broker.test".to_string()),
                "NEENEE_QUANT_LIVE_BROKER_TOKEN_ENV" => Some("BROKER_TOKEN".to_string()),
                "BROKER_TOKEN" => Some("from-custom-env".to_string()),
                _ => None,
            })
            .expect("env override");

        assert_eq!(config.broker.mode, "live-http");
        assert_eq!(
            config.broker.live_http.token.as_deref(),
            Some("from-custom-env")
        );
    }

    #[test]
    fn live_http_broker_requires_explicit_gateway_and_token() {
        let mut config = QuantConfig::default();
        config.broker.mode = "live-http".to_string();

        let err = match config.build_runtime() {
            Ok(_) => panic!("missing token and url should fail"),
            Err(err) => err,
        };
        assert!(err.contains("live broker token"), "err: {err}");

        config.broker.live_http.token = Some("secret".to_string());
        let err = match config.build_runtime() {
            Ok(_) => panic!("missing url should fail"),
            Err(err) => err,
        };
        assert!(err.contains("gateway URL"), "err: {err}");

        config.broker.live_http.base_url = "http://broker.test".to_string();
        let err = match config.build_runtime() {
            Ok(_) => panic!("non-https broker url should fail"),
            Err(err) => err,
        };
        assert!(err.contains("https"), "err: {err}");
    }

    #[test]
    fn config_builds_runtime() {
        let config = QuantConfig::default();
        let runtime = config.build_runtime().expect("runtime");

        let quote = runtime.quote("BTCUSDT").expect("quote");

        assert_eq!(quote.symbol, "BTCUSDT");
        assert_eq!(quote.source, "synthetic-paper");
    }

    #[test]
    fn config_builds_runtime_with_configured_paper_cash() {
        let mut config = QuantConfig::default();
        config.paper.starting_cash = 12_345.0;
        let runtime = config.build_runtime().expect("runtime");

        let portfolio = runtime.portfolio(None);

        assert_eq!(portfolio.account.cash, 12_345.0);
        assert_eq!(portfolio.account.equity, 12_345.0);
    }

    #[test]
    fn config_builds_runtime_with_configured_commission() {
        let mut config = QuantConfig::default();
        config.paper.commission_bps = 100.0;
        let runtime = config.build_runtime().expect("runtime");

        let decision = runtime
            .place_order(crate::OrderRequest {
                symbol: "AAPL".to_string(),
                side: crate::OrderSide::Buy,
                order_type: crate::OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("place order");

        let order = decision.order.as_ref().expect("order");
        assert!(order.commission > 0.0);
        assert_eq!(decision.account.total_commission, order.commission);
        assert!(config.summary().contains("commission_bps=100"));
    }

    #[test]
    fn config_builds_runtime_that_persists_paper_state() {
        let state_path = std::env::temp_dir().join(format!(
            "neenee-quant-paper-state-{}-persist.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);
        let mut config = QuantConfig::default();
        config.paper.state_path = Some(state_path.clone());
        config.paper.risk.max_order_notional = 100_000.0;

        let runtime = config.build_runtime().expect("runtime");
        runtime
            .place_order(crate::OrderRequest {
                symbol: "AAPL".to_string(),
                side: crate::OrderSide::Buy,
                order_type: crate::OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("place order");

        let raw = std::fs::read_to_string(&state_path).expect("read state");
        let state: serde_json::Value = serde_json::from_str(&raw).expect("state json");
        assert_eq!(state["version"], 1);
        assert!(state["account"].is_object());

        let restored = config.build_runtime().expect("restored runtime");
        let portfolio = restored.portfolio(Some("AAPL"));

        assert_eq!(portfolio.positions.len(), 1);
        assert_eq!(portfolio.order_history.len(), 1);
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn persisted_open_order_can_be_cancelled_after_restart() {
        let state_path = std::env::temp_dir().join(format!(
            "neenee-quant-paper-state-{}-open-order.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);
        let mut config = QuantConfig::default();
        config.paper.state_path = Some(state_path.clone());

        let runtime = config.build_runtime().expect("runtime");
        runtime
            .place_order(crate::OrderRequest {
                symbol: "BTCUSDT".to_string(),
                side: crate::OrderSide::Buy,
                order_type: crate::OrderType::Limit,
                quantity: 0.1,
                price: Some(64_000.0),
            })
            .expect("place open order");

        let restored = config.build_runtime().expect("restored runtime");
        assert_eq!(restored.portfolio(None).open_orders.len(), 1);

        let decision = restored
            .cancel_order(crate::CancelOrderRequest {
                order_id: "PAPER-000000".to_string(),
            })
            .expect("cancel restored order");
        assert_eq!(decision.status, "cancelled_paper");

        let restored = config.build_runtime().expect("restored after cancel");
        let portfolio = restored.portfolio(None);
        assert!(portfolio.open_orders.is_empty());
        assert_eq!(portfolio.order_history.len(), 2);
        assert_eq!(portfolio.order_history[1].status, "cancelled_paper");
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn legacy_unversioned_paper_state_still_loads() {
        let state_path = std::env::temp_dir().join(format!(
            "neenee-quant-paper-state-{}-legacy.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);
        std::fs::write(
            &state_path,
            r#"{
                "next_decision_id": 7,
                "next_order_id": 3,
                "cash": 12345.0,
                "positions": {},
                "open_orders": [],
                "order_history": []
            }"#,
        )
        .expect("write legacy state");

        let mut config = QuantConfig::default();
        config.paper.state_path = Some(state_path.clone());
        let runtime = config.build_runtime().expect("load legacy state");
        let portfolio = runtime.portfolio(None);

        assert_eq!(portfolio.account.cash, 12_345.0);
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn unsupported_paper_state_version_is_rejected() {
        let state_path = std::env::temp_dir().join(format!(
            "neenee-quant-paper-state-{}-future.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);
        std::fs::write(
            &state_path,
            r#"{
                "version": 999,
                "account": {
                    "next_decision_id": 0,
                    "next_order_id": 0,
                    "cash": 100000.0,
                    "positions": {},
                    "open_orders": [],
                    "order_history": []
                }
            }"#,
        )
        .expect("write future state");

        let mut config = QuantConfig::default();
        config.paper.state_path = Some(state_path.clone());
        let err = match config.build_runtime() {
            Ok(_) => panic!("future state should be rejected"),
            Err(err) => err,
        };

        assert!(err.contains("unsupported paper state version"));
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn config_rejects_unknown_market_data_source() {
        let mut config = QuantConfig::default();
        config.market_data.source = "unknown".to_string();

        let err = match config.build_runtime() {
            Ok(_) => panic!("unknown source should fail"),
            Err(err) => err,
        };

        assert!(err.contains("unknown quant market data source"));
    }

    #[test]
    fn config_rejects_invalid_starting_cash() {
        let mut config = QuantConfig::default();
        config.paper.starting_cash = 0.0;

        let err = match config.build_runtime() {
            Ok(_) => panic!("invalid cash should fail"),
            Err(err) => err,
        };

        assert!(err.contains("starting_cash"));
    }

    #[test]
    fn config_rejects_invalid_commission() {
        let mut config = QuantConfig::default();
        config.paper.commission_bps = -1.0;

        let err = match config.build_runtime() {
            Ok(_) => panic!("invalid commission should fail"),
            Err(err) => err,
        };

        assert!(err.contains("commission_bps"));
    }
}

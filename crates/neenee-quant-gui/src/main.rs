use iris::{Align, Application, Color, Config, Frame, LayoutOpts, PaintCanvas, TextBuf};
use neenee_quant_gui::{AppState, TradingMode, View};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut state = GuiState::new(AppState::from_environment()?);
    let cfg = Config::new("neenee quant")?.size(1180, 760);
    Application::run(
        cfg,
        move |frame, _input| {
            build_ui(frame, &mut state);
        },
        None::<fn(PaintCanvas)>,
    )?;
    Ok(())
}

struct GuiState {
    app: AppState,
    symbol: TextBuf,
    interval: TextBuf,
    strategy: TextBuf,
    start: TextBuf,
    end: TextBuf,
    capital: TextBuf,
    quantity: TextBuf,
    price: TextBuf,
    order_id: TextBuf,
}

impl GuiState {
    fn new(app: AppState) -> Self {
        Self {
            symbol: TextBuf::new(32, &app.symbol),
            interval: TextBuf::new(16, &app.interval),
            strategy: TextBuf::new(96, &app.strategy),
            start: TextBuf::new(16, &app.start),
            end: TextBuf::new(16, &app.end),
            capital: TextBuf::new(24, &app.capital),
            quantity: TextBuf::new(24, &app.quantity),
            price: TextBuf::new(24, &app.price),
            order_id: TextBuf::new(32, &app.order_id),
            app,
        }
    }

    fn sync_inputs(&mut self) {
        self.app.symbol = self.symbol.as_str().trim().to_string();
        self.app.interval = self.interval.as_str().trim().to_string();
        self.app.strategy = self.strategy.as_str().trim().to_string();
        self.app.start = self.start.as_str().trim().to_string();
        self.app.end = self.end.as_str().trim().to_string();
        self.app.capital = self.capital.as_str().trim().to_string();
        self.app.quantity = self.quantity.as_str().trim().to_string();
        self.app.price = self.price.as_str().trim().to_string();
        self.app.order_id = self.order_id.as_str().trim().to_string();
    }
}

fn build_ui(frame: &mut Frame, state: &mut GuiState) {
    let theme = frame.theme();
    let panel = panel_color(theme.is_dark());
    frame.column_ex(
        &LayoutOpts {
            flex: 1.0,
            gap: 10.0,
            pad: 12.0,
            bg: theme.bg(),
            ..LayoutOpts::default()
        },
        |frame| {
            top_bar(frame, &mut state.app);
            frame.row_ex(
                &LayoutOpts {
                    flex: 1.0,
                    gap: 10.0,
                    cross: Align::Stretch,
                    ..LayoutOpts::default()
                },
                |frame| {
                    sidebar(frame, &mut state.app, panel);
                    workspace(frame, state, panel);
                    inspector(frame, &state.app, panel);
                },
            );
        },
    );
}

fn top_bar(frame: &mut Frame, state: &mut AppState) {
    frame.row_ex(
        &LayoutOpts {
            gap: 10.0,
            cross: Align::Center,
            ..LayoutOpts::default()
        },
        |frame| {
            frame.title("neenee quant");
            frame.flex(1.0);
            frame.label(&state.risk_status);
            frame.label(&format!("Mode: {}", state.mode.label()));
            if frame.button(match state.mode {
                TradingMode::Paper => "Arm trading",
                TradingMode::TradingArmed => "Return to paper",
            }) {
                state.toggle_mode();
            }
        },
    );
}

fn sidebar(frame: &mut Frame, state: &mut AppState, panel: Color) {
    frame.size_next(190.0, 0.0);
    frame.column_ex(
        &LayoutOpts {
            flex: 0.0,
            gap: 6.0,
            pad: 8.0,
            bg: panel,
            radius: 6.0,
            ..LayoutOpts::default()
        },
        |frame| {
            frame.label_sized("Workspace", 13.0);
            nav_item(frame, state, View::Market);
            nav_item(frame, state, View::Backtest);
            nav_item(frame, state, View::Portfolio);
            nav_item(frame, state, View::Orders);
            nav_item(frame, state, View::Config);
            frame.separator();
            frame.label_sized("Runtime", 13.0);
            frame.label(&format!("Market data: {}", state.market_data_source));
            frame.label(&format!("Config: {}", state.config_summary));
            frame.label("Tools: market_data, backtest, list_positions, place_order, cancel_order");
            frame.label("Broker: paper runtime with risk limits");
        },
    );
}

fn nav_item(frame: &mut Frame, state: &mut AppState, view: View) {
    if frame.selectable(view.label(), state.view == view) {
        state.set_view(view);
    }
}

fn workspace(frame: &mut Frame, state: &mut GuiState, panel: Color) {
    frame.column_ex(
        &LayoutOpts {
            flex: 1.0,
            gap: 10.0,
            pad: 12.0,
            bg: panel,
            radius: 6.0,
            ..LayoutOpts::default()
        },
        |frame| match state.app.view {
            View::Market => market_view(frame, state),
            View::Backtest => backtest_view(frame, state),
            View::Portfolio => portfolio_view(frame, state),
            View::Orders => orders_view(frame, state),
            View::Config => config_view(frame, state),
        },
    );
}

fn market_view(frame: &mut Frame, state: &mut GuiState) {
    frame.title("Market");
    frame.row_ex(&form_row(), |frame| {
        frame.textfield("Symbol", &mut state.symbol);
        frame.dropdown(
            "Kind",
            &mut state.app.market_kind,
            &["quote", "klines", "depth"],
        );
        frame.textfield("Interval", &mut state.interval);
    });
    if frame.button("Fetch market data") {
        state.sync_inputs();
        state.app.fetch_market_data();
    }
}

fn backtest_view(frame: &mut Frame, state: &mut GuiState) {
    frame.title("Backtest");
    frame.textfield("Strategy", &mut state.strategy);
    frame.row_ex(&form_row(), |frame| {
        frame.textfield("Symbol", &mut state.symbol);
        frame.textfield("Interval", &mut state.interval);
        frame.textfield("Start", &mut state.start);
        frame.textfield("End", &mut state.end);
    });
    frame.textfield("Initial capital", &mut state.capital);
    if frame.button("Run backtest") {
        state.sync_inputs();
        state.app.run_backtest();
    }
}

fn portfolio_view(frame: &mut Frame, state: &mut GuiState) {
    frame.title("Portfolio");
    frame.label(&state.app.account_summary);
    frame.label(&state.app.positions_summary);
    frame.label(&state.app.open_orders_summary);
    frame.separator();
    frame.textfield("Symbol filter", &mut state.symbol);
    if frame.button("Refresh positions") {
        state.sync_inputs();
        state.app.refresh_positions();
    }
}

fn orders_view(frame: &mut Frame, state: &mut GuiState) {
    frame.title("Orders");
    frame.label("Account-mutating order submission is blocked until the workspace is armed.");
    frame.label(&state.app.recent_order_summary);
    frame.label(&state.app.open_orders_summary);
    frame.separator();
    frame.row_ex(&form_row(), |frame| {
        frame.textfield("Symbol", &mut state.symbol);
        frame.dropdown("Side", &mut state.app.order_side, &["buy", "sell"]);
        frame.dropdown("Type", &mut state.app.order_type, &["market", "limit"]);
    });
    frame.row_ex(&form_row(), |frame| {
        frame.textfield("Quantity", &mut state.quantity);
        frame.textfield("Limit price", &mut state.price);
    });
    if frame.button("Submit order") {
        state.sync_inputs();
        state.app.submit_order();
        state.order_id.set(&state.app.order_id);
    }
    frame.separator();
    frame.row_ex(&form_row(), |frame| {
        frame.textfield("Order id", &mut state.order_id);
    });
    if frame.button("Cancel order") {
        state.sync_inputs();
        state.app.cancel_order();
    }
}

fn config_view(frame: &mut Frame, state: &mut GuiState) {
    let config = &state.app.config;
    frame.title("Config");
    frame.label(&format!(
        "Market data source: {}",
        state.app.market_data_source
    ));
    frame.label(&format!(
        "Binance base URL: {}",
        config.market_data.binance_base_url
    ));
    frame.separator();
    frame.label("Paper runtime");
    frame.label(&format!("Starting cash: {}", config.paper.starting_cash));
    frame.label(&format!("Commission bps: {}", config.paper.commission_bps));
    frame.label(&format!(
        "State path: {}",
        config
            .paper
            .state_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "memory".to_string())
    ));
    frame.label(&format!(
        "Audit log: {}",
        config
            .paper
            .audit_log
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "disabled".to_string())
    ));
    frame.label(&format!(
        "Max order notional: {}",
        config.paper.risk.max_order_notional
    ));
    frame.label(&format!(
        "Max gross exposure: {}",
        config.paper.risk.max_gross_exposure
    ));
    frame.label(&format!(
        "Allow short selling: {}",
        config.paper.risk.allow_short_selling
    ));
    frame.separator();
    frame.label(&state.app.config_summary);
}

fn inspector(frame: &mut Frame, state: &AppState, panel: Color) {
    frame.size_next(360.0, 0.0);
    frame.column_ex(
        &LayoutOpts {
            flex: 0.0,
            gap: 8.0,
            pad: 10.0,
            bg: panel,
            radius: 6.0,
            ..LayoutOpts::default()
        },
        |frame| {
            frame.title("Result");
            frame.label(&format!("Last action: {}", state.last_action));
            frame.label(&state.account_summary);
            frame.label(&state.open_orders_summary);
            frame.separator();
            frame.scroll("result-scroll", |frame| {
                for line in state.last_result.lines() {
                    frame.label(line);
                }
            });
        },
    );
}

fn form_row() -> LayoutOpts {
    LayoutOpts {
        gap: 8.0,
        cross: Align::Center,
        ..LayoutOpts::default()
    }
}

fn panel_color(dark: bool) -> Color {
    if dark {
        Color::rgba(24, 27, 32, 245)
    } else {
        Color::rgba(246, 248, 250, 245)
    }
}

# neenee-quant-gui

GUI frontend for the `neenee-quant` quantitative-trading application.

A thin presentation crate that renders the quant-trading views (backtesting,
order placement, market data) and forwards actions to the `neenee-quant`
application layer. It depends on `neenee-core` for the tool/capability types and
on `neenee-quant` for the trading domain.

See [`neenee-quant`](../neenee-quant) for the application layer and layering.

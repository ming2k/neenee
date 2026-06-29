# neenee-quant

Quantitative-trading application for neenee.

An **application-layer** crate, a peer of `neenee-code`: it depends on
`neenee-agent` (so it reuses the full turn/round loop, pursuits, permission
broker) and layers on quantitative-trading domain tools — market data,
backtesting, order placement — plus a GUI.

## Layering

```text
neenee-core (domain) + neenee-store (persistence)
        ^
        |
neenee-providers (LLM) + neenee-tools (generic tools)
        ^
        |
neenee-agent (orchestration)
        ^
        |
neenee-quant ── adds quant domain tools & the GUI (`neenee-quant-gui`)
```

It is a sibling application crate alongside `neenee-code` (the coding agent),
sharing the core/store/agent foundation but adding trading-specific domain
tools and a GUI frontend.

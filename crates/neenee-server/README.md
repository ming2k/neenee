# neenee-server

The session/transport layer between the orchestration crate (`neenee-agent`)
and the frontends (`neenee-code` TUI today, a browser frontend tomorrow).

## Why this crate exists

Historically `neenee-code` was a single process: one TUI driving one agent
background task over a pair of `mpsc` channels. When the TUI process and the
agent process were split apart, this crate became the bridge — it owns the
long-lived agent session, multiplexes requests/responses, and exposes a stable
transport so any frontend can attach.

It depends on `neenee-agent` for the orchestration loop and on `neenee-store`
for durable state; frontends depend only on the transport surface exposed here.

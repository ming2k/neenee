//! Per-concern handlers for the agent background task's [`AgentRequest`]
//! dispatch loop. Each handler is one match arm (or, for `SlashCommand`, one
//! arm's worth of sub-dispatch), extracted verbatim from `agent_loop::run`'s
//! inline `match`.
//!
//! Handlers take only the references they need, with names matching the
//! original loop locals (`config`, `agent`, `session`, `resp_tx`, …) so the
//! transplanted bodies read exactly as they did inline. This keeps the
//! extraction pure code motion — no identifier rewrites inside the bodies.
//!
//! The dispatcher in `agent_loop::run` routes each request variant to its
//! handler here.

pub(crate) mod chat;
pub(crate) mod permission;
pub(crate) mod provider;
pub(crate) mod session;
pub(crate) mod slash;

//! Agent-context helpers extracted from `main.rs`: resolving the active
//! model's context window and re-seeding the mid-turn prune threshold after a
//! provider/model switch. Pure reads of the live [`Agent`] + [`Config`].

use neenee_agent::Agent;
use neenee_core::resolve_model;
use neenee_store::config::Config;

/// Resolve the active model's context window (tokens) from the live provider.
/// `0` means unknown (a user-defined or local model not in the registry); the
/// compaction policy substitutes a conservative fallback at resolve time.
pub fn active_context_window(agent: &Agent) -> usize {
    resolve_model(&agent.provider.model()).context_window
}

/// Re-seed the mid-turn prune threshold from the active model's context window.
/// Called at startup and after every provider/model switch so mid-turn relief
/// tracks the live model instead of a frozen, model-agnostic budget. A no-op
/// when pruning is disabled (no gate is installed in that case).
pub fn reseed_prune_threshold(agent: &Agent, config: &Config) {
    if !config.compaction_prune {
        return;
    }
    let window = active_context_window(agent);
    agent.set_context_prune_threshold(config.compaction.resolve(window).prune_threshold_tokens);
}

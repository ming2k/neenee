# Session Persistence

A neenee session is a recoverable work site, not a transcript-shaped cache
for the next model request. The local session has to preserve enough state to
resume the work exactly where it stopped: what the user saw, what the model saw,
which tool calls ran, which results returned, which context was projected out of
the model window, and which loop-level obligations were still active.

This page explains that durable model. For the request-scoped view sent to a
provider, see [Model context](model-context.md). For the storage location model,
see [Persistence and the XDG layout](../persistence.md).

## Durable Scene

The **durable session** is the local source of truth for one coding session. It
contains both the visible conversation and the hidden machinery needed to make
that conversation resumable:

- the model window that future provider requests start from,
- the archived transcript that has been projected out of the model window,
- user-visible assistant and tool events,
- hidden harness messages such as pursuit continuation and compaction
  checkpoints,
- task and pursuit state,
- the latest context-projection metadata,
- the local blobs needed to reconstruct large tool results and attachments.

That split is deliberate. The model window is what the provider can still read.
The archived transcript is what the user and the session store can still
recover. A message can leave the model window without leaving the durable
session.

## Event Log and Snapshot

The event log is the durable history. Each meaningful session mutation is
recorded as an event, so replay can rebuild the session even if the process
stopped between turns. A snapshot exists as an acceleration layer: it gives the
loader a compact current image, but it must describe the same state the event
history would produce.

This matters for context projection. Pruning and compaction are not ordinary
message edits. They archive the original messages and replace the model window
in one atomic session mutation. On replay, that mutation must not be expanded
into "archive these messages" plus "replace the model window" plus another
projection record, because that would duplicate the archive. The event and the
snapshot describe one operation: original context was retained durably, while a
smaller projection became model-visible.

## Admission and Commit

Durability starts before the provider call. When a user submits a round, the
session admits the user message first. Only after that does the agent call the
provider, run tools, or ask for permissions. If the process stops after
admission, resume can see that the user request existed and continue from a
well-defined point rather than losing the prompt.

During a round, tool turns are also committed back to the session. Assistant
tool-call messages and matching tool-result messages enter the model window
after the tool work completes. A late commit is rejected if the user switched
sessions while the round was running, so work from an older branch cannot land in
the newly selected one.

The invariant is simple: the durable session should never require guessing
which side effects already happened. If a tool result is present, the tool call
has completed. If it is absent, the resumed round can reason from the persisted
state instead of replaying an unsafe half-known action.

## Context Projection

Context pressure changes what the model sees, but it must not erase the
recoverable scene. neenee uses **model-context projection** for that boundary:

- pruning clears stale tool-result bodies while preserving the tool-call chain,
- compaction replaces older complete turns with a checkpoint summary,
- both operations retain the originals in the archived transcript,
- both operations record which projection operation happened.

The operation tag matters for resume and audit. A resumed session should know
whether the last projection was a prune, a compaction, or a legacy record whose
operation was not known. That prevents the next run from treating the session as
if projection details vanished and pressure relief had to be rediscovered from
scratch.

For the two projection layers, see [Context pruning](context-pruning.md) and
[Context compaction](context-compaction.md). For the naming decision, see
[ADR-0040](../../adr/0040-session-state-and-context-projection.md).

## Resume

Resume starts from the durable session, not from provider memory. Providers are
stateless; they do not remember previous requests. neenee therefore restores the
local session first, then sends the restored model window on the next provider
request.

The resume path restores the visible transcript, the model window, the archived
transcript, hidden harness context, projection metadata, task state, pursuit
state, and any blobs that are still referenced by messages. Once this state is
loaded, the next model request uses the restored model window as its live
conversation history.

The archived transcript is not blindly reinserted into the provider request.
Doing so would undo pruning or compaction and push the session back into the
same pressure state. Instead, archived originals remain available for recovery,
audit, and future tooling, while the model reads the checkpointed or pruned
projection that was committed before the restart.

## Correct Recovery Contract

A session is correctly resumable when these conditions hold:

- the model window after resume matches the last committed projection,
- the archived transcript still contains originals removed by pruning or
  compaction,
- tool-call ids still pair assistant calls with tool results,
- hidden harness messages keep their provenance,
- unfinished pursuit or task state is restored before the next round,
- context-projection metadata says what operation produced the current window.

If those conditions hold, a resumed session does not need to re-prune or
re-compact just to rediscover the state it already had. It may project again
later if new messages create new pressure, but the prior projection remains a
durable fact.


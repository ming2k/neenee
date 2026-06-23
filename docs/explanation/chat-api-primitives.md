# Chat API primitives

neenee's agent is not an arbitrary program that happens to call a language
model. Its shape — the role split, the tool loop, the compaction backstop, the
durable pursuit — is a direct consequence of the chat completion API contract.
This page names the three protocol primitives that drive that shape, so the
rest of the agent design reads as their consequence rather than a series of
independent choices.

For the byte-level wire shape, see [Request flow](request-flow.md). For the
round trip of a single tool call, see [Tool rounds](agent-design/tool-rounds.md).
For why providers differ on which primitives they implement, see
[Provider capabilities](provider-capabilities.md).

## Roles carry trained authority

A chat request is a sequence of messages, each tagged with a role. The four
roles — `system`, `user`, `assistant`, `tool` — are protocol, not a neenee
invention: every OpenAI-compatible endpoint speaks them.

The model is trained (through supervised fine-tuning and RLHF) to treat each
role differently, which produces a practical authority gradient rather than a
hard guarantee. There is no numeric weight attached to a role; the gradient is
a property of the training distribution:

- `system` is treated as durable framing — persona, global rules, the highest
  authority, hard to override in a single turn.
- `user` is treated as a request to respond to or act on.
- `assistant` is the model's own prior output, retained as memory but
  superseded by newer information.
- `tool` is raw data returned by a tool call, taken as fact.

neenee exploits this gradient to layer intent by durability:

| Placed in `system` | Placed in `user` |
|--------------------|------------------|
| Persona, tool catalog, the active **pursuit** as a persistent anchor | The real prompt, plus dynamic control prompts that should drive action |

The pursuit lives in the system prompt so the model treats it as a durable rule
rather than a one-off request it can consider finished. Loop control prompts
live in `user` so the model treats them as the current turn's work. Reversing
the two would fail: a pursuit in `user` reads as a request that is done after one
turn; a control prompt in `system` dilutes the authority a system message
carries. See [Pursuits](agent-design/pursuits.md).

There is one attribute orthogonal to role: **visibility**. A message can be
marked hidden, which means the model still receives it (it enters the request)
but the terminal UI does not render it. This opens a control channel that
drives the model as a `user` request without polluting the visible transcript
— used for autonomous-loop iteration prompts, stall-reflection nudges, and
skill injection. Visibility is a neenee concept; the API itself has no notion
of it.

## The API is stateless; the messages array is the only memory

Every round of the agent loop is one independent HTTP request. The provider
remembers nothing between requests. What the model "knows" about prior turns
is entirely a function of the messages array neenee re-sends each round:

```text
round 1:  [system, user]
round 2:  [system, user, assistant(tool_calls), tool(result)]
round 3:  [system, user, assistant, tool, assistant(tool_calls), tool, ...]
```

The array grows monotonically until the turn ends. This single fact forces
most of the harness machinery:

- **The array is bounded**, so long work needs
  [context compaction](agent-design/harness.md) to stay within the model's
  window — summarizing or pruning old messages so the array stays finite.
- **Cross-session intent cannot live in the array**, so a durable objective
  needs a [pursuit](agent-design/pursuits.md) persisted outside the request, then
  re-injected into the system prompt each round.
- **A tool's effect is invisible to the model unless it re-enters the array**,
  so every tool result is appended as a `tool` message before the next
  request. The model cannot "remember" running a command; it can only read the
  result message neenee sends back.

The agent is therefore a stateless API plus client-managed external state —
the messages array, the persisted pursuit and session, the in-memory checklist.
Nothing else carries across a round.

## Function calling is the ReAct loop, not an implementation detail

A tool-using turn is not a neenee loop grafted onto a chat API. The loop *is*
the protocol. Function calling — the `tools` array, the assistant's
`tool_calls`, and the `tool` message that carries a `tool_call_id` back — is
part of the OpenAI contract, and one round of the agent loop maps onto it
exactly:

```text
client sends: messages + tools
model replies: assistant message carrying tool_calls
client executes the tools locally
client appends: tool messages (one per call, each with tool_call_id)
client re-sends: messages + tools   ← next round
```

The loop ends when the model replies with no `tool_calls` — and that is also
protocol, not policy. neenee adds guards on top (a repeated-call limit, a
read-only stall detector), but the loop's existence and termination condition
come from the contract. See
[ADR-0009](../adr/0009-uncapped-agentic-loop.md) for why the round count was
left uncapped to match this.

Two protocol constraints shape the harness:

- **Tool results must pair with their call.** A `tool` message with no
  matching preceding `tool_call_id` is rejected by the endpoint, so neenee
  filters orphan results before sending. This pairing is also what makes
  retry unsafe once any tool has run: replaying the request would replay side
  effects. See [Request flow](request-flow.md).
- **Native function calling is a capability, not a given.** Providers without
  it never emit `tool_calls`; the model is instructed to emit the call as
  plain assistant text, which neenee parses back into a synthetic
  `tool_calls` / `tool_call_id` pair. This is the same loop over a degraded
  transport, not a different mechanism. See
  [Provider capabilities](provider-capabilities.md).

## Why this matters

Read together, the three primitives explain design choices that otherwise look
arbitrary:

- The **pursuit sits in the system prompt** because of the role authority
  gradient — it needs to read as durable framing, not a request.
- **Compaction exists** because the API is stateless and the messages array
  is the only memory, and it is bounded.
- **The tool loop looks the way it does** because function calling is the
  protocol's own agent primitive; neenee wraps it, it did not invent it.
- **The hidden control channel exists** because the `user` role drives action,
  and visibility is separable from role.

The rest of this section — harness, turns and rounds, pursuits, tool rounds — is
each a specialization of one or more of these primitives.

## See also

- [Request flow](request-flow.md) — the wire-level shape of each transaction
- [Tool rounds](agent-design/tool-rounds.md) — the round trip of one tool call
- [Provider capabilities](provider-capabilities.md) — which providers implement
  which primitives
- [Pursuits](agent-design/pursuits.md) — the pursuit as a system-prompt anchor
- [Harness architecture](agent-design/harness.md) — compaction and the
  stateless-memory consequence
- [ADR-0009](../adr/0009-uncapped-agentic-loop.md) — the uncapped loop as a
  consequence of protocol-driven termination

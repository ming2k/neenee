# Skills

Skills are on-demand domain expertise. A skill is a Markdown document with a
small YAML header; when the agent needs that expertise, the document body is
injected into the conversation so the model can act on it. Skills are *not*
tools — they carry no executable code. They steer the model by adding
instructions, and the model then uses ordinary tools (bash, edit, ...) to do
the work those instructions describe.

This page covers where skills come from, how they are ordered, and how a skill
body reaches the model. For the lookup-oriented file layout, see
[Paths](../../reference/paths.md); for the `use_skill` tool contract, see
[Skills and `use_skill`](../../reference/tools/skills.md).

## The two-channel model

A skill crosses the model's context boundary through two independent channels.
Keeping them separate is the central design idea:

| Channel | What it carries | Where it lands | When |
|---------|-----------------|----------------|------|
| **Catalog** | Each enabled skill's name and one-line description | The system prompt | Every turn, rebuilt from the live registry |
| **Body** | The full Markdown expertise document | A tool result, or a hidden user message | On demand only |

The catalog is cheap and always present: it tells the model what expertise
exists without paying for the full text. The body is expensive and loaded only
when a skill is actually relevant. This is why a skills index can list dozens of
skills in the system prompt at near-zero cost, while their full bodies never
enter context until invoked.

Each turn the harness rebuilds the system message from the live mode, pursuit,
tool list, and skills catalog. The catalog is the only skills-related content
that lives in the system prompt; everything else is delivered as a turn-scoped
message.

## Sources and priority

Skills are discovered from several sources. Each source is labelled with a
**scope**, and scopes are ordered: a higher-priority scope overrides a
lower-priority scope when two skills share a name.

| Scope | Source | Priority |
|-------|--------|----------|
| **System** | Bundled skills, compile-time-embedded into the binary (never on disk) | Lowest |
| **Remote** | Skill repositories fetched from `[skills] urls` and cached locally | |
| **User** | User-global skills: the XDG data dir, plus external application conventions (`~/.agents/skills/`, `~/.claude/skills/`) | |
| **Extra** | Extra paths configured under `[skills] paths` | |
| **Repo** | Project-local skills in the project working tree (`.neenee/skills/`, `.agents/skills/`, `.claude/skills/`) | Highest |

The intent of the cascade is that the most specific source wins: a skill
checked into a project overrides a user-global skill with the same name, which
in turn overrides a bundled one. Bundled skills sit at the bottom so that
anything a user or project defines always takes precedence over what ships with
neenee.

Two design notes worth calling out:

- **Bundled skills are embedded, not installed.** They are baked into the
  binary at build time and have no on-disk location. This avoids writing
  read-only shipped data into a user-writable tree and needs no install or sync
  step. See [ADR-0013](../../adr/0013-skills-xdg-paths-and-bundled-embed.md).
- **External directories are read-only.** `~/.agents/skills/` and
  `~/.claude/skills/` (and their project-local `.agents/skills/`,
  `.claude/skills/` counterparts) are other tools' conventions. neenee reads
  them so a shared skill library works across agents, but it never writes to
  them.

All user-level paths resolve through the central `Dirs` layer and honour the
standard XDG overrides. See [Persistence and the XDG
layout](../persistence.md) for the override stack.

## The skill format

A skill is a `SKILL.md` file inside its own directory (so it can carry
auxiliary files the body references). The YAML frontmatter declares who the
skill is and how it behaves; the Markdown body is the expertise itself.

The meaningful frontmatter fields:

| Field | Purpose |
|-------|---------|
| `name` | Identity used for invocation and override. If omitted, the parent directory name is used. |
| `description` | One line shown in the catalog and used to decide relevance. |
| `short-description` | Fallback for the catalog when `description` is empty. |
| `policy.allow_implicit_invocation` | Whether the skill may auto-load when its name is mentioned (default true). |
| `dependencies` | Tools the skill expects to be available (e.g. an MCP server). Declarative; not yet enforced. |
| `tags`, `version` | Metadata. |

A skill with no frontmatter is still valid: its body becomes the whole file and
its name is derived from the directory.

## How a skill is invoked

There are two paths from "the catalog mentions a skill" to "the body is in
context", and they differ only in the message shape they produce:

1. **Explicit — `use_skill`.** The model calls the `use_skill` tool with a
   skill name. The tool looks up the skill, returns its body as a tool result,
   and also lists the auxiliary files in the skill directory. `use_skill` is an
   ordinary read-only tool, architecturally identical to `read_file` or `bash`;
   its only specialty is that its result happens to be a skill body. This works
   even for disabled skills, so the model can load one and explain why it did
   nothing.

2. **Implicit — mention detection.** Before a turn runs, the harness scans the
   latest visible user message for skill mentions. A mention is one of:
   - an `@skill-name` reference,
   - a `skill://skill-name` URI,
   - the plain skill name as a standalone token (exact match; substrings do not
     count, so a skill named `rust` is not triggered by `rust-expert`).

   Each mentioned skill whose policy allows implicit invocation is loaded as a
   **hidden user message** carrying the same `[Skill '<name>' loaded]` marker
   the explicit path uses. Hidden means it steers the model but is not rendered
   as part of the visible transcript. Already-loaded skills are not re-injected.

Because both paths emit the same marker, implicit loading de-duplicates against
explicit loading: if the model already called `use_skill('foo')`, mentioning
`foo` later in the same turn does not inject it a second time.

## Policy and enabled state

Two flags govern visibility:

- **`enabled`** (default true). A disabled skill is dropped from the catalog
  and is never auto-loaded on mention. It can still be requested explicitly via
  `use_skill`, which lets the model surface it and explain why it is inactive.
  Skills can be disabled through configuration (`[skills] disable`).
- **`allow_implicit_invocation`** (default true). When false, the skill appears
  in the catalog and responds to `use_skill`, but mention detection skips it.
  Use this for skills that should only be loaded deliberately.

A skill participates in implicit invocation only when it is both enabled and
allows it.

## Reloading

The `reload_skills` tool rescans every source — local directories and remote
repositories — and rebuilds the registry in place. It is the way to pick up
newly added, removed, or edited skill files without restarting neenee.

## Decision history

- [ADR-0013](../../adr/0013-skills-xdg-paths-and-bundled-embed.md) — XDG paths
  for user skills/commands and compile-time-embedded bundled skills.
- [ADR-0014](../../adr/0014-xdg-persistence-architecture.md) — the unified XDG
  persistence architecture that all skill paths resolve through.

## Adjacent layers

Skills are an **extension surface** of the harness, alongside MCP servers (which
add tools, not instructions). The harness refreshes the skills catalog when it
rebuilds the system prompt each turn; see [Harness
architecture](harness.md). Skill invocation is a special case of a tool round,
so [Tool rounds](tool-rounds.md) describes the execution path an explicit
`use_skill` call takes.

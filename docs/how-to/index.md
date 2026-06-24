# How-to guides

Task-oriented guides for extending neenee. Each guide assumes familiarity
with the relevant reference material.

| Guide | Task |
|-------|------|
| [How to plan a change before implementing](plan-a-change.md) | Delegate planning to a read-only `PLAN` subagent and get approval before editing |
| [How to add a built-in tool](add-a-tool.md) | Implement the `Tool` trait, pick a `ToolAccess`, register, verify |
| [How to add a provider](add-a-provider.md) | Wrap `OpenAiCompatProvider` or build a standalone adapter, register dispatch sites |
| [How to ask the user a question during a task](ask-the-user.md) | Use `ask_user` to resolve ambiguity or collect preferences mid-task |

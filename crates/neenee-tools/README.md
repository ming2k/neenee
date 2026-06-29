# neenee-tools

Built-in tools for the coding-agent stack: filesystem, shell, web, and
ask-user.

Each tool lives in its own module and self-registers via
`neenee_core::register_tool!` (collected by `inventory` at link time). The
binary assembles concrete instances from the registry at startup; this crate
does not enumerate them itself. Shared helpers live in `helpers`, and pluggable
web-search backends in `search`.

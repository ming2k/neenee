//! Distributed tool registration.
//!
//! Built-in tools self-register at their definition site via the
//! [`register_tool!`](crate::register_tool!) macro instead of being enumerated
//! by hand at the agent's assembly point. Submissions are collected at runtime
//! by the [`inventory`] crate, so adding a tool is a one-line change in its own
//! module — no central list to keep in sync.
//!
//! Tools that need runtime state (config blobs, shared registries) pull it out
//! of an opaque [`ToolContext`]: a type-keyed service map. This keeps
//! `neenee-core` free of dependencies on the concrete state types, which live
//! in higher crates (e.g. `SkillRegistry` in `neenee-agent`). The map is the
//! only seam.
//!
//! A handful of "meta" tools genuinely cannot self-register — e.g. a sub-agent
//! dispatch tool that needs a snapshot of the *rest* of the toolset, which is
//! the registry's own output. Those are still assembled explicitly where the
//! dependency is created; the registry collects everything that is
//! self-contained.

use crate::Tool;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// An opaque, type-keyed bag of services handed to each [`ToolFactory`] at
/// build time. Tools look their concrete dependencies up by Rust type. There
/// is intentionally no typed list of fields: that would force `neenee-core` to
/// depend on every higher crate's state types. The map is the seam.
///
/// Cheaply [`Clone`](self::ToolContext#method.clone)-able (shared via `Arc`);
/// build one with [`ToolContextBuilder`] and freeze it.
#[derive(Clone, Default)]
pub struct ToolContext {
    services: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl ToolContext {
    /// Look up a value by its exact type. `None` if no service of type `T` was
    /// provided.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.services
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Look up a shared handle stored as `Arc<T>` and clone the `Arc` out.
    /// Shorthand for `self.get::<Arc<T>>().cloned()`.
    pub fn shared<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.get::<Arc<T>>().cloned()
    }
}

/// Builder for [`ToolContext`]. Provide services by concrete type, then
/// [`build`](Self::build) to freeze an immutable, cheaply cloneable context.
#[derive(Default)]
pub struct ToolContextBuilder {
    services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ToolContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide a service by its concrete type. Later inserts of the same type
    /// replace earlier ones.
    pub fn provide<T: Any + Send + Sync>(&mut self, value: T) -> &mut Self {
        self.services.insert(TypeId::of::<T>(), Arc::new(value));
        self
    }

    /// Freeze the context.
    pub fn build(self) -> ToolContext {
        ToolContext {
            services: Arc::new(self.services),
        }
    }
}

/// Build step that turns a [`ToolContext`] into one tool instance. Each
/// concrete tool registers its own factory (a private unit struct) via
/// [`register_tool!`](crate::register_tool!). Returning `None` lets a factory
/// decline (e.g. a required service is absent or a feature is off); the tool
/// then simply does not appear in the assembled set.
pub trait ToolFactory: Send + Sync {
    fn build(&self, ctx: &ToolContext) -> Option<Arc<dyn Tool>>;
}

/// An entry in the global, compile-time tool registry. Collected by
/// [`inventory`]; never constructed by hand.
pub struct ToolRegistration {
    pub factory: &'static dyn ToolFactory,
}

inventory::collect!(ToolRegistration);

/// Collect every self-registered tool into a `Vec`. Factories that decline
/// (return `None`) are skipped. Duplicate tool names are dropped, keeping the
/// first registration — an accidental double-register then merely shadows
/// instead of breaking the agent.
pub fn collect_tools(ctx: &ToolContext) -> Vec<Arc<dyn Tool>> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut tools = Vec::new();
    for entry in inventory::iter::<ToolRegistration> {
        let Some(tool) = entry.factory.build(ctx) else {
            continue;
        };
        if seen.insert(tool.name().to_string()) {
            tools.push(tool);
        }
    }
    tools
}

/// Register a self-contained tool at its definition site.
///
/// Expands to a private factory unit struct implementing [`ToolFactory`] plus
/// an [`inventory`] submission.
///
/// Two forms:
///
/// - **Context-free** — for tools needing no runtime state. `$build` is any
///   expression evaluating to `T: Tool`:
///   ```ignore
///   neenee_core::register_tool!(BashFactory => BashTool);
///   ```
///
/// - **Context-aware** — `$ctx` (a name *you* choose) binds the build context,
///   which `$build` may then reference. Use `?` / early `return None` to
///   decline (the tool then won't appear in the assembled set):
///   ```ignore
///   neenee_core::register_tool!(WebFetchFactory => |ctx| {
///       let cfg = ctx.get::<neenee_core::WebSearchConfig>()?.clone();
///       WebFetchTool::with_config(cfg)
///   });
///   ```
///
/// The caller names the context parameter (`|ctx|`) on purpose: Rust macro
/// hygiene means a binding invented inside the macro can't be referenced from
/// the call-site `$build` tokens. Giving the caller the parameter name keeps
/// both in the same hygiene context.
#[macro_export]
macro_rules! register_tool {
    ($id:ident => |$ctx:ident| $build:expr) => {
        struct $id;
        impl $crate::ToolFactory for $id {
            fn build(
                &self,
                $ctx: &$crate::tool_registry::ToolContext,
            ) -> Option<std::sync::Arc<dyn $crate::Tool>> {
                let tool: std::sync::Arc<dyn $crate::Tool> = std::sync::Arc::new($build);
                Some(tool)
            }
        }
        ::inventory::submit!($crate::tool_registry::ToolRegistration { factory: &$id });
    };
    ($id:ident => $build:expr) => {
        struct $id;
        impl $crate::ToolFactory for $id {
            fn build(
                &self,
                _ctx: &$crate::tool_registry::ToolContext,
            ) -> Option<std::sync::Arc<dyn $crate::Tool>> {
                let tool: std::sync::Arc<dyn $crate::Tool> = std::sync::Arc::new($build);
                Some(tool)
            }
        }
        ::inventory::submit!($crate::tool_registry::ToolRegistration { factory: &$id });
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct PingTool;
    #[async_trait]
    impl Tool for PingTool {
        fn name(&self) -> &str {
            "registry_ping"
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("pong".to_string())
        }
    }

    struct DeclinedTool;
    #[async_trait]
    impl Tool for DeclinedTool {
        fn name(&self) -> &str {
            "registry_declined"
        }
        fn description(&self) -> &str {
            "declined test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("never".to_string())
        }
    }

    register_tool!(PingFactory => PingTool);
    // Declines when a (deliberately absent) marker service is missing.
    register_tool!(DeclinedFactory => |ctx| {
        let _ = ctx.get::<String>()?;
        DeclinedTool
    });

    #[test]
    fn collect_tools_includes_registered_and_skips_declined() {
        let ctx = ToolContextBuilder::new().build();
        let tools = collect_tools(&ctx);
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| name.starts_with("registry_"))
            .collect();
        assert!(names.contains(&"registry_ping"));
        assert!(!names.contains(&"registry_declined"));
    }

    #[test]
    fn context_provides_and_looks_up_by_type() {
        let mut builder = ToolContextBuilder::new();
        builder.provide(42_u32).provide(String::from("hi"));
        let ctx = builder.build();
        assert_eq!(ctx.get::<u32>(), Some(&42));
        assert_eq!(ctx.get::<String>().map(String::as_str), Some("hi"));
        assert_eq!(ctx.get::<bool>(), None);
    }

    #[test]
    fn shared_clones_an_arc_out() {
        let mut builder = ToolContextBuilder::new();
        builder.provide(Arc::new(7_i32));
        let ctx = builder.build();
        let handle: Arc<i32> = ctx.shared::<i32>().unwrap();
        assert_eq!(*handle, 7);
        assert!(ctx.shared::<u64>().is_none());
    }
}

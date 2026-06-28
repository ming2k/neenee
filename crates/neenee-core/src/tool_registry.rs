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
//! A handful of "meta" tools genuinely cannot self-register — e.g. an envoy
//! dispatch tool that needs a snapshot of the *rest* of the toolset, which is
//! the registry's own output. Those are still assembled explicitly where the
//! dependency is created; the registry collects everything that is
//! self-contained.

use crate::{Tool, VariantSelection, empty_variant_selection};
use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
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

/// Collect every self-registered tool into a [`ToolSet`], grouping the
/// registrations into capabilities (by [`Tool::name`]) and variants (by
/// [`Tool::variant`]). Factories that decline (return `None`) are skipped.
/// Duplicate `(name, variant)` pairs are dropped keeping the first registration
/// — an accidental double-register merely shadows instead of breaking the
/// agent.
pub fn collect_toolset(ctx: &ToolContext) -> ToolSet {
    let mut tools = Vec::new();
    for entry in inventory::iter::<ToolRegistration> {
        if let Some(tool) = entry.factory.build(ctx) {
            tools.push(tool);
        }
    }
    ToolSet::from_tools(tools)
}

/// One capability: a logical tool identity (its [`Tool::name`]) realized by one
/// or more variants keyed by [`Tool::variant`]. Exactly one variant is the
/// default — the one the model sees when no selection names this capability.
#[derive(Clone)]
pub struct Capability {
    name: String,
    default_variant: String,
    variants: BTreeMap<String, Arc<dyn Tool>>,
}

impl Capability {
    /// The capability name (== every variant's [`Tool::name`]).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The id of the default variant.
    pub fn default_variant(&self) -> &str {
        &self.default_variant
    }

    /// The variant for `id`, or the default variant when `id` is `None` or names
    /// a variant this capability does not have.
    pub fn variant_or_default(&self, id: Option<&str>) -> &Arc<dyn Tool> {
        id.and_then(|v| self.variants.get(v))
            .unwrap_or_else(|| &self.variants[&self.default_variant])
    }

    /// All variant ids of this capability, in sorted order.
    pub fn variant_ids(&self) -> impl Iterator<Item = &str> {
        self.variants.keys().map(String::as_str)
    }

    /// Recompute the default variant: the one literally named `"default"` if
    /// present, otherwise the lexicographically-smallest id (deterministic —
    /// `inventory` link order is not). `variants` is never empty here.
    fn recompute_default(&mut self) {
        const DEFAULT: &str = "default";
        self.default_variant = if self.variants.contains_key(DEFAULT) {
            DEFAULT.to_string()
        } else {
            // BTreeMap keys are sorted, so the first key is the smallest.
            self.variants
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| DEFAULT.to_string())
        };
    }
}

/// The full set of tool capabilities, each with its variants. This is the
/// single source of truth the agent resolves against: given a
/// [`VariantSelection`] (per-model and/or per-profile), [`ToolSet::resolve`]
/// yields exactly one variant per capability. Cheaply [`Clone`]-able (variants
/// are `Arc`).
#[derive(Clone, Default)]
pub struct ToolSet {
    capabilities: BTreeMap<String, Capability>,
}

impl ToolSet {
    /// Build a toolset from a flat list of tools. Tools are grouped by
    /// [`Tool::name`] into capabilities and by [`Tool::variant`] into variants;
    /// the first tool seen for a given `(name, variant)` wins.
    pub fn from_tools(tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        let mut set = ToolSet::default();
        for tool in tools {
            set.insert(tool);
        }
        set
    }

    /// Add one tool as a variant of its capability, creating the capability if
    /// new. A `(name, variant)` already present is left untouched (first wins).
    /// The capability's default variant is recomputed after the insert.
    pub fn insert(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let variant = tool.variant().to_string();
        let cap = self
            .capabilities
            .entry(name.clone())
            .or_insert_with(|| Capability {
                name,
                default_variant: variant.clone(),
                variants: BTreeMap::new(),
            });
        cap.variants.entry(variant).or_insert(tool);
        cap.recompute_default();
    }

    /// Resolve the toolset to exactly one variant per capability for the given
    /// selection: a capability listed in `selection` is realized by its named
    /// variant (falling back to the default if the named variant is absent);
    /// capabilities not listed use their default variant.
    pub fn resolve(&self, selection: &VariantSelection) -> Vec<Arc<dyn Tool>> {
        self.capabilities
            .values()
            .map(|cap| {
                cap.variant_or_default(selection.get(cap.name()).map(String::as_str))
                    .clone()
            })
            .collect()
    }

    /// The default view: one default variant per capability (empty selection).
    pub fn default_view(&self) -> Vec<Arc<dyn Tool>> {
        self.resolve(empty_variant_selection())
    }

    /// Look up a capability by name.
    pub fn variants_of(&self, name: &str) -> Option<&Capability> {
        self.capabilities.get(name)
    }

    /// All capability names, in sorted order.
    pub fn capability_names(&self) -> impl Iterator<Item = &str> {
        self.capabilities.keys().map(String::as_str)
    }

    /// Whether the set holds no capabilities.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Number of capabilities (not variants).
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }
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
    fn collect_toolset_includes_registered_and_skips_declined() {
        let ctx = ToolContextBuilder::new().build();
        let toolset = collect_toolset(&ctx);
        let names: Vec<&str> = toolset
            .capability_names()
            .filter(|name| name.starts_with("registry_"))
            .collect();
        assert!(names.contains(&"registry_ping"));
        assert!(!names.contains(&"registry_declined"));
    }

    /// Two variants of one capability: `name()` shared, `variant()` distinct.
    struct VariantTool {
        variant: &'static str,
        desc: &'static str,
    }
    #[async_trait]
    impl Tool for VariantTool {
        fn name(&self) -> &str {
            "cap"
        }
        fn variant(&self) -> &str {
            self.variant
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn toolset_with_variants() -> ToolSet {
        ToolSet::from_tools(vec![
            Arc::new(VariantTool {
                variant: "default",
                desc: "default impl",
            }) as Arc<dyn Tool>,
            Arc::new(VariantTool {
                variant: "terse",
                desc: "terse impl",
            }) as Arc<dyn Tool>,
        ])
    }

    #[test]
    fn from_tools_groups_variants_under_one_capability() {
        let toolset = toolset_with_variants();
        assert_eq!(toolset.len(), 1);
        let cap = toolset.variants_of("cap").unwrap();
        assert_eq!(cap.default_variant(), "default");
        let ids: Vec<&str> = cap.variant_ids().collect();
        assert_eq!(ids, vec!["default", "terse"]);
    }

    #[test]
    fn resolve_picks_selected_variant_else_default() {
        let toolset = toolset_with_variants();

        // No selection → default variant.
        let resolved = toolset.resolve(empty_variant_selection());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].description(), "default impl");

        // Selection names the terse variant → terse impl.
        let mut selection = VariantSelection::new();
        selection.insert("cap".to_string(), "terse".to_string());
        let resolved = toolset.resolve(&selection);
        assert_eq!(resolved[0].description(), "terse impl");

        // Selection names a missing variant → falls back to default.
        let mut selection = VariantSelection::new();
        selection.insert("cap".to_string(), "nope".to_string());
        let resolved = toolset.resolve(&selection);
        assert_eq!(resolved[0].description(), "default impl");
    }

    #[test]
    fn default_falls_back_to_smallest_id_when_no_default_named() {
        let toolset = ToolSet::from_tools(vec![
            Arc::new(VariantTool {
                variant: "zulu",
                desc: "z",
            }) as Arc<dyn Tool>,
            Arc::new(VariantTool {
                variant: "alpha",
                desc: "a",
            }) as Arc<dyn Tool>,
        ]);
        let cap = toolset.variants_of("cap").unwrap();
        assert_eq!(cap.default_variant(), "alpha");
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

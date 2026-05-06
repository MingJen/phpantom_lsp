//! Eloquent Builder-as-static forwarding.
//!
//! Laravel's `Model::__callStatic()` delegates static calls to
//! `static::query()`, which returns an Eloquent Builder.  This module
//! loads the Builder class, fully resolves it (including `@mixin`
//! `Query\Builder` members), and converts each public instance method
//! into a static virtual method on the model.
//!
//! Return type mapping:
//! - `static`, `$this`, `self` → `\Illuminate\Database\Eloquent\Builder<ConcreteModel>`
//!   (the chain continues on the builder, not the model).
//! - Template parameters (e.g. `TModel`) → the concrete model class name.
//!
//! Methods whose name starts with `__` (magic methods) are skipped.

use std::sync::Arc;

use crate::inheritance::apply_substitution_to_conditional;
use crate::php_type::PhpType;
use crate::types::{ClassInfo, ELOQUENT_COLLECTION_FQN, MethodInfo, Visibility};
use crate::virtual_members::ResolvedClassCache;

use super::ELOQUENT_BUILDER_FQN;

/// Replace `\Illuminate\Database\Eloquent\Collection` with a custom
/// collection class in a [`PhpType`], preserving generic parameters.
pub(super) fn replace_eloquent_collection_typed(ty: &PhpType, custom_collection: &str) -> PhpType {
    replace_collection_in_type(ty, custom_collection)
}

/// Recursively walk a `PhpType` tree and replace any `Generic` whose
/// base name is the Eloquent Collection FQN with `custom_collection`.
fn replace_collection_in_type(ty: &PhpType, custom_collection: &str) -> PhpType {
    match ty {
        PhpType::Generic(name, args) if name == ELOQUENT_COLLECTION_FQN => {
            let new_args = args
                .iter()
                .map(|a| replace_collection_in_type(a, custom_collection))
                .collect();
            PhpType::Generic(custom_collection.to_string(), new_args)
        }
        PhpType::Generic(name, args) => {
            let new_args = args
                .iter()
                .map(|a| replace_collection_in_type(a, custom_collection))
                .collect();
            PhpType::Generic(name.clone(), new_args)
        }
        PhpType::Union(members) => PhpType::Union(
            members
                .iter()
                .map(|m| replace_collection_in_type(m, custom_collection))
                .collect(),
        ),
        PhpType::Intersection(members) => PhpType::Intersection(
            members
                .iter()
                .map(|m| replace_collection_in_type(m, custom_collection))
                .collect(),
        ),
        PhpType::Nullable(inner) => PhpType::Nullable(Box::new(replace_collection_in_type(
            inner,
            custom_collection,
        ))),
        PhpType::Array(inner) => PhpType::Array(Box::new(replace_collection_in_type(
            inner,
            custom_collection,
        ))),
        // Named types, scalars, etc. — no collection to replace.
        other => other.clone(),
    }
}

/// Build static virtual methods by forwarding Eloquent Builder's public
/// instance methods onto the model class.
///
/// Laravel's `Model::__callStatic()` delegates static calls to
/// `static::query()`, which returns a `Builder<static>`.  This function
/// loads the Builder class, fully resolves it (including `@mixin`
/// `Query\Builder` members), and converts each public instance method
/// into a static virtual method on the model.
///
/// Return type mapping:
/// - `static`, `$this`, `self` → `\Illuminate\Database\Eloquent\Builder<ConcreteModel>`
///   (the chain continues on the builder, not the model).
/// - Template parameters (e.g. `TModel`) → the concrete model class name.
///
/// Methods whose name starts with `__` (magic methods) are skipped.
pub(super) fn build_builder_forwarded_methods(
    class: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
    cache: Option<&ResolvedClassCache>,
) -> Vec<MethodInfo> {
    let custom_builder = class.laravel().and_then(|l| l.custom_builder.as_ref());
    let builder_fqn = custom_builder
        .and_then(|t| t.base_name())
        .unwrap_or(ELOQUENT_BUILDER_FQN);

    // Load the Eloquent Builder class (or custom builder).
    let builder_class = match class_loader(builder_fqn) {
        Some(c) => c,
        // Fallback to standard builder if custom builder fails to load.
        None if builder_fqn != ELOQUENT_BUILDER_FQN => match class_loader(ELOQUENT_BUILDER_FQN) {
            Some(c) => c,
            None => return Vec::new(),
        },
        None => return Vec::new(),
    };

    // Fully resolve Builder (own + traits + parents + virtual members
    // including @mixin Query\Builder).  This is safe because Builder
    // does not extend Model, so the LaravelModelProvider will not
    // recurse.
    let resolved_builder = crate::virtual_members::resolve_class_fully_maybe_cached(
        &builder_class,
        class_loader,
        cache,
    );
    let effective_methods = builder_methods_with_unsubstituted_parent_templates(
        &builder_class,
        &resolved_builder,
        class_loader,
    );

    // Build a substitution map: TModel → concrete model class name,
    // and static/$this/self → Builder<ConcreteModel>.
    let builder_self_type = PhpType::Generic(
        builder_fqn.to_string(),
        vec![PhpType::Named(class.name.to_string())],
    );
    let mut subs = super::self_ref_subs(builder_self_type.clone());
    insert_builder_template_substitutions(
        &mut subs,
        &builder_class,
        class,
        builder_fqn,
        class_loader,
    );

    let mut methods = Vec::new();

    for method in &effective_methods {
        if method.visibility != Visibility::Public {
            continue;
        }
        // Skip magic methods (__construct, __call, etc.).
        if method.name.starts_with("__") {
            continue;
        }
        // Skip methods already present on the model (real methods,
        // scope methods, etc.).  The merge logic in
        // `merge_virtual_members` would also skip them, but filtering
        // here avoids unnecessary cloning and substitution work.
        if class
            .methods
            .iter()
            .any(|m| m.name == method.name && m.is_static)
        {
            continue;
        }

        let mut forwarded = (**method).clone();
        forwarded.is_static = true;

        // Apply template and self-type substitutions.
        if !subs.is_empty() {
            if let Some(ref mut ret) = forwarded.return_type {
                *ret = ret.substitute(&subs);
            }
            if let Some(ref mut cond) = forwarded.conditional_return {
                apply_substitution_to_conditional(cond, &subs);
            }
            for param in &mut forwarded.parameters {
                if let Some(ref mut hint) = param.type_hint {
                    *hint = hint.substitute(&subs);
                }
            }
        }

        // Replace Eloquent Collection with custom collection class.
        if let Some(coll) = class.laravel().and_then(|l| l.custom_collection.as_ref())
            && let Some(coll_name) = coll.base_name()
            && let Some(ref mut ret) = forwarded.return_type
        {
            *ret = replace_eloquent_collection_typed(ret, coll_name);
        }

        methods.push(forwarded);
    }

    // ── query() / newQuery() / newModelQuery() ──────────────────────
    // When a model has a custom builder, User::query() should return
    // UserBuilder<User> instead of the default Builder<User>.
    for name in ["query", "newQuery", "newModelQuery"] {
        methods.push(MethodInfo {
            is_static: true,
            ..MethodInfo::virtual_method_typed(name, Some(&builder_self_type))
        });
    }

    methods
}

fn builder_methods_with_unsubstituted_parent_templates(
    builder_class: &ClassInfo,
    resolved_builder: &ClassInfo,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> Vec<Arc<MethodInfo>> {
    let mut methods: Vec<Arc<MethodInfo>> = resolved_builder.methods.iter().cloned().collect();
    if builder_class.fqn() == ELOQUENT_BUILDER_FQN || builder_class.name == ELOQUENT_BUILDER_FQN {
        return methods;
    }

    let Some(base_builder) = class_loader(ELOQUENT_BUILDER_FQN) else {
        return methods;
    };
    let resolved_base =
        crate::virtual_members::resolve_class_fully_maybe_cached(&base_builder, class_loader, None);

    for parent_method in &resolved_base.methods {
        if builder_class
            .methods
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(&parent_method.name))
        {
            continue;
        }

        if let Some(existing) = methods
            .iter_mut()
            .find(|m| m.name.eq_ignore_ascii_case(&parent_method.name))
        {
            *existing = Arc::clone(parent_method);
        } else {
            methods.push(Arc::clone(parent_method));
        }
    }

    methods
}

fn insert_builder_template_substitutions(
    subs: &mut std::collections::HashMap<String, PhpType>,
    builder_class: &ClassInfo,
    model_class: &ClassInfo,
    builder_fqn: &str,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) {
    let model_type = PhpType::Named(model_class.name.to_string());
    for param in &builder_class.template_params {
        subs.insert(param.to_string(), model_type.clone());
    }

    if builder_fqn != ELOQUENT_BUILDER_FQN
        && let Some(base_builder) = class_loader(ELOQUENT_BUILDER_FQN)
    {
        for param in &base_builder.template_params {
            subs.entry(param.to_string())
                .or_insert_with(|| model_type.clone());
        }
    }

    subs.entry("TModel".to_string()).or_insert(model_type);
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "builder_tests.rs"]
mod tests;

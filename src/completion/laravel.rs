use crate::Backend;
use crate::completion::resolver::ResolutionCtx;
use crate::php_type::PhpType;
use crate::types::{ClassInfo, FileContext, ResolvedType, Visibility};
use crate::util::{find_class_at_offset, position_to_offset};
use crate::virtual_members::laravel::{
    ELOQUENT_BUILDER_FQN, RELATIONSHIP_STRING_METHODS, classify_relationship_typed,
    extends_eloquent_builder, extends_eloquent_model, resolve_relation_chain,
};
use std::sync::Arc;
use tower_lsp::lsp_types::*;

pub(crate) fn try_laravel_completion(
    backend: &Backend,
    uri: &str,
    content: &str,
    position: Position,
    file_ctx: &FileContext,
) -> Option<CompletionResponse> {
    let offset = position_to_offset(content, position);
    let symbol_map = backend.symbol_maps.read().get(uri).cloned()?;

    // Find the innermost call site containing the cursor.
    let call_site = symbol_map.find_enclosing_call_site(offset)?;

    // Extract the method name from the call expression.
    // Format: "functionName", "$subject->method", "ClassName::method", "new ClassName"
    let method_name = extract_method_name(&call_site.call_expression)?;

    if !RELATIONSHIP_STRING_METHODS
        .iter()
        .any(|&m| m.eq_ignore_ascii_case(method_name))
    {
        return None;
    }

    // Only complete for the first argument.
    let active_param = call_site
        .comma_offsets
        .iter()
        .filter(|&&o| o < offset)
        .count();
    if active_param > 0 {
        return None;
    }

    // Resolve the receiver to find the base model.
    let class_loader = backend.class_loader(file_ctx);
    let function_loader = backend.function_loader(file_ctx);
    let cursor_offset = offset;
    let current_class = find_class_at_offset(&file_ctx.classes, cursor_offset);

    let rctx = ResolutionCtx {
        current_class,
        all_classes: &file_ctx.classes,
        content,
        cursor_offset,
        class_loader: &class_loader,
        resolved_class_cache: Some(&backend.resolved_class_cache),
        function_loader: Some(&function_loader),
        scope_var_resolver: None,
    };

    let receiver = extract_receiver(&call_site.call_expression)?;
    let resolved_types = if receiver == "static" || receiver == "self" || receiver == "parent" {
        let cc = current_class?;
        if receiver == "parent" {
            let p = cc.parent_class.as_ref()?;
            vec![ResolvedType::from_arc(backend.find_or_load_class(p)?)]
        } else {
            vec![ResolvedType::from_arc(Arc::new(cc.clone()))]
        }
    } else {
        crate::completion::resolver::resolve_target_classes(
            receiver,
            crate::types::AccessKind::Arrow,
            &rctx,
        )
    };

    let model = find_model_from_resolved_types(&resolved_types, &class_loader)?;

    // Determine the partial relationship string and dot-context.
    let (prefix, partial) =
        get_relationship_prefix(content, offset, call_site.args_start as usize)?;

    let target_model = if prefix.is_empty() {
        model
    } else {
        let fqn = resolve_relation_chain(
            &model,
            &prefix,
            &class_loader,
            Some(&backend.resolved_class_cache),
        )?;
        class_loader(&fqn)?
    };

    let items = build_relationship_completions(&target_model, &partial, &class_loader, backend);

    if items.is_empty() {
        None
    } else {
        Some(CompletionResponse::Array(items))
    }
}

fn extract_method_name(call_expr: &str) -> Option<&str> {
    if let Some(pos) = call_expr.rfind("->") {
        return Some(&call_expr[pos + 2..]);
    }
    if let Some(pos) = call_expr.rfind("::") {
        return Some(&call_expr[pos + 2..]);
    }
    if call_expr.starts_with("new ") {
        return None;
    }
    Some(call_expr)
}

fn extract_receiver(call_expr: &str) -> Option<&str> {
    if let Some(pos) = call_expr.find("->") {
        return Some(&call_expr[..pos]);
    }
    if let Some(pos) = call_expr.find("::") {
        return Some(&call_expr[..pos]);
    }
    None
}

fn find_model_from_resolved_types(
    types: &[ResolvedType],
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> Option<Arc<ClassInfo>> {
    for rt in types {
        if let Some(cls) = rt.class_info.as_ref() {
            if extends_eloquent_model(cls, class_loader) {
                return Some(Arc::clone(cls));
            }

            if let Some(model_cls) = extract_model_from_builder_type(&rt.type_string, class_loader)
            {
                return Some(model_cls);
            }

            // Check for Builder<Model>. Custom builders are supported by
            // walking their parent chain to the Eloquent Builder base class.
            let fqn = cls.fqn();
            if cls.name == "Builder"
                || fqn == ELOQUENT_BUILDER_FQN
                || extends_eloquent_builder(cls, class_loader)
            {
                // Try to extract model from Builder's template params or return types.
                if let Some(model_type) = extract_model_from_builder(cls)
                    && let Some(model_name) = model_type.base_name()
                    && let Some(model_cls) = class_loader(model_name)
                    && extends_eloquent_model(&model_cls, class_loader)
                {
                    return Some(model_cls);
                }
            }
        }
    }
    None
}

fn extract_model_from_builder_type(
    ty: &PhpType,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
) -> Option<Arc<ClassInfo>> {
    let PhpType::Generic(base, args) = ty else {
        return None;
    };
    let model_type = args.first()?;
    if model_type.is_empty() || model_type.is_named("TModel") {
        return None;
    }

    let builder_cls = class_loader(base)?;
    if builder_cls.fqn() != ELOQUENT_BUILDER_FQN
        && builder_cls.name != "Builder"
        && !extends_eloquent_builder(&builder_cls, class_loader)
    {
        return None;
    }

    let model_name = model_type.base_name()?;
    let model_cls = class_loader(model_name)?;
    extends_eloquent_model(&model_cls, class_loader).then_some(model_cls)
}

fn extract_model_from_builder(builder: &ClassInfo) -> Option<PhpType> {
    for method in &builder.methods {
        if let Some(ref ret) = method.return_type
            && let PhpType::Generic(base, args) = ret
            && !args.is_empty()
            && (base == ELOQUENT_BUILDER_FQN || base == "Builder")
            && !args[0].is_empty()
            && !args[0].is_named("TModel")
        {
            return Some(args[0].clone());
        }
    }
    None
}

fn get_relationship_prefix(
    content: &str,
    offset: u32,
    args_start: usize,
) -> Option<(String, String)> {
    let offset = offset as usize;
    if offset <= args_start {
        return None;
    }

    let text = &content[args_start..offset];

    // Find the start of the current string literal.
    // We expect the cursor to be inside quotes.
    let mut i = text.len();
    while i > 0 && text.as_bytes()[i - 1] != b'\'' && text.as_bytes()[i - 1] != b'"' {
        i -= 1;
    }

    if i == 0 {
        return None; // Not inside a string literal?
    }

    let full_str = &text[i..];
    if let Some(dot_pos) = full_str.rfind('.') {
        let prefix = full_str[..dot_pos].to_string();
        let partial = full_str[dot_pos + 1..].to_string();
        Some((prefix, partial))
    } else {
        Some((String::new(), full_str.to_string()))
    }
}

fn build_relationship_completions(
    model: &ClassInfo,
    partial: &str,
    class_loader: &dyn Fn(&str) -> Option<Arc<ClassInfo>>,
    backend: &Backend,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Fully resolve the model to get methods from traits/parents.
    let resolved_model = crate::virtual_members::resolve_class_fully_maybe_cached(
        model,
        class_loader,
        Some(&backend.resolved_class_cache),
    );

    for method in &resolved_model.methods {
        let method_name = method.name.to_string();
        if !is_string_usable_relationship_method(method) {
            continue;
        }

        if !method_name
            .to_lowercase()
            .starts_with(&partial.to_lowercase())
        {
            continue;
        }

        // Check if it returns a relationship.
        if let Some(ref rt) = method.return_type
            && classify_relationship_typed(rt).is_some()
        {
            items.push(CompletionItem {
                label: method_name.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some(format!("Laravel relation: {}", rt)),
                insert_text: Some(method_name.clone()),
                filter_text: Some(method_name),
                ..CompletionItem::default()
            });
        }
    }

    items
}

fn is_string_usable_relationship_method(method: &crate::types::MethodInfo) -> bool {
    method.visibility == Visibility::Public
        && !method.parameters.iter().any(|p| p.is_required)
        && !is_eloquent_relationship_factory_method(&method.name)
}

fn is_eloquent_relationship_factory_method(name: &str) -> bool {
    matches!(
        name,
        "hasOne"
            | "hasMany"
            | "belongsTo"
            | "belongsToMany"
            | "morphOne"
            | "morphMany"
            | "morphTo"
            | "morphToMany"
            | "morphedByMany"
            | "hasManyThrough"
            | "hasOneThrough"
            | "newHasOne"
            | "newHasMany"
            | "newBelongsTo"
            | "newBelongsToMany"
            | "newMorphOne"
            | "newMorphMany"
            | "newMorphTo"
            | "newMorphToMany"
            | "newMorphedByMany"
            | "newHasManyThrough"
            | "newHasOneThrough"
    )
}

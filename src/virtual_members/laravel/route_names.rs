use std::path::PathBuf;

use bumpalo::Bump;
use mago_database::file::FileId;
use mago_syntax::ast::*;
use tower_lsp::lsp_types::{Location, Url};

use crate::Backend;
use crate::util::offset_to_position;

use super::helpers::extract_string_literal;

/// Resolve `route('name')` to the `->name('name')` declaration in `routes/`.
///
/// Supports both explicit full-name declarations:
///   `Route::get('/create', ...)->name('admin.email.template.create')`
///
/// And group prefix notation:
///   `Route::name('admin.email.template.')->group(fn() { Route::get(...)->name('create'); })`
pub(crate) fn resolve_route_definition(backend: &Backend, name: &str) -> Option<Location> {
    let root = backend.workspace_root.read().clone()?;
    let routes_dir = root.join("routes");
    if !routes_dir.is_dir() {
        return None;
    }

    for path in collect_php_files(&routes_dir) {
        let Some(uri) = Url::from_file_path(&path).ok() else {
            continue;
        };
        let Some(content) = backend.get_file_content(&uri.to_string()) else {
            continue;
        };
        if let Some(loc) = scan_route_file(&content, name, &uri) {
            return Some(loc);
        }
    }
    None
}

// ─── Route file scanner ──────────────────────────────────────────────────────

fn scan_route_file(content: &str, target: &str, uri: &Url) -> Option<Location> {
    let arena = Bump::new();
    let file_id = FileId::new("input.php");
    let program = mago_syntax::parser::parse_file_content(&arena, file_id, content);

    for stmt in program.statements.iter() {
        if let Some(loc) = scan_stmt(stmt, content, "", target, uri) {
            return Some(loc);
        }
    }
    None
}

fn scan_stmt<'a>(
    stmt: &Statement<'a>,
    content: &str,
    prefix: &str,
    target: &str,
    uri: &Url,
) -> Option<Location> {
    match stmt {
        Statement::Expression(e) => scan_expr(e.expression, content, prefix, target, uri),
        Statement::Return(r) => {
            r.value
                .and_then(|v| scan_expr(v, content, prefix, target, uri))
        }
        _ => None,
    }
}

/// Walk a call-chain expression while tracking the accumulated group name prefix.
///
/// - `->group(closure)` — extract name prefix from the preceding chain and
///   recursively scan the closure body with the combined prefix.
/// - `->name('something')` — check if `prefix + something == target`.
/// - All other method calls — recurse into the receiver object.
fn scan_expr<'a>(
    expr: &Expression<'a>,
    content: &str,
    prefix: &str,
    target: &str,
    uri: &Url,
) -> Option<Location> {
    let Expression::Call(Call::Method(mc)) = expr else {
        return None;
    };
    let ClassLikeMemberSelector::Identifier(ident) = &mc.method else {
        // Dynamic method name – recurse into receiver.
        return scan_expr(mc.object, content, prefix, target, uri);
    };
    let method = ident.value.to_ascii_lowercase();

    if method == "group" {
        // Collect the ->name('prefix.') value(s) from the chain before ->group().
        let chain_prefix = chain_name_prefix(mc.object, content);
        let new_prefix = format!("{prefix}{chain_prefix}");

        for arg in mc.argument_list.arguments.iter() {
            if let Some(loc) = scan_group_body(arg.value(), content, &new_prefix, target, uri) {
                return Some(loc);
            }
        }
        return None;
    }

    if method == "name" {
        if let Some(first_arg) = mc.argument_list.arguments.iter().next() {
            if let Some((name_val, start, _)) = extract_string_literal(first_arg.value(), content)
            {
                let full = format!("{prefix}{name_val}");
                if full == target {
                    return Some(crate::definition::point_location(
                        uri.clone(),
                        offset_to_position(content, start),
                    ));
                }
            }
        }
        // ->name() can appear mid-chain (e.g. ->name('x')->middleware(...)); continue.
        return scan_expr(mc.object, content, prefix, target, uri);
    }

    // Any other method: continue up the receiver chain.
    scan_expr(mc.object, content, prefix, target, uri)
}

/// Walk the argument that was passed to `->group()`.
fn scan_group_body<'a>(
    expr: &Expression<'a>,
    content: &str,
    prefix: &str,
    target: &str,
    uri: &Url,
) -> Option<Location> {
    match expr {
        Expression::Closure(closure) => {
            for stmt in closure.body.statements.iter() {
                if let Some(loc) = scan_stmt(stmt, content, prefix, target, uri) {
                    return Some(loc);
                }
            }
            None
        }
        Expression::ArrowFunction(af) => scan_expr(af.expression, content, prefix, target, uri),
        _ => None,
    }
}

/// Collect all `->name('...')` values from the call chain that precedes `->group()`.
///
/// Handles both instance method chains (`->name('prefix.')`) and the static
/// entry point (`Route::name('prefix.')`).
fn chain_name_prefix<'a>(expr: &Expression<'a>, content: &str) -> String {
    match expr {
        Expression::Call(Call::Method(mc)) => {
            let ClassLikeMemberSelector::Identifier(ident) = &mc.method else {
                return chain_name_prefix(mc.object, content);
            };
            if ident.value.eq_ignore_ascii_case("name") {
                let arg_name = mc
                    .argument_list
                    .arguments
                    .iter()
                    .next()
                    .and_then(|a| extract_string_literal(a.value(), content))
                    .map(|(n, _, _)| n)
                    .unwrap_or("");
                let parent = chain_name_prefix(mc.object, content);
                format!("{parent}{arg_name}")
            } else {
                chain_name_prefix(mc.object, content)
            }
        }
        // Route::name('prefix.') — static entry point of the chain.
        Expression::Call(Call::StaticMethod(sc)) => {
            let ClassLikeMemberSelector::Identifier(ident) = &sc.method else {
                return String::new();
            };
            if ident.value.eq_ignore_ascii_case("name") {
                sc.argument_list
                    .arguments
                    .iter()
                    .next()
                    .and_then(|a| extract_string_literal(a.value(), content))
                    .map(|(n, _, _)| n.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

// ─── File collector ──────────────────────────────────────────────────────────

fn collect_php_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("php"))
        .collect()
}

use std::ops::ControlFlow;
use std::path::PathBuf;

use mago_syntax::ast::*;
use tower_lsp::lsp_types::{Location, Url};

use crate::Backend;
use crate::util::offset_to_position;

use super::helpers::{extract_string_literal, walk_all_php_expressions};

/// Resolve `route('name')` to the `->name('name')` declaration in `routes/`.
pub(crate) fn resolve_route_definition(backend: &Backend, name: &str) -> Option<Location> {
    let root = backend.workspace_root.read().clone()?;
    let routes_dir = root.join("routes");
    if !routes_dir.is_dir() {
        return None;
    }

    for path in collect_php_files(&routes_dir) {
        let uri = Url::from_file_path(&path).ok()?;
        let content = backend
            .get_file_content(&uri.to_string())
            .or_else(|| std::fs::read_to_string(&path).ok())?;
        if let Some(loc) = find_route_name_decl(&content, name, &uri) {
            return Some(loc);
        }
    }
    None
}

fn find_route_name_decl(content: &str, target: &str, uri: &Url) -> Option<Location> {
    let mut result: Option<Location> = None;
    walk_all_php_expressions(content, &mut |expr| {
        let Expression::Call(Call::Method(mc)) = expr else {
            return ControlFlow::Continue(());
        };
        let ClassLikeMemberSelector::Identifier(ident) = &mc.method else {
            return ControlFlow::Continue(());
        };
        if !ident.value.eq_ignore_ascii_case("name") {
            return ControlFlow::Continue(());
        }
        let Some(first_arg) = mc.argument_list.arguments.iter().next() else {
            return ControlFlow::Continue(());
        };
        let Some((found, start, _)) = extract_string_literal(first_arg.value(), content) else {
            return ControlFlow::Continue(());
        };
        if found == target {
            result = Some(crate::definition::point_location(
                uri.clone(),
                offset_to_position(content, start),
            ));
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    result
}

/// Collect all `.php` files under `dir` (non-recursive, routes/ is typically flat).
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

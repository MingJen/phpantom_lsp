use std::path::PathBuf;

use bumpalo::Bump;
use mago_database::file::FileId;
use mago_syntax::ast::*;
use tower_lsp::lsp_types::{Location, Position, Url};

use crate::Backend;

/// Resolve `__('file.key')` / `trans('file.key')` / `Lang::get('file.key')` to the
/// matching key inside a `lang/{locale}/file.php` translation file.
///
/// The key format is `file_stem.nested.key` (first segment = file, rest = array path).
/// Falls back to the top of the file when the exact key cannot be located.
pub(crate) fn resolve_trans_definition(backend: &Backend, key: &str) -> Option<Location> {
    let root = backend.workspace_root.read().clone()?;

    let mut parts = key.splitn(2, '.');
    let file_stem = parts.next()?;

    for lang_base in candidate_lang_dirs(&root) {
        if !lang_base.is_dir() {
            continue;
        }
        if let Some(loc) = find_in_lang_base(backend, &lang_base, file_stem, key) {
            return Some(loc);
        }
    }
    None
}

/// Returns candidate base lang directories in priority order.
fn candidate_lang_dirs(root: &std::path::Path) -> [PathBuf; 2] {
    [root.join("lang"), root.join("resources/lang")]
}

/// Search inside a single lang base dir (e.g. `lang/`) across all locales.
/// Prefers `en` locale; falls back to the first locale sub-directory found.
fn find_in_lang_base(
    backend: &Backend,
    lang_base: &std::path::Path,
    file_stem: &str,
    full_key: &str,
) -> Option<Location> {
    let Ok(entries) = std::fs::read_dir(lang_base) else {
        return None;
    };

    let mut locale_dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    // Prefer `en` locale.
    locale_dirs.sort_by(|a, b| {
        let a_is_en = a.file_name().and_then(|n| n.to_str()) == Some("en");
        let b_is_en = b.file_name().and_then(|n| n.to_str()) == Some("en");
        b_is_en.cmp(&a_is_en)
    });

    for locale_dir in &locale_dirs {
        let php_path = locale_dir.join(format!("{file_stem}.php"));
        if !php_path.is_file() {
            continue;
        }
        let uri = Url::from_file_path(&php_path).ok()?;
        let content = backend
            .get_file_content(&uri.to_string())
            .or_else(|| std::fs::read_to_string(&php_path).ok())?;

        let declarations = collect_trans_declarations(&content, file_stem);
        if let Some(decl) = declarations.into_iter().find(|d| d.key == full_key) {
            let pos = crate::util::offset_to_position(&content, decl.start);
            return Some(crate::definition::point_location(uri, pos));
        }

        // File matched by stem but key not found — point to top of file.
        return Some(crate::definition::point_location(uri, Position::new(0, 0)));
    }
    None
}

// ─── Declaration extractor (mirrors config_keys logic) ───────────────────────

#[derive(Debug)]
struct TransKeyMatch {
    key: String,
    start: usize,
}

fn collect_trans_declarations(content: &str, file_stem: &str) -> Vec<TransKeyMatch> {
    let arena = Bump::new();
    let file_id = FileId::new("input.php");
    let program = mago_syntax::parser::parse_file_content(&arena, file_id, content);
    let mut out = Vec::new();

    let mut returned_var_name: Option<String> = None;
    let mut return_expr: Option<&Expression<'_>> = None;

    for stmt in program.statements.iter() {
        if let Statement::Return(ret) = stmt {
            if let Some(val) = ret.value {
                match val {
                    Expression::Variable(Variable::Direct(dv)) => {
                        returned_var_name = Some(dv.name.to_string());
                    }
                    _ => {
                        return_expr = Some(val);
                    }
                }
            }
            break;
        }
    }

    if let Some(expr) = return_expr {
        collect_expr(expr, content, file_stem, &[], &mut out);
    } else if let Some(var_name) = returned_var_name {
        for stmt in program.statements.iter() {
            if let Statement::Expression(expr_stmt) = stmt
                && let Expression::Assignment(assign) = expr_stmt.expression
                && let Expression::Variable(Variable::Direct(dv)) = assign.lhs
                && dv.name == var_name
            {
                collect_expr(assign.rhs, content, file_stem, &[], &mut out);
            }
        }
    }

    out
}

fn collect_expr<'a>(
    expr: &'a Expression<'a>,
    content: &str,
    prefix: &str,
    path: &[String],
    out: &mut Vec<TransKeyMatch>,
) {
    match expr {
        Expression::Array(arr) => {
            collect_array(arr.elements.iter(), content, prefix, path, out);
        }
        Expression::LegacyArray(arr) => {
            collect_array(arr.elements.iter(), content, prefix, path, out);
        }
        Expression::Parenthesized(p) => {
            collect_expr(p.expression, content, prefix, path, out);
        }
        Expression::Call(Call::Function(fc)) => {
            if let Expression::Identifier(ident) = fc.function
                && ident.value().eq_ignore_ascii_case("array_merge")
            {
                for arg in fc.argument_list.arguments.iter() {
                    let arg_expr = match arg {
                        Argument::Positional(pos) => pos.value,
                        Argument::Named(named) => named.value,
                    };
                    collect_expr(arg_expr, content, prefix, path, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_array<'a>(
    elements: impl Iterator<Item = &'a ArrayElement<'a>>,
    content: &str,
    prefix: &str,
    path: &[String],
    out: &mut Vec<TransKeyMatch>,
) {
    for element in elements {
        let ArrayElement::KeyValue(kv) = element else {
            continue;
        };
        let Some((key_text, key_start, _)) =
            super::helpers::extract_string_literal(kv.key, content)
        else {
            continue;
        };

        let mut full_path = path.to_vec();
        full_path.push(key_text.to_string());
        let dot_key = format!("{prefix}.{}", full_path.join("."));
        out.push(TransKeyMatch {
            key: dot_key,
            start: key_start,
        });

        collect_expr(kv.value, content, prefix, &full_path, out);
    }
}

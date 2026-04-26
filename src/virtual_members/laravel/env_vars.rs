use std::sync::Arc;

use mago_syntax::ast::*;
use tower_lsp::lsp_types::{Location, Position, Url};

use crate::Backend;
use crate::symbol_map::{LaravelStringKind, SymbolKind, SymbolMap};
use crate::util::{offset_to_position, push_unique_location, strip_fqn_prefix};

use super::helpers::{extract_string_literal, walk_all_php_expressions};

/// Env file names probed in priority order.
///
/// The first file that exists and contains the key wins for go-to-definition;
/// all matching files are included for find-references declarations.
pub(crate) const ENV_FILE_NAMES: &[&str] = &[
    ".env.local",
    ".env",
    ".env.testing",
    ".env.production",
    ".env.staging",
    ".env.example",
];

/// Resolve an already-known env `key` to its declaration in the first
/// matching env file (highest-priority first per [`ENV_FILE_NAMES`]).
pub(crate) fn resolve_env_key_definition(backend: &Backend, key: &str) -> Option<Location> {
    let root = backend.workspace_root.read().clone()?;
    for name in ENV_FILE_NAMES {
        let path = root.join(name);
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        if let Some(pos) = find_env_key_position(&content, key) {
            let uri = Url::from_file_path(&path).ok()?;
            return Some(crate::definition::point_location(uri, pos));
        }
    }
    None
}

/// Return all env file locations where `KEY=` is declared (used when
/// `include_declaration` is true in find-references).
pub(crate) fn find_env_key_declarations(backend: &Backend, key: &str) -> Vec<Location> {
    let Some(root) = backend.workspace_root.read().clone() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in ENV_FILE_NAMES {
        let path = root.join(name);
        if !path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(pos) = find_env_key_position(&content, key) {
            if let Ok(uri) = Url::from_file_path(&path) {
                out.push(crate::definition::point_location(uri, pos));
            }
        }
    }
    out
}

/// Find all `env('KEY')` references across the project.
///
/// Iterates pre-built [`SymbolKind::LaravelStringKey`] spans (zero re-parses
/// per PHP file). When `include_declaration` is true, `.env*` declaration
/// lines are appended at the end.
pub(crate) fn find_env_references(
    backend: &Backend,
    key: &str,
    snapshot: &[(String, Arc<SymbolMap>)],
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    for (file_uri, symbol_map) in snapshot {
        let parsed_uri = match Url::parse(file_uri) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let file_content = match backend.get_file_content_arc(file_uri) {
            Some(c) => c,
            None => continue,
        };
        for span in &symbol_map.spans {
            if let SymbolKind::LaravelStringKey {
                kind: LaravelStringKind::Env,
                key: k,
            } = &span.kind
                && k == key
            {
                let start = offset_to_position(&file_content, span.start as usize);
                let end = offset_to_position(&file_content, span.end as usize);
                push_unique_location(&mut locations, &parsed_uri, start, end);
            }
        }
    }

    if include_declaration {
        locations.extend(find_env_key_declarations(backend, key));
    }

    locations
}

// ─── Entry point for cursor-in-.env-file ────────────────────────────────────

/// Handle "Find All References" when the cursor is inside a `.env*` file.
///
/// Extracts the env key from the current line (the part before `=`), then
/// scans the pre-built symbol map for all matching `env('KEY')` PHP call
/// sites.  When `include_declaration` is true the declaration lines in every
/// env file are appended at the end (same behaviour as the PHP-side handler).
pub(crate) fn find_env_references_from_dotenv(
    backend: &Backend,
    content: &str,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let key = env_key_at_dotenv_position(content, position)?;
    let snapshot = backend.user_file_symbol_maps();
    let locations = find_env_references(backend, &key, &snapshot, include_declaration);
    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

/// Extract the env key (`KEY` in `KEY=value`) from a cursor position in a
/// `.env` file.  Returns `None` for comment lines, blank lines, or lines
/// without an `=`.
fn env_key_at_dotenv_position(content: &str, position: Position) -> Option<String> {
    let line = content.lines().nth(position.line as usize)?;
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.is_empty() {
        return None;
    }
    let key = trimmed.split('=').next()?.trim();
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

// ─── Fallback path (cursor not yet in symbol map) ───────────────────────────
//
// Used only when the env() call site hasn't been indexed yet (e.g. a file
// that was just opened but not scanned). Once the symbol map is populated
// the normal `resolve_from_symbol` path takes over.

/// Fallback: resolve `env('KEY')` under cursor when the symbol map has no
/// span for this position yet.
pub(crate) fn resolve_env_definition_fallback(
    backend: &Backend,
    content: &str,
    position: Position,
) -> Option<Location> {
    if !content.contains("env(") {
        return None;
    }
    let cursor_offset = crate::util::position_to_offset(content, position) as usize;
    let key = find_env_usage_at_cursor(content, cursor_offset)?;
    resolve_env_key_definition(backend, &key)
}

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Return the env key string for the `env('KEY')` call under cursor.
fn find_env_usage_at_cursor(content: &str, cursor_offset: usize) -> Option<String> {
    let mut found: Option<String> = None;
    walk_all_php_expressions(content, &mut |expr| {
        if found.is_some() {
            return;
        }
        if let Some((key, s, e)) = try_env_call(expr, content)
            && cursor_offset >= s
            && cursor_offset <= e
        {
            found = Some(key.to_string());
        }
    });
    found
}

fn try_env_call<'c>(expr: &Expression<'_>, content: &'c str) -> Option<(&'c str, usize, usize)> {
    let Expression::Call(Call::Function(fc)) = expr else {
        return None;
    };
    let Expression::Identifier(ident) = fc.function else {
        return None;
    };
    if !strip_fqn_prefix(ident.value()).eq_ignore_ascii_case("env") {
        return None;
    }
    let first_arg = fc.argument_list.arguments.iter().next()?.value();
    extract_string_literal(first_arg, content)
}

/// Return the position of `KEY=` in an env file, or `None` if not found.
fn find_env_key_position(env_content: &str, key: &str) -> Option<Position> {
    for (line_idx, line) in env_content.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(key)
            && rest.trim_start().starts_with('=')
        {
            return Some(Position::new(line_idx as u32, 0));
        }
    }
    None
}

/// Find the line number of `KEY=` in env content, falling back to line 0.
///
/// Kept for the fallback go-to-definition path where the caller already
/// knows the file exists and wants a best-effort position.
fn find_env_key_line(env_content: &str, key: &str) -> Position {
    find_env_key_position(env_content, key).unwrap_or(Position::new(0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_key_at(content: &str, offset: usize) -> Option<String> {
        find_env_usage_at_cursor(content, offset)
    }

    #[test]
    fn detects_env_call() {
        let php = "<?php\n$v = env('APP_NAME');\n";
        let offset = php.find("APP_NAME").unwrap();
        assert_eq!(env_key_at(php, offset), Some("APP_NAME".into()));
    }

    #[test]
    fn ignores_non_env_string() {
        let php = "<?php\n$v = 'APP_NAME';\n";
        let offset = php.find("APP_NAME").unwrap();
        assert!(env_key_at(php, offset).is_none());
    }

    #[test]
    fn detects_env_with_default() {
        let php = "<?php\nenv('DB_HOST', 'localhost');\n";
        let offset = php.find("DB_HOST").unwrap();
        assert_eq!(env_key_at(php, offset), Some("DB_HOST".into()));
    }

    #[test]
    fn finds_env_key_line() {
        let env = "APP_NAME=Laravel\nDB_HOST=127.0.0.1\n";
        let pos = find_env_key_line(env, "DB_HOST");
        assert_eq!(pos.line, 1);
    }

    #[test]
    fn missing_env_key_returns_line_zero() {
        let env = "APP_NAME=Laravel\n";
        let pos = find_env_key_line(env, "MISSING");
        assert_eq!(pos.line, 0);
    }

    #[test]
    fn find_env_key_position_returns_none_for_missing() {
        let env = "APP_NAME=Laravel\n";
        assert!(find_env_key_position(env, "MISSING").is_none());
    }

    #[test]
    fn find_env_key_position_finds_key() {
        let env = "APP_NAME=Laravel\nAPP_KEY=secret\n";
        let pos = find_env_key_position(env, "APP_KEY").unwrap();
        assert_eq!(pos.line, 1);
    }
}

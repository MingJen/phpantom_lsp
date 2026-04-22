use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Url};

use crate::Backend;
use crate::symbol_map::SymbolMap;
use crate::util::{offset_to_position, push_unique_location};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigKeyMatch {
    pub(crate) key: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Clone)]
struct ConfigArrayFrame {
    close: u8,
    prev_path_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentState {
    None,
    Line,
    Block,
}

/// Extract `app` from a URI like `file:///.../config/app.php`.
pub(crate) fn laravel_config_prefix_from_uri(uri: &str) -> Option<String> {
    let parsed = Url::parse(uri).ok()?;
    let path = parsed.path();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let config_idx = segments.iter().position(|seg| *seg == "config")?;
    let file = segments.last()?;
    if config_idx >= segments.len().saturating_sub(1) || !file.ends_with(".php") {
        return None;
    }
    let stem = file.strip_suffix(".php")?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

/// Find the config usage key under cursor in `config('x.y')` or
/// `Config::get('x.y')`.
pub(crate) fn find_laravel_config_usage_at_cursor(
    content: &str,
    cursor_offset: usize,
) -> Option<ConfigKeyMatch> {
    scan_laravel_config_usages(content, None)
        .into_iter()
        .find(|m| cursor_offset >= m.start && cursor_offset <= m.end)
}

/// Scan `content` for Laravel config usages.
///
/// When `target_key` is set, only usages with exactly that key are returned.
pub(crate) fn scan_laravel_config_usages(
    content: &str,
    target_key: Option<&str>,
) -> Vec<ConfigKeyMatch> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut comment = CommentState::None;

    while i < bytes.len() {
        match comment {
            CommentState::Line => {
                if bytes[i] == b'\n' {
                    comment = CommentState::None;
                }
                i += 1;
                continue;
            }
            CommentState::Block => {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    comment = CommentState::None;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            CommentState::None => {}
        }

        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                comment = CommentState::Line;
                i += 2;
                continue;
            }
            if bytes[i + 1] == b'*' {
                comment = CommentState::Block;
                i += 2;
                continue;
            }
        }
        if bytes[i] == b'#' {
            comment = CommentState::Line;
            i += 1;
            continue;
        }

        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let quote_start = i;
            i += 1;
            let inner_start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                i += 1;
            }
            if i < bytes.len() && bytes[i] == quote {
                let inner_end = i;
                if is_laravel_config_call_prefix(content, quote_start) {
                    let value = &content[inner_start..inner_end];
                    if !value.is_empty()
                        && !value.contains('$')
                        && target_key.is_none_or(|key| key == value)
                    {
                        out.push(ConfigKeyMatch {
                            key: value.to_string(),
                            start: inner_start,
                            end: inner_end,
                        });
                    }
                }
            }
        }

        i += 1;
    }

    out
}

/// Collect Laravel config declaration keys from a `config/*.php` file.
///
/// Produces keys in dot notation (`app.mail.from.address`) and records
/// source spans for the key literal content (inside quotes).
pub(crate) fn collect_laravel_config_declarations(
    content: &str,
    prefix: &str,
) -> Vec<ConfigKeyMatch> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut frames: Vec<ConfigArrayFrame> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut pending_path: Option<Vec<String>> = None;
    let mut comment = CommentState::None;

    while i < bytes.len() {
        match comment {
            CommentState::Line => {
                if bytes[i] == b'\n' {
                    comment = CommentState::None;
                }
                i += 1;
                continue;
            }
            CommentState::Block => {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    comment = CommentState::None;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            CommentState::None => {}
        }

        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                comment = CommentState::Line;
                i += 2;
                continue;
            }
            if bytes[i + 1] == b'*' {
                comment = CommentState::Block;
                i += 2;
                continue;
            }
        }
        if bytes[i] == b'#' {
            comment = CommentState::Line;
            i += 1;
            continue;
        }

        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // String key candidate.
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            i += 1;
            let inner_start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != quote {
                break;
            }
            let inner_end = i;
            let key_text = &content[inner_start..inner_end];
            let after = skip_ws_and_comments(content, i + 1);
            if after + 1 < bytes.len() && bytes[after] == b'=' && bytes[after + 1] == b'>' {
                let mut full = path.clone();
                full.push(key_text.to_string());
                let key = format!("{prefix}.{}", full.join("."));
                out.push(ConfigKeyMatch {
                    key,
                    start: inner_start,
                    end: inner_end,
                });
                pending_path = Some(full);
            }
            i += 1;
            continue;
        }

        if bytes[i] == b'[' {
            let prev_path_len = path.len();
            if let Some(next) = pending_path.take() {
                path = next;
            }
            frames.push(ConfigArrayFrame {
                close: b']',
                prev_path_len,
            });
            i += 1;
            continue;
        }

        if bytes[i] == b'(' && is_array_call_before(content, i) {
            let prev_path_len = path.len();
            if let Some(next) = pending_path.take() {
                path = next;
            }
            frames.push(ConfigArrayFrame {
                close: b')',
                prev_path_len,
            });
            i += 1;
            continue;
        }

        if let Some(top) = frames.last()
            && bytes[i] == top.close
        {
            let frame = frames.pop().expect("frame exists");
            path.truncate(frame.prev_path_len);
            i += 1;
            continue;
        }

        // Keep pending path alive across `=>` / commas until we know
        // whether the value starts with an array container.
        if pending_path.is_some() && matches!(bytes[i], b'=' | b'>' | b',') {
            i += 1;
            continue;
        }

        // Any other non-whitespace token means the value started and is
        // not an array container we need to descend into.
        pending_path = None;
        i += 1;
    }

    out
}

/// Find all references for a Laravel config key across the project.
pub(crate) fn find_config_references(
    backend: &Backend,
    uri: &str,
    content: &str,
    position: tower_lsp::lsp_types::Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let cursor_offset = crate::util::position_to_offset(content, position) as usize;
    let target_key = config_key_at_cursor(backend, uri, content, cursor_offset)?;

    let snapshot = backend.user_file_symbol_maps();
    let mut locations = find_all_config_references(backend, &target_key, &snapshot, include_declaration);

    if locations.is_empty() {
        return None;
    }

    locations.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
    });

    Some(locations)
}

/// Resolve a Laravel config usage to its declaration in `config/*.php`.
pub(crate) fn resolve_config_definition(
    _backend: &Backend,
    content: &str,
    position: tower_lsp::lsp_types::Position,
) -> Option<Location> {
    let cursor_offset = crate::util::position_to_offset(content, position) as usize;
    let hit = find_laravel_config_usage_at_cursor(content, cursor_offset)?;
    let mut key_parts = hit.key.split('.');
    let config_file_stem = key_parts.next()?;

    let root = backend.workspace_root.read().clone()?;
    let config_path = root.join("config").join(format!("{config_file_stem}.php"));
    if !config_path.is_file() {
        return None;
    }

    let target_uri = Url::from_file_path(&config_path).ok()?;
    let target_uri_string = target_uri.to_string();
    let target_content = backend
        .get_file_content(&target_uri_string)
        .or_else(|| std::fs::read_to_string(&config_path).ok())?;

    let declarations = collect_laravel_config_declarations(&target_content, config_file_stem);
    if let Some(decl) = declarations.into_iter().find(|d| d.key == hit.key) {
        let pos = crate::util::offset_to_position(&target_content, decl.start);
        return Some(crate::definition::point_location(target_uri, pos));
    }

    // Key not found exactly: still jump to file top so users land in
    // the right config file.
    Some(crate::definition::point_location(target_uri, tower_lsp::lsp_types::Position::new(0, 0)))
}

/// Resolve the Laravel config key under cursor, from either a usage
/// site (`config('app.name')`) or a declaration site in `config/*.php`.
fn config_key_at_cursor(
    backend: &Backend,
    uri: &str,
    content: &str,
    cursor_offset: usize,
) -> Option<String> {
    if let Some(hit) = find_laravel_config_usage_at_cursor(content, cursor_offset) {
        return Some(hit.key);
    }

    let prefix = laravel_config_prefix_from_uri(uri)?;
    collect_laravel_config_declarations(content, &prefix)
        .into_iter()
        .find(|d| cursor_offset >= d.start && cursor_offset <= d.end)
        .map(|d| d.key)
}

/// Find all references for a Laravel config key across the project.
pub(crate) fn find_all_config_references(
    backend: &Backend,
    target_key: &str,
    snapshot: &[(String, Arc<SymbolMap>)],
    include_declaration: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();

    // Usages: config('...') / Config::get('...')
    for (file_uri, _) in snapshot {
        let parsed_uri = match Url::parse(file_uri) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let file_content = match backend.get_file_content_arc(file_uri) {
            Some(c) => c,
            None => continue,
        };
        for usage in scan_laravel_config_usages(&file_content, Some(target_key)) {
            let start = offset_to_position(&file_content, usage.start);
            let end = offset_to_position(&file_content, usage.end);
            push_unique_location(&mut locations, &parsed_uri, start, end);
        }
    }

    // Declarations: keys in config/*.php
    if include_declaration {
        for (file_uri, _) in snapshot {
            let prefix = match laravel_config_prefix_from_uri(file_uri) {
                Some(p) => p,
                None => continue,
            };
            let parsed_uri = match Url::parse(file_uri) {
                Ok(u) => u,
                Err(_) => continue,
            };
            let file_content = match backend.get_file_content_arc(file_uri) {
                Some(c) => c,
                None => continue,
            };
            for decl in collect_laravel_config_declarations(&file_content, &prefix) {
                if decl.key != target_key {
                    continue;
                }
                let start = offset_to_position(&file_content, decl.start);
                let end = offset_to_position(&file_content, decl.end);
                push_unique_location(&mut locations, &parsed_uri, start, end);
            }
        }
    }

    locations
}

/// Check whether the nearest call prefix before a string literal is
/// `config(` or `Config::get(` (with optional leading backslash).
fn is_laravel_config_call_prefix(content: &str, quote_start: usize) -> bool {
    let bytes = content.as_bytes();
    if quote_start == 0 {
        return false;
    }

    let mut i = quote_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'(' {
        return false;
    }
    i -= 1;

    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let end = i;
    while i > 0 && is_php_callee_char(bytes[i - 1]) {
        i -= 1;
    }
    if i == end {
        return false;
    }

    let callee = &content[i..end];
    let lower = callee.to_ascii_lowercase();
    lower == "config" || lower == "\\config" || lower == "config::get" || lower == "\\config::get"
}

fn is_php_callee_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'\\' || b == b':'
}

fn skip_ws_and_comments(content: &str, mut i: usize) -> usize {
    let bytes = content.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }
        if bytes[i] == b'#' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        break;
    }
    i
}

fn is_array_call_before(content: &str, open_paren: usize) -> bool {
    if open_paren == 0 {
        return false;
    }
    let bytes = content.as_bytes();
    let mut i = open_paren;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    let end = i;
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    if i == end {
        return false;
    }
    content[i..end].eq_ignore_ascii_case("array")
}

#[cfg(test)]
mod tests {
    use super::{
        find_laravel_config_usage_at_cursor, laravel_config_prefix_from_uri,
        scan_laravel_config_usages,
    };

    #[test]
    fn config_prefix_from_uri_normal() {
        assert_eq!(
            laravel_config_prefix_from_uri("file:///project/config/app.php"),
            Some("app".to_string())
        );
    }

    #[test]
    fn config_prefix_from_uri_root_level() {
        assert_eq!(
            laravel_config_prefix_from_uri("file:///config/app.php"),
            Some("app".to_string())
        );
    }

    #[test]
    fn config_prefix_from_uri_not_in_config_dir() {
        assert_eq!(
            laravel_config_prefix_from_uri("file:///project/src/Service.php"),
            None
        );
    }

    #[test]
    fn config_prefix_from_uri_file_named_config() {
        assert_eq!(
            laravel_config_prefix_from_uri("file:///project/config.php"),
            None
        );
    }

    #[test]
    fn scan_usages_ignores_comment_matches() {
        let php = concat!(
            "<?php\n",
            "// config('app.name')\n",
            "/* config('app.timezone') */\n",
            "$x = config('app.env');\n",
        );
        let found = scan_laravel_config_usages(php, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "app.env");
    }

    #[test]
    fn usage_at_cursor_ignores_non_call_string_literals() {
        let php = concat!(
            "<?php\n",
            "$x = 'app.name';\n",
            "$y = config('app.name');\n",
        );
        let bare = php.find("app.name';").expect("bare string");
        assert!(find_laravel_config_usage_at_cursor(php, bare).is_none());

        let call = php.rfind("app.name").expect("config call");
        let hit = find_laravel_config_usage_at_cursor(php, call).expect("should hit");
        assert_eq!(hit.key, "app.name");
    }
}

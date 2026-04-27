use tower_lsp::lsp_types::{Location, Position, Url};

use crate::Backend;

/// Resolve `view('name')` or `View::make('name')` to the corresponding blade template.
///
/// Converts dot-notation to a file path under `resources/views/`:
/// `'components.button'` → `resources/views/components/button.blade.php`
pub(crate) fn resolve_view_definition(backend: &Backend, name: &str) -> Option<Location> {
    let root = backend.workspace_root.read().clone()?;
    let views_dir = root.join("resources/views");
    let rel = name.replace('.', "/");

    for ext in [".blade.php", ".php"] {
        let path = views_dir.join(format!("{rel}{ext}"));
        if path.is_file() {
            let uri = Url::from_file_path(&path).ok()?;
            return Some(crate::definition::point_location(uri, Position::new(0, 0)));
        }
    }
    None
}

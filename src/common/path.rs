use std::path::{Component, Path, PathBuf};

/// Join a dataset-relative companion path without allowing absolute paths or
/// parent-directory escapes.
pub fn confined_join(base: &Path, relative: &str) -> Option<PathBuf> {
    let rel = Path::new(relative.trim());
    if rel.as_os_str().is_empty() || rel.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from(base);
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        }
    }
    Some(out)
}

use std::collections::HashSet;
use std::path::Path;

pub(super) fn default_name(path: &str, used: &HashSet<String>) -> String {
    let base = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let base = if base.is_empty() { "Library" } else { base };
    if !used.contains(base) {
        return base.to_owned();
    }
    for suffix in 1u64.. {
        let name = format!("{base}-{suffix}");
        if !used.contains(&name) {
            return name;
        }
    }
    unreachable!()
}

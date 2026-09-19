//! Minimal `.env` loader: `KEY=VALUE` lines, `#` comments. Never overrides a
//! variable that is already set and never prints values.

/// Secret files in precedence order: the user's (`~/.config/mobius/.env`),
/// then `./.env` next to the checkout. The process environment beats both.
pub fn sources() -> Vec<std::path::PathBuf> {
    vec![searcher_core::config::user_env_path(), std::path::PathBuf::from(".env")]
}

pub fn load(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let mut loaded = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().trim_start_matches("export ").trim();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if k.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        // SAFETY: called once at startup before any other thread exists.
        unsafe { std::env::set_var(k, v) };
        loaded.push(k.to_string());
    }
    loaded
}

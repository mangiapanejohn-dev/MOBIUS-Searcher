//! Writing operator threshold changes into the user's config file: only the
//! changed keys are touched (comments and layout stay), the previous file is
//! kept as `.bak`, and the result must load back to the same values.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Set `values` (dotted key → TOML value) in the file at `path` (created if
/// missing). Returns the backup path when there was a previous file.
pub fn write(path: &Path, values: &[(String, toml::Value)]) -> Result<Option<PathBuf>> {
    let before = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = before.parse().with_context(|| format!("parsing {}", path.display()))?;
    for (key, value) in values {
        let (sec, leaf) = key.split_once('.').with_context(|| format!("bad key {key}"))?;
        if !doc.contains_key(sec) {
            doc[sec] = toml_edit::table();
        }
        let table = doc[sec].as_table_mut().with_context(|| format!("[{sec}] is not a table in {}", path.display()))?;
        let new = to_edit(value)?;
        match table.get_mut(leaf).and_then(|i| i.as_value_mut()) {
            // keep the line's comment / spacing
            Some(old) => {
                let decor = old.decor().clone();
                *old = new;
                *old.decor_mut() = decor;
            }
            None => {
                table.insert(leaf, toml_edit::Item::Value(new));
            }
        }
    }
    let text = doc.to_string();
    let check: toml::Table = toml::from_str(&text).context("the edited file does not parse")?;
    for (key, value) in values {
        let (sec, leaf) = key.split_once('.').unwrap_or_default();
        if check.get(sec).and_then(|s| s.get(leaf)) != Some(value) {
            bail!("{key} did not round-trip through the edited file");
        }
    }
    let backup = backup(path)?;
    atomic_write(path, text.as_bytes())?;
    Ok(backup)
}

fn to_edit(v: &toml::Value) -> Result<toml_edit::Value> {
    Ok(match v {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Float(f) => (*f).into(),
        other => bail!("unsupported value {other}"),
    })
}

fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut candidate = PathBuf::from(format!("{}.bak", path.display()));
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}.bak.{n}", path.display()));
        n += 1;
    }
    std::fs::copy(path, &candidate).with_context(|| format!("backing up to {}", candidate.display()))?;
    Ok(Some(candidate))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_in_place_keeping_comments_and_backs_up() {
        let dir = std::env::temp_dir().join(format!("mobius-thr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original =
            "# my settings\n[profit]\nmin_profit_lamports = 10000   # ≈ 2× base fee\nprotect_min_out = true\n";
        std::fs::write(&path, original).unwrap();
        let bak = write(
            &path,
            &[
                ("profit.min_profit_lamports".into(), toml::Value::Integer(0)),
                ("risk.max_daily_loss_usd".into(), toml::Value::String("0.5".into())),
            ],
        )
        .unwrap()
        .unwrap();
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.contains("# my settings"), "{now}");
        assert!(now.contains("min_profit_lamports = 0   # ≈ 2× base fee"), "comment kept: {now}");
        assert!(now.contains("[risk]") && now.contains("max_daily_loss_usd = \"0.5\""), "{now}");
        assert_eq!(std::fs::read_to_string(bak).unwrap(), original);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

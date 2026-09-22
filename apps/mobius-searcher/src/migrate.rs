//! `--migrate-config`: move the Solana sections of your config file from the
//! top level (`[rpc]`, `[jupiter]`, `[jito]`, `[feeds]`, `[wallet]`) under
//! `[venues.solana]`. Nothing changes unless you confirm; the file must load
//! to exactly the same effective configuration before and after; the old
//! file is kept as `.bak`. Both layouts are read until v0.4.

use anyhow::{Context, Result, bail};
use searcher_core::config::{SOLANA_SECTIONS, load_layered};
use std::path::Path;

/// The migrated text and the sections moved, or `None` when there is
/// nothing at the top level to move.
pub fn plan(text: &str) -> Result<Option<(String, Vec<String>)>> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("parsing the config file")?;
    let mut moved = Vec::new();
    let mut items = Vec::new();
    for sec in SOLANA_SECTIONS {
        if let Some(item) = doc.remove(sec) {
            if !item.is_table() {
                bail!("`{sec}` is not a table");
            }
            moved.push(sec.to_string());
            items.push((sec, item));
        }
    }
    if moved.is_empty() {
        return Ok(None);
    }
    if !doc.contains_key("venues") {
        let mut v = toml_edit::Table::new();
        v.set_implicit(true);
        doc.insert("venues", toml_edit::Item::Table(v));
    }
    let venues = doc["venues"].as_table_mut().context("[venues] is not a table")?;
    if !venues.contains_key("solana") {
        let mut s = toml_edit::Table::new();
        s.set_implicit(true);
        venues.insert("solana", toml_edit::Item::Table(s));
    }
    let solana = venues["solana"].as_table_mut().context("[venues.solana] is not a table")?;
    for (sec, item) in items {
        if solana.contains_key(sec) {
            bail!("[{sec}] is both at the top level and under [venues.solana]; merge them by hand first");
        }
        solana.insert(sec, item);
    }
    Ok(Some((doc.to_string(), moved)))
}

/// Migrate `user` (layered over `repo`). `confirm` sees the new file and the
/// moved sections and says whether to write it.
pub fn run(repo: &Path, user: &Path, confirm: impl FnOnce(&str, &[String]) -> bool) -> Result<String> {
    if !user.exists() {
        return Ok(format!("{} does not exist: nothing to migrate", user.display()));
    }
    let text = std::fs::read_to_string(user).with_context(|| format!("reading {}", user.display()))?;
    let Some((new_text, moved)) = plan(&text)? else {
        return Ok(format!("{} already keeps the Solana settings under [venues.solana]", user.display()));
    };
    // the migrated file must mean exactly the same thing
    let tmp = user.with_extension(format!("migrate-{}.toml", std::process::id()));
    std::fs::write(&tmp, &new_text)?;
    let same = (|| -> Result<bool> {
        let before = load_layered(repo, user).map_err(|e| anyhow::anyhow!("{e}"))?;
        let after = load_layered(repo, &tmp).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(toml::Value::try_from(&before.config)? == toml::Value::try_from(&after.config)?)
    })();
    let _ = std::fs::remove_file(&tmp);
    if !same? {
        bail!("the migrated file would load differently; nothing was changed");
    }
    if !confirm(&new_text, &moved) {
        return Ok("not changed".into());
    }
    let mut bak = std::path::PathBuf::from(format!("{}.bak", user.display()));
    let mut n = 1;
    while bak.exists() {
        bak = std::path::PathBuf::from(format!("{}.bak.{n}", user.display()));
        n += 1;
    }
    std::fs::copy(user, &bak).with_context(|| format!("backing up to {}", bak.display()))?;
    let part = user.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&part, &new_text)?;
    std::fs::rename(&part, user)?;
    load_layered(repo, user).map_err(|e| anyhow::anyhow!("reloading after the migration: {e}"))?;
    Ok(format!(
        "moved {} under [venues.solana] in {} (previous file: {}); the effective configuration is unchanged",
        moved.iter().map(|s| format!("[{s}]")).collect::<Vec<_>>().join(" "),
        user.display(),
        bak.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "\
# MØBIUS-Searcher: your settings
[general]
mode = \"paper\"

[jupiter]
for_jito_bundle = false
# tighter than rtse
slippage = \"10\"

[profit]
min_profit_lamports = 10000   # ≈ 2× base fee

[wallet]
pubkey = \"F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe\"
";

    #[test]
    fn moves_the_solana_sections_keeping_comments_and_meaning() {
        let dir = std::env::temp_dir().join(format!("mobius-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("config.toml");
        std::fs::write(&user, USER).unwrap();
        let repo = dir.join("absent-repo.toml");

        // declined: nothing changes
        assert_eq!(run(&repo, &user, |_, _| false).unwrap(), "not changed");
        assert_eq!(std::fs::read_to_string(&user).unwrap(), USER);

        let mut shown = String::new();
        let msg = run(&repo, &user, |t, moved| {
            shown = t.to_string();
            assert_eq!(moved, ["jupiter", "wallet"]);
            true
        })
        .unwrap();
        assert!(msg.contains("unchanged"), "{msg}");
        let now = std::fs::read_to_string(&user).unwrap();
        assert_eq!(now, shown);
        assert!(now.contains("[venues.solana.jupiter]") && now.contains("[venues.solana.wallet]"), "{now}");
        assert!(now.contains("# tighter than rtse") && now.contains("# ≈ 2× base fee"), "comments kept:\n{now}");
        assert!(!now.contains("\n[jupiter]"), "{now}");
        assert_eq!(std::fs::read_to_string(dir.join("config.toml.bak")).unwrap(), USER);
        // a second run finds nothing to do
        assert!(run(&repo, &user, |_, _| true).unwrap().contains("already"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

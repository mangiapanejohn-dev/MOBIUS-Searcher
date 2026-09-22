//! The thresholds an operator may change while the bot runs (TUI panel `T`).
//! A change is validated on a copy of the configuration before anything uses
//! it; loosening into possible losses needs an explicit acknowledgement.

use crate::config::{Config, ConfigError};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    UInt,
    /// Decimal string (`"0.002"`).
    Decimal,
    Bool,
    /// Free text (`jupiter.slippage`: `"rtse"` or bps).
    Text,
}

#[derive(Copy, Clone, Debug)]
pub struct Threshold {
    /// Dotted config path (`profit.min_profit_lamports`).
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

pub const THRESHOLDS: &[Threshold] = &[
    Threshold {
        key: "profit.min_profit_lamports",
        label: "Min profit (lamports)",
        kind: Kind::Int,
        help: "net after all costs; also the on-chain min-out margin (negative = accept a loss)",
    },
    Threshold {
        key: "profit.min_profit_bps",
        label: "Min profit (bp)",
        kind: Kind::Int,
        help: "net edge of the input",
    },
    Threshold { key: "profit.min_profit_usd", label: "Min profit (USD)", kind: Kind::Decimal, help: "net in dollars" },
    Threshold {
        key: "profit.protect_min_out",
        label: "On-chain min-out",
        kind: Kind::Bool,
        help: "the final leg reverts unless it returns input + costs + min profit",
    },
    Threshold {
        key: "profit.expected_slippage_share_bps",
        label: "Slippage reserve (bp of tolerance)",
        kind: Kind::UInt,
        help: "share of the slippage tolerance charged as a cost (1500 = 15 %)",
    },
    Threshold {
        key: "profit.safety_buffer_lamports",
        label: "Safety buffer (lamports)",
        kind: Kind::UInt,
        help: "fixed amount subtracted from every candidate",
    },
    Threshold { key: "profit.safety_buffer_bps", label: "Safety buffer (bp)", kind: Kind::Int, help: "of the input" },
    Threshold {
        key: "profit.max_new_deposit_lamports",
        label: "Max deposit per trade",
        kind: Kind::UInt,
        help: "rent a trade may lock in accounts it leaves created (capital)",
    },
    Threshold {
        key: "jupiter.slippage",
        label: "Slippage tolerance",
        kind: Kind::Text,
        help: "\"rtse\" (Jupiter's estimate) or bps; also bounds the inventory a leg may draw",
    },
    Threshold {
        key: "risk.max_trade_lamports",
        label: "Max trade size (lamports)",
        kind: Kind::UInt,
        help: "input per trade",
    },
    Threshold {
        key: "risk.max_daily_loss_usd",
        label: "Max daily loss (USD)",
        kind: Kind::Decimal,
        help: "trading stops for the UTC day beyond it",
    },
];

pub fn find(key: &str) -> Option<&'static Threshold> {
    THRESHOLDS.iter().find(|t| t.key == key)
}

fn table(cfg: &Config) -> Result<toml::Table, String> {
    match toml::Value::try_from(cfg).map_err(|e| e.to_string())? {
        toml::Value::Table(t) => Ok(t),
        _ => Err("config did not serialize to a table".into()),
    }
}

fn get<'a>(t: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let (sec, leaf) = key.split_once('.')?;
    t.get(sec)?.as_table()?.get(leaf)
}

/// Current value of every threshold, as text (strings unquoted).
pub fn values(cfg: &Config) -> Vec<(String, String)> {
    let t = table(cfg).unwrap_or_default();
    THRESHOLDS
        .iter()
        .map(|th| {
            let v = match get(&t, th.key) {
                Some(toml::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            (th.key.to_string(), v)
        })
        .collect()
}

/// Parse operator input for `th` into a TOML value.
pub fn parse(th: &Threshold, input: &str) -> Result<toml::Value, String> {
    let s = input.trim();
    match th.kind {
        Kind::Int => s.parse::<i64>().map(toml::Value::Integer).map_err(|_| format!("{}: whole number", th.label)),
        Kind::UInt => s
            .parse::<u64>()
            .ok()
            .and_then(|v| i64::try_from(v).ok())
            .map(toml::Value::Integer)
            .ok_or_else(|| format!("{}: whole number ≥ 0", th.label)),
        Kind::Decimal => crate::units::parse_decimal(s, 6)
            .map(|_| toml::Value::String(s.to_string()))
            .map_err(|e| format!("{}: {e}", th.label)),
        Kind::Bool => match s {
            "true" | "on" | "yes" => Ok(toml::Value::Boolean(true)),
            "false" | "off" | "no" => Ok(toml::Value::Boolean(false)),
            _ => Err(format!("{}: true or false", th.label)),
        },
        Kind::Text => Ok(toml::Value::String(s.to_string())),
    }
}

/// Whether a landed trade could lose money under `cfg`: without the on-chain
/// min-out, or with a minimum profit below zero.
pub fn loss_possible(cfg: &Config) -> bool {
    !cfg.profit.protect_min_out || cfg.profit.min_profit_lamports < 0
}

/// A validated change: the new configuration and the parsed values.
#[derive(Clone, Debug)]
pub struct Change {
    pub config: Config,
    pub values: Vec<(String, toml::Value)>,
    /// `(key, old, new)` as text, for the log and the review screen.
    pub diff: Vec<(String, String, String)>,
    /// The change opens losses the current settings do not allow.
    pub opens_loss: bool,
}

/// Apply `changes` (key → operator text) to a copy of `cfg` and validate it.
pub fn apply(cfg: &Config, changes: &[(String, String)]) -> Result<Change, String> {
    let before: std::collections::BTreeMap<String, String> = values(cfg).into_iter().collect();
    let mut t = table(cfg)?;
    let mut parsed = Vec::new();
    let mut diff = Vec::new();
    for (key, text) in changes {
        let th = find(key).ok_or_else(|| format!("{key} cannot be changed here"))?;
        let v = parse(th, text)?;
        let (sec, leaf) = key.split_once('.').ok_or("bad key")?;
        t.get_mut(sec)
            .and_then(|s| s.as_table_mut())
            .ok_or_else(|| format!("no [{sec}] section"))?
            .insert(leaf.to_string(), v.clone());
        let new_text = match &v {
            toml::Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        if before.get(key) != Some(&new_text) {
            diff.push((key.clone(), before.get(key).cloned().unwrap_or_default(), new_text));
        }
        parsed.push((key.clone(), v));
    }
    let config: Config = toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| e.to_string())?;
    config.validate().map_err(|e: ConfigError| e.to_string())?;
    let opens_loss = loss_possible(&config) && !loss_possible(cfg);
    Ok(Change { config, values: parsed, diff, opens_loss })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_threshold_exists_in_the_config() {
        let v = values(&Config::default());
        assert_eq!(v.len(), THRESHOLDS.len());
        for (k, val) in v {
            assert!(!val.is_empty(), "{k} has no value");
        }
    }

    #[test]
    fn a_valid_change_is_applied_to_a_copy_and_diffed() {
        let cfg = Config::default();
        let c = apply(&cfg, &[("profit.safety_buffer_lamports".into(), "0".into())]).unwrap();
        assert_eq!(c.config.profit.safety_buffer_lamports, 0);
        assert_eq!(cfg.profit.safety_buffer_lamports, 5_000, "original untouched");
        assert_eq!(c.diff, vec![("profit.safety_buffer_lamports".into(), "5000".into(), "0".into())]);
        assert!(!c.opens_loss, "a smaller buffer keeps the on-chain floor at break-even");
    }

    #[test]
    fn loosening_into_losses_is_flagged() {
        let cfg = Config::default();
        assert!(apply(&cfg, &[("profit.protect_min_out".into(), "false".into())]).unwrap().opens_loss);
        assert!(apply(&cfg, &[("profit.min_profit_lamports".into(), "-5000".into())]).unwrap().opens_loss);
        let already = apply(&cfg, &[("profit.protect_min_out".into(), "false".into())]).unwrap().config;
        assert!(!apply(&already, &[("profit.min_profit_bps".into(), "0".into())]).unwrap().opens_loss);
    }

    #[test]
    fn bad_input_is_refused_with_the_reason() {
        let cfg = Config::default();
        assert!(apply(&cfg, &[("profit.max_new_deposit_lamports".into(), "-1".into())]).is_err());
        assert!(apply(&cfg, &[("profit.min_profit_usd".into(), "abc".into())]).is_err());
        assert!(apply(&cfg, &[("jupiter.slippage".into(), "fast".into())]).unwrap_err().contains("slippage"));
        assert!(apply(&cfg, &[("general.mode".into(), "live".into())]).unwrap_err().contains("cannot be changed"));
    }
}

//! Proxy discovery shared by every network client. A terminal that does not
//! export `HTTPS_PROXY` (Ghostty, the Claude app's terminal panel) still gets
//! the macOS system proxy: reqwest's `system-proxy` feature did not pick it
//! up on this machine, so clients connected directly and timed out.
//!
//! `[network] proxy` in the config selects the behaviour once at startup
//! ([`configure`]): `auto` (above), `none` (always direct, environment
//! ignored) or an explicit `http://host:port`.

use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxySetting {
    /// Proxy environment variables, else the OS proxy settings.
    Auto,
    /// Always connect directly.
    Direct,
    /// This HTTP CONNECT proxy for everything (`http://host:port`).
    Url(String),
}

impl ProxySetting {
    /// `auto` | `none` | `http://host:port`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "" | "auto" => Ok(Self::Auto),
            "none" | "direct" => Ok(Self::Direct),
            u if u.starts_with("http://") && ws_target(&u.replacen("http://", "ws://", 1)).is_some() => {
                Ok(Self::Url(u.trim_end_matches('/').to_string()))
            }
            other => Err(format!("network.proxy must be \"auto\", \"none\" or http://host:port, got `{other}`")),
        }
    }
}

static SETTING: OnceLock<ProxySetting> = OnceLock::new();

/// Set the process-wide proxy behaviour (first call wins; before any client is built).
pub fn configure(setting: ProxySetting) {
    let _ = SETTING.set(setting);
}

pub fn setting() -> &'static ProxySetting {
    SETTING.get().unwrap_or(&ProxySetting::Auto)
}

/// HTTP clients must call `ClientBuilder::no_proxy()` (ignore the environment).
pub fn direct() -> bool {
    *setting() == ProxySetting::Direct
}

/// `(host, port)` of a ws/wss URL.
pub fn ws_target(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = if scheme.eq_ignore_ascii_case("wss") { 443 } else { 80 };
    let authority = rest.split(['/', '?']).next()?;
    let authority = authority.rsplit('@').next()?;
    match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((authority.to_string(), default)),
    }
}

/// HTTP(S) proxy for a target, from the standard env vars (honours NO_PROXY),
/// else the OS proxy settings (as reqwest does for HTTP).
pub fn proxy_for(url: &str) -> Option<(String, u16)> {
    let (host, _) = ws_target(url)?;
    let explicit = match setting() {
        ProxySetting::Direct => return None,
        ProxySetting::Url(p) => Some(p.clone()),
        ProxySetting::Auto => None,
    };
    let no_proxy = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).unwrap_or_default();
    if no_proxy.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|n| {
        n == "*" || host == n.trim_start_matches('.') || host.ends_with(&format!(".{}", n.trim_start_matches('.')))
    }) {
        return None;
    }
    if let Some(p) = explicit {
        return ws_target(&p.replacen("http://", "ws://", 1));
    }
    let keys: &[&str] = if url.starts_with("wss") {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    let Some(p) = keys.iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())) else {
        return system_proxy(url.starts_with("wss"));
    };
    if !p.starts_with("http://") {
        return None; // only plain HTTP CONNECT proxies are supported
    }
    let (h, port) = ws_target(&p.replacen("http://", "ws://", 1))?;
    Some((h, port))
}

/// macOS system proxy (System Settings → Network → Proxies). A terminal that
/// does not export HTTPS_PROXY still gets it, like reqwest's HTTP clients do.
#[cfg(target_os = "macos")]
pub fn system_proxy(secure: bool) -> Option<(String, u16)> {
    let out = std::process::Command::new("scutil").arg("--proxy").output().ok()?;
    parse_scutil_proxy(&String::from_utf8_lossy(&out.stdout), secure)
}

#[cfg(not(target_os = "macos"))]
pub fn system_proxy(_secure: bool) -> Option<(String, u16)> {
    None
}

/// `(host, port)` of the enabled HTTP or HTTPS proxy in `scutil --proxy` output.
pub fn parse_scutil_proxy(s: &str, secure: bool) -> Option<(String, u16)> {
    let get = |key: String| {
        s.lines().find_map(|l| {
            let (k, v) = l.split_once(" : ")?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let p = if secure { "HTTPS" } else { "HTTP" };
    if get(format!("{p}Enable"))? != "1" {
        return None;
    }
    Some((get(format!("{p}Proxy"))?, get(format!("{p}Port"))?.parse().ok()?))
}

/// Proxy to set with `reqwest::ClientBuilder::proxy`: the configured one, or
/// (`auto`) the macOS system proxy for HTTPS when no proxy environment
/// variable is set (those are honoured by reqwest itself, including
/// `NO_PROXY`). With `none`, callers also call [`direct`].
pub fn fallback_https_proxy() -> Option<String> {
    match setting() {
        ProxySetting::Direct => return None,
        ProxySetting::Url(p) => return Some(p.clone()),
        ProxySetting::Auto => {}
    }
    let env_set = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
    if env_set {
        return None;
    }
    system_proxy(true).map(|(h, p)| format!("http://{h}:{p}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_setting_parses_the_three_forms() {
        assert_eq!(ProxySetting::parse("auto"), Ok(ProxySetting::Auto));
        assert_eq!(ProxySetting::parse(""), Ok(ProxySetting::Auto));
        assert_eq!(ProxySetting::parse("none"), Ok(ProxySetting::Direct));
        assert_eq!(
            ProxySetting::parse("http://127.0.0.1:7890/"),
            Ok(ProxySetting::Url("http://127.0.0.1:7890".into()))
        );
        assert!(ProxySetting::parse("socks5://127.0.0.1:1080").is_err());
        assert!(ProxySetting::parse("yes").is_err());
    }
}

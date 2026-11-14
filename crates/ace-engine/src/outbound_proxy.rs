//! Startup handling for the optional outbound SOCKS5 proxy (`OUTPACE_PROXY`,
//! `OUTPACE_PROXY_MODE`). The dialer itself lives in `ace_swarm::proxy`.

use crate::config::Config;
use ace_swarm::proxy::ProxySettings;

/// Read both variables. Values never appear in errors: the URL may carry a password.
pub fn settings_from_env() -> Result<ProxySettings, Box<dyn std::error::Error>> {
    let read = |name: &'static str| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid UTF-8")),
    };
    let url = read("OUTPACE_PROXY")?;
    let mode = read("OUTPACE_PROXY_MODE")?;
    Ok(ProxySettings::from_values(url.as_deref(), mode.as_deref())?)
}

/// With `OUTPACE_PROXY_MODE=on` nobody can reach the engine through the proxy, so inbound
/// serving and port mapping default off. An explicit `=1` for either is a conflict.
pub fn normalize_proxy_env(
    config: &mut Config,
    inbound: Option<bool>,
    mapping: Option<bool>,
) -> Result<(), &'static str> {
    if !config.proxy.always_proxied() {
        return Ok(());
    }
    if inbound == Some(true) {
        return Err("OUTPACE_PROXY_MODE=on conflicts with OUTPACE_ENABLE_INBOUND=1");
    }
    if mapping == Some(true) {
        return Err("OUTPACE_PROXY_MODE=on conflicts with OUTPACE_ENABLE_PORT_MAPPING=1");
    }
    config.enable_inbound = false;
    config.enable_port_mapping = false;
    Ok(())
}

/// Final check for every caller, including configs built directly in Rust.
pub fn validate_proxy_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    config.proxy.validate()?;
    if config.proxy.always_proxied() {
        if config.enable_inbound {
            return Err("OUTPACE_PROXY_MODE=on requires inbound serving off".into());
        }
        if config.enable_port_mapping {
            return Err("OUTPACE_PROXY_MODE=on requires port mapping off".into());
        }
    }
    Ok(())
}

/// Public status: mode and current path only, never the endpoint or credentials.
pub fn proxy_status() -> serde_json::Value {
    let active = ace_swarm::proxy::active().is_some();
    serde_json::json!({
        "mode": ace_swarm::proxy::mode().as_str(),
        "path": ace_swarm::proxy::path(),
        "udp_trackers": ace_swarm::proxy::udp_trackers_enabled(),
        "dht": !active,
        "inbound_advertised": !active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxied(mode: &str) -> Config {
        Config {
            proxy: ProxySettings::from_values(Some("socks5://127.0.0.1:1080"), Some(mode)).unwrap(),
            ..Config::default()
        }
    }

    #[test]
    fn mode_on_turns_inbound_and_mapping_off_and_rejects_explicit_conflicts() {
        let mut config = proxied("on");
        config.enable_port_mapping = true;
        assert!(validate_proxy_config(&config).is_err());
        normalize_proxy_env(&mut config, None, None).unwrap();
        assert!(!config.enable_inbound && !config.enable_port_mapping);
        validate_proxy_config(&config).unwrap();
        assert!(normalize_proxy_env(&mut proxied("on"), Some(true), None).is_err());
        assert!(normalize_proxy_env(&mut proxied("on"), None, Some(true)).is_err());
        normalize_proxy_env(&mut proxied("on"), Some(false), Some(false)).unwrap();
    }

    #[test]
    fn fallback_and_off_keep_inbound_defaults() {
        for mode in ["fallback", "off"] {
            let mut config = proxied(mode);
            normalize_proxy_env(&mut config, Some(true), Some(true)).unwrap();
            assert!(config.enable_inbound);
            validate_proxy_config(&config).unwrap();
        }
        let missing = Config {
            proxy: ProxySettings {
                mode: ace_swarm::proxy::ProxyMode::On,
                endpoint: None,
            },
            ..Config::default()
        };
        assert!(validate_proxy_config(&missing).is_err());
    }

    #[test]
    fn serialized_config_never_carries_proxy_credentials() {
        let config = Config {
            proxy: ProxySettings::from_values(
                Some("socks5://user:hunter2@127.0.0.1:1080"),
                Some("fallback"),
            )
            .unwrap(),
            ..Config::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let debug = format!("{config:?}");
        for text in [json, debug] {
            assert!(!text.contains("hunter2") && !text.contains("user:"));
        }
    }

    #[test]
    fn status_reports_direct_when_no_proxy_is_installed() {
        let status = proxy_status();
        assert_eq!(status["mode"], "off");
        assert_eq!(status["path"], "direct");
        assert_eq!(status["dht"], true);
    }
}

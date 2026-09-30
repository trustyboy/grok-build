use crate::util::config::RemoteSettings;
use toml::Value as TomlValue;

/// Resolve whether ZDR users are allowed to use the product.
///
/// Precedence: requirements > env > config.toml > managed > remote settings > default (false).
pub fn resolve_zdr_access_enabled(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
    managed: Option<&TomlValue>,
    remote: Option<&RemoteSettings>,
) -> bool {
    use crate::agent::config::BoolFlag;
    fn from_toml(v: Option<&TomlValue>) -> Option<bool> {
        v?.get("features")?.get("zdr_access_enabled")?.as_bool()
    }
    BoolFlag::env("GROK_ZDR_ACCESS_ENABLED")
        .requirement(from_toml(requirements))
        .config(from_toml(user))
        .managed(from_toml(managed))
        .feature_flag(remote.and_then(|r| r.zdr_access_enabled))
        .resolve()
        .value
}

pub(crate) fn turn_transient_retry_from_toml(v: Option<&TomlValue>) -> Option<bool> {
    v?.get("features")?.get("turn_transient_retry")?.as_bool()
}

/// Spawn-time kill switch: `[features] turn_transient_retry`, `GROK_TURN_TRANSIENT_RETRY`, remote key (below local), default on.
/// Loads local layers itself; call once per session.
pub(crate) fn resolve_turn_transient_retry(remote: Option<bool>) -> bool {
    let user_cfg = crate::config::load_effective_config().ok();
    // Merged (user, system, MDM last-wins) like the sibling resolvers, so an enterprise pin is not silently dropped
    let requirements = crate::config::load_merged_requirements();
    compose_turn_transient_retry(requirements.as_ref(), user_cfg.as_ref(), remote)
}

fn compose_turn_transient_retry(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
    remote: Option<bool>,
) -> bool {
    use turn_transient_retry_from_toml as from_toml;
    crate::agent::config::resolve_turn_transient_retry(
        from_toml(requirements),
        /* cli          */ None,
        from_toml(user),
        /* managed: merged into the config layer by load_effective_config */ None,
        remote,
    )
    .value
}

pub use xai_grok_config::resolve_remote_fetch_enabled;

/// 显式查询用户上游不启动后台获取，但仍服从管理层的禁止联网策略。
pub(crate) fn explicit_model_discovery_allowed() -> bool {
    if !xai_grok_config::Distribution::current().allows(xai_grok_config::Capability::RemoteFetch) {
        return false;
    }
    let Ok(layers) = crate::config::ConfigLayers::load() else {
        return false;
    };
    explicit_model_discovery_from_layers(&layers)
}

fn explicit_model_discovery_from_layers(layers: &crate::config::ConfigLayers) -> bool {
    [
        layers.mdm_requirements.as_ref(),
        layers.system_requirements.as_ref(),
        layers.user_requirements.as_ref(),
        Some(&layers.managed),
        Some(&layers.system_managed),
    ]
    .into_iter()
    .flatten()
    .find_map(remote_fetch_value)
    .unwrap_or(true)
}

pub const REMOTE_FETCH_CONFIG_PATH: &str = "features.remote_fetch";

/// Keys whose dedicated resolver walks managed before user `config.toml`.
/// The effective merge lets the user file win for every other key.
pub const MANAGED_WINS_OVER_USER: &[&str] = &[REMOTE_FETCH_CONFIG_PATH];

fn remote_fetch_value(v: &TomlValue) -> Option<bool> {
    v.get("features")?.get("remote_fetch")?.as_bool()
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn explicit_discovery_respects_management_without_enabling_background_fetch() {
        let policy = |value: bool| -> TomlValue {
            toml::from_str(&format!("[features]\nremote_fetch = {value}\n")).unwrap()
        };
        let mut layers = crate::config::ConfigLayers::default();
        layers.user = policy(false);
        assert!(explicit_model_discovery_from_layers(&layers));
        layers.managed = policy(false);
        assert!(!explicit_model_discovery_from_layers(&layers));
        layers.user_requirements = Some(policy(true));
        assert!(explicit_model_discovery_from_layers(&layers));
        layers.mdm_requirements = Some(policy(false));
        assert!(!explicit_model_discovery_from_layers(&layers));
        layers.env_overlay = Some(policy(true));
        assert!(!explicit_model_discovery_from_layers(&layers));
    }
}

#[cfg(test)]
mod turn_transient_retry_toml_tests {
    use super::turn_transient_retry_from_toml;

    #[test]
    fn composition_orders_requirements_over_config_over_remote() {
        let t = |b: bool| -> toml::Value {
            let mut f = toml::map::Map::new();
            f.insert("turn_transient_retry".into(), toml::Value::Boolean(b));
            let mut root = toml::map::Map::new();
            root.insert("features".into(), toml::Value::Table(f));
            toml::Value::Table(root)
        };
        // requirements beat user config; config beats remote; remote beats default.
        assert!(!super::compose_turn_transient_retry(
            Some(&t(false)),
            Some(&t(true)),
            None
        ));
        assert!(!super::compose_turn_transient_retry(
            None,
            Some(&t(false)),
            Some(true)
        ));
        assert!(!super::compose_turn_transient_retry(
            None,
            None,
            Some(false)
        ));
        assert!(super::compose_turn_transient_retry(None, None, None));
    }

    #[test]
    fn reads_the_features_key_and_defaults_absent_to_none() {
        let v: toml::Value = toml::toml! { [features] turn_transient_retry = false }.into();
        assert_eq!(turn_transient_retry_from_toml(Some(&v)), Some(false));
        let empty: toml::Value = toml::toml! { [features] }.into();
        assert_eq!(turn_transient_retry_from_toml(Some(&empty)), None);
        assert_eq!(turn_transient_retry_from_toml(None), None);
    }
}

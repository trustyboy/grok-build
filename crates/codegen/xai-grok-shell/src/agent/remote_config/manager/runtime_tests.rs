use super::*;
use crate::agent::auth_method::ModelByok;

/// 每个测试使用不同目标 ID，避免进程级兼容入口互相干扰。
fn fixture(extra: &str) -> (ModelsManager, config::Config) {
    let raw: toml::Value = toml::from_str(&format!(
        "[model.source]\nmodel = 'source-wire'\nbase_url = 'https://runtime.example/v1'\napi_key = 'source-key'\n{extra}"
    )).unwrap();
    let cfg = config::Config::new_from_toml_cfg(&raw).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(temp.path(), GrokComConfig::default()));
    let manager = ModelsManager::new(
        None,
        resolve_model_catalog(&cfg, None),
        acp::ModelId::new("source"),
        auth,
        cfg.clone(),
    );
    (manager, cfg)
}

#[test]
fn runtime_defaults_do_not_inherit_capabilities() {
    let (manager, _) = fixture(
        "supports_vision = true\nsupports_reasoning_effort = true\nreasoning_effort = 'high'\nsupports_backend_search = true\ncontext_window = 999999\nuse_concise = true\n",
    );
    {
        let mut catalog = manager.inner.catalog.write();
        let source = catalog.models.get_mut("source").unwrap();
        source.info.variants.push(config::ModelVariant {
            effort: ReasoningEffort::High,
            model_id: "source-thinking-variant".to_owned(),
        });
        source
            .info
            .extra_headers
            .insert("x-route".into(), "source-route".into());
        source
            .info
            .query_params
            .insert("region".into(), "test-region".into());
        source
            .info
            .env_http_headers
            .insert("x-runtime-header".into(), "RUNTIME_TEST_HEADER".into());
        source.mtls_cert_dir = Some(std::path::PathBuf::from("runtime-test-cert"));
    }
    let id = "runtime-conservative";
    manager.register_runtime_model("source", id).unwrap();
    let entry = manager.runtime_model_entry(id).unwrap().unwrap();
    assert!(!entry.info.supports_vision);
    assert_eq!(entry.info.context_window.get(), 256_000);
    assert_eq!(entry.info.reasoning_effort, None);
    assert_eq!(entry.info.reasoning_summary, None);
    assert!(entry.info.context_windows.is_empty());
    assert_eq!(entry.info.max_request_bytes, None);
    assert!(!entry.info.supports_reasoning_effort);
    assert!(entry.info.reasoning_efforts.is_empty());
    assert!(entry.info.variants.is_empty());
    assert!(!entry.info.supports_backend_search);
    assert!(!entry.info.use_concise);
    assert_eq!(entry.info.model_family, None);
    assert_eq!(entry.info.max_completion_tokens, None);
    assert_eq!(entry.info.base_url, "https://runtime.example/v1");
    assert_eq!(entry.api_key.as_deref(), Some("source-key"));
    assert_eq!(entry.info.extra_headers["x-route"], "source-route");
    assert_eq!(entry.info.query_params["region"], "test-region");
    assert_eq!(
        entry.info.env_http_headers["x-runtime-header"],
        "RUNTIME_TEST_HEADER"
    );
    assert_eq!(
        entry.mtls_cert_dir,
        Some(std::path::PathBuf::from("runtime-test-cert"))
    );
    let available = manager.available();
    let meta = available
        .get(&acp::ModelId::new(id))
        .unwrap()
        .meta
        .as_ref()
        .unwrap();
    assert_eq!(
        meta.get("runtimeDiscovered"),
        Some(&serde_json::json!(true))
    );
    assert!(!manager.model_supports_vision(id));
    assert_eq!(manager.current_model_id().0.as_ref(), "source");
    assert!(manager.inner.cfg.read().models.default.is_none());
}

#[test]
fn runtime_task_projection_tracks_valid_sources_and_metadata() {
    let (manager, mut cfg) = fixture("");
    let id = "runtime-task-projection";
    manager.register_runtime_model("source", id).unwrap();
    let stale_models = manager.models();
    let snapshot = manager.task_model_catalog_snapshot(false);
    assert_eq!(snapshot.authority, CatalogAuthority::Complete);
    assert!(
        snapshot
            .eligible
            .iter()
            .any(|entry| entry.id == id && entry.model_family.is_none())
    );
    assert!(manager.task_model_error(id).is_none());
    assert_eq!(manager.catalog_key(id), Some(acp::ModelId::new(id)));
    assert!(manager.available().contains_key(&acp::ModelId::new(id)));

    cfg.models.hidden_models = Some(vec![id.to_owned()]);
    manager.apply_config(cfg);
    assert!(manager.runtime_model_entry(id).is_err());
    assert!(manager.task_model_error(id).is_some());
    assert!(manager.catalog_key(id).is_none());
    assert!(!manager.model_in_catalog(id));
    assert!(!manager.available().contains_key(&acp::ModelId::new(id)));
    assert!(
        !manager
            .task_model_catalog_snapshot(false)
            .eligible
            .iter()
            .any(|entry| entry.id == id)
    );
    let stale_acp = config::to_acp_model_info(&stale_models);
    let meta = stale_acp
        .get(&acp::ModelId::new(id))
        .unwrap()
        .meta
        .as_ref()
        .unwrap();
    assert_eq!(
        meta.get("runtimeDiscovered"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        meta.get("totalContextTokens"),
        Some(&serde_json::json!(256_000))
    );
}

#[test]
fn runtime_survives_rebuild_and_follows_current_source() {
    let (manager, mut cfg) = fixture("");
    let id = "runtime-rebuild";
    manager.register_runtime_model("source", id).unwrap();
    manager.set_current_model_id(acp::ModelId::new(id));
    cfg.config_models.get_mut("source").unwrap().api_key = Some("rotated-key".into());
    cfg.config_models.get_mut("source").unwrap().base_url =
        Some("https://rotated.example/v1".into());
    let old_generation = manager.inner.catalog.read().generation;
    manager.apply_config(cfg.clone());
    assert!(!manager.apply_catalog_fenced(&cfg, IndexMap::new(), None, Some(old_generation)));
    manager.rebuild(&cfg, None);
    manager.apply_catalog(&cfg, IndexMap::new(), None);
    let entry = manager.runtime_model_entry(id).unwrap().unwrap();
    assert_eq!(entry.api_key.as_deref(), Some("rotated-key"));
    assert_eq!(entry.info.base_url, "https://rotated.example/v1");
    assert_eq!(manager.current_model_id().0.as_ref(), id);
    assert!(manager.models().contains_key(id));
    assert!(manager.prefetched().unwrap().is_empty());
    assert!(config::find_model_by_id(&manager.models(), id).is_some());
    assert!(manager.model_in_catalog(id));
    manager.clear();
    assert!(manager.runtime_model_entry(id).is_err());
    manager.rebuild(&cfg, None);
    assert!(manager.runtime_model_entry(id).unwrap().is_some());
}

#[test]
fn runtime_source_removal_and_auth_removal_fail_closed() {
    let (manager, mut cfg) = fixture("");
    let id = "runtime-removed";
    manager.register_runtime_model("source", id).unwrap();
    manager.set_current_model_id(acp::ModelId::new(id));
    cfg.config_models.get_mut("source").unwrap().api_key = None;
    manager.apply_config(cfg.clone());
    assert!(
        manager
            .runtime_model_entry(id)
            .unwrap_err()
            .contains("认证已删除")
    );
    cfg.config_models.shift_remove("source");
    manager.apply_config(cfg);
    assert!(
        manager
            .runtime_model_entry(id)
            .unwrap_err()
            .contains("已删除")
    );
    assert!(!manager.available().contains_key(&acp::ModelId::new(id)));
    assert_eq!(manager.current_model_id().0.as_ref(), id);
    assert!(manager.sampling_config().api_key.is_none());
    assert!(manager.sampling_config().base_url.is_empty());
    assert!(config::try_resolve_model_credentials(id, Some("session-secret")).is_none());
    assert_eq!(
        config::resolve_model_auth_facts_and_provider(id).0.byok,
        ModelByok::Byok
    );
    assert!(
        manager
            .runtime_model_entry("never-discovered")
            .unwrap()
            .is_none()
    );
}

#[test]
fn runtime_registration_and_reload_enforce_filters() {
    for (index, (section, value)) in [
        ("disabled_models", "['runtime-filter-*']"),
        ("hidden_models", "['runtime-filter-*']"),
        ("allowed_models", "['source']"),
    ]
    .into_iter()
    .enumerate()
    {
        let (manager, mut cfg) = fixture("");
        let id = format!("runtime-filter-{index}");
        manager.register_runtime_model("source", &id).unwrap();
        let raw: toml::Value = toml::from_str(&format!("[models]\n{section} = {value}")).unwrap();
        let filters = config::Config::new_from_toml_cfg(&raw).unwrap().models;
        cfg.models = filters;
        manager.apply_config(cfg);
        assert!(manager.runtime_model_entry(&id).is_err());
        assert!(
            manager
                .register_runtime_model("source", &format!("{id}-new"))
                .is_err()
        );
    }
}

#[test]
fn runtime_cannot_expose_hidden_alias_or_ambiguous_wire_id() {
    let (manager, _) = fixture(
        "\n[model.hidden-alias]\nmodel = 'runtime-alias'\nhidden = true\nbase_url = 'https://other.example/v1'\n",
    );
    assert!(
        manager
            .register_runtime_model("source", "runtime-alias")
            .is_err()
    );
    assert!(
        manager
            .register_runtime_model("source", "hidden-alias")
            .is_err()
    );
    assert!(
        manager
            .register_runtime_model("hidden-alias", "runtime-child")
            .is_err()
    );
    assert!(
        manager
            .register_runtime_model("source", "source-wire")
            .is_err()
    );
    assert!(
        manager
            .register_runtime_model("missing", "runtime-child")
            .is_err()
    );
}

#[test]
fn runtime_child_keeps_original_source_and_rejects_rebinding() {
    let (manager, cfg) = fixture("");
    manager
        .register_runtime_model("source", "runtime-parent")
        .unwrap();
    manager
        .register_runtime_model("runtime-parent", "runtime-child-chain")
        .unwrap();
    manager.rebuild(&cfg, None);
    assert_eq!(
        manager
            .runtime_model_entry("runtime-child-chain")
            .unwrap()
            .unwrap()
            .api_key
            .as_deref(),
        Some("source-key")
    );
    assert!(
        manager
            .register_runtime_model("source", "runtime-parent")
            .is_ok()
    );
    manager.inner.catalog.write().models.insert(
        "other".into(),
        ModelEntry::fallback("other", &cfg.endpoints),
    );
    assert!(
        manager
            .register_runtime_model("other", "runtime-parent")
            .is_err()
    );
}

#[test]
fn runtime_external_catalog_removal_invalidates_source() {
    let raw: toml::Value = toml::from_str(
        "[auth]\nauth_provider_command = 'unused-runtime-provider'\n\
         [endpoints]\nmodels_base_url = 'https://runtime.example/v1'\n\
         [model.source]\nmodel = 'source-wire'\nbase_url = 'https://runtime.example/v1'\napi_key = 'source-key'\n",
    ).unwrap();
    let cfg = config::Config::new_from_toml_cfg(&raw).unwrap();
    assert!(cfg.config_models.is_empty());
    assert!(resolve_model_catalog(&cfg, None).is_empty());
    // 外部目录只信任端点返回的条目，本地模型表不能提供来源。
    let mut source = ModelEntry::fallback("source-wire", &cfg.endpoints);
    source.info.base_url = "https://runtime.example/v1".to_owned();
    source.api_key = Some("source-key".to_owned());
    let prefetched = IndexMap::from([("source".to_owned(), source)]);
    let catalog = resolve_model_catalog(&cfg, Some(prefetched.clone()));
    assert!(catalog.contains_key("source"));
    let temp = tempfile::tempdir().unwrap();
    let manager = ModelsManager::new(
        Some(prefetched),
        catalog,
        acp::ModelId::new("source"),
        Arc::new(AuthManager::new(temp.path(), GrokComConfig::default())),
        cfg.clone(),
    );
    let id = "runtime-external-source";
    manager.register_runtime_model("source", id).unwrap();
    manager.set_current_model_id(acp::ModelId::new(id));
    manager.apply_catalog(&cfg, IndexMap::new(), None);
    assert!(manager.runtime_model_entry(id).is_err());
    assert!(
        manager
            .task_model_catalog_snapshot(false)
            .eligible
            .is_empty()
    );
    assert!(manager.available().is_empty());
    assert!(manager.sampling_config().api_key.is_none());
    assert!(manager.sampling_config().base_url.is_empty());
}

#[test]
fn runtime_auth_lookup_keeps_credentials_on_later_turns() {
    let (manager, _) = fixture("");
    let id = "runtime-auth-turns";
    manager.register_runtime_model("source", id).unwrap();
    for _ in 0..2 {
        let creds = config::try_resolve_model_credentials(id, Some("session-secret")).unwrap();
        assert_eq!(creds.api_key.as_deref(), Some("source-key"));
        assert_eq!(creds.base_url, "https://runtime.example/v1");
        assert_eq!(creds.auth_type, xai_chat_state::AuthType::ApiKey);
        assert_eq!(
            config::resolve_model_auth_facts_and_provider(id).0.byok,
            ModelByok::Byok
        );
    }
}

#[test]
fn runtime_provider_reference_survives_rebuild_without_session_fallback() {
    let (manager, mut cfg) = fixture("");
    let provider: xai_grok_login::AuthProviderRef =
        serde_json::from_value(serde_json::json!({"name":"runtime-helper"})).unwrap();
    let source = cfg.config_models.get_mut("source").unwrap();
    source.api_key = None;
    source.auth_provider = Some(provider.name.clone());
    cfg.auth_providers.insert(
        provider.name.clone(),
        xai_grok_config_types::AuthProviderConfig {
            command: "unused-test-command".into(),
            ..Default::default()
        },
    );
    manager.apply_config(cfg.clone());
    manager
        .register_runtime_model("source", "runtime-provider")
        .unwrap();
    manager.rebuild(&cfg, None);
    let (facts, provider) = config::resolve_model_auth_facts_and_provider("runtime-provider");
    assert_eq!(facts.byok, ModelByok::Byok);
    assert_eq!(provider.unwrap().name, "runtime-helper");
    let creds =
        config::try_resolve_model_credentials("runtime-provider", Some("session-secret")).unwrap();
    assert!(creds.api_key.is_none());
    cfg.auth_providers.clear();
    manager.apply_config(cfg);
    assert!(manager.runtime_model_entry("runtime-provider").is_err());
}

#[test]
fn runtime_rejected_policy_reload_never_keeps_old_permission() {
    let (manager, mut cfg) = fixture("");
    let id = "runtime-invalid-policy";
    manager.register_runtime_model("source", id).unwrap();
    manager.apply_catalog(&cfg, IndexMap::new(), None);
    cfg.models.allowed_models = Some(vec!["nothing-matches".to_owned()]);
    manager.apply_config(cfg);
    assert!(manager.runtime_model_entry(id).is_err());
    assert!(
        manager
            .register_runtime_model("source", "runtime-after-rejected-policy")
            .is_err()
    );
    assert!(config::try_resolve_model_credentials(id, Some("session-secret")).is_none());
}

#[test]
fn runtime_fleet_policy_cannot_be_relaxed_by_user_allowlist() {
    let (manager, mut cfg) = fixture("");
    cfg.models.allowed_models = Some(vec!["*".to_owned()]);
    cfg.requirements.allowed_models.pin(
        config::AllowlistPin::List(vec!["source-wire".to_owned()]),
        crate::config::RequirementSource::Unknown,
    );
    manager.apply_config(cfg);
    let error = manager
        .register_runtime_model("source", "runtime-fleet-denied")
        .unwrap_err();
    assert!(error.contains("organization"));
}

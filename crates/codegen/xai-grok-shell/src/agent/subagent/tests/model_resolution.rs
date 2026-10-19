use super::*;
use crate::agent::subagent::model_resolution::{resolve_fixed_model, resolve_ordinary_model};
use crate::session::subagents_model::SubagentsModelSelection;
use std::num::NonZeroU64;
use xai_grok_agent::config::{AgentDefinition, Effort};
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_subagent_resolution::config::{SubagentPersona, SubagentRole};

/// 不依赖进程全局模型目录的解析夹具。
fn model_context() -> (SubagentSpawnContext, AgentDefinition) {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("parent");
    ctx.sampling_config.model = "parent".into();
    ctx.sampling_config.reasoning_effort = Some(ReasoningEffort::Low);
    ctx.sampling_config.context_window = 256_000;
    ctx.sampling_config.fast_mode = true;
    for id in [
        "parent",
        "explicit",
        "session",
        "role",
        "persona",
        "type",
        "per-parent",
        "config",
        "definition",
    ] {
        let mut entry = test_model_entry(id);
        entry.info.reasoning_effort = Some(ReasoningEffort::Medium);
        entry.info.supports_reasoning_effort = true;
        entry.info.context_windows = vec![NonZeroU64::new(128_000).unwrap()];
        ctx.available_models.insert(id.into(), entry);
    }
    let mut definition = resolve_agent_definition("general-purpose", &ctx).unwrap();
    definition.model = ModelOverride::Inherit;
    definition.effort = None;
    (ctx, definition)
}

/// 普通解析采用真实生产入口。
async fn ordinary(
    ctx: &SubagentSpawnContext,
    definition: &AgentDefinition,
    overrides: &SubagentRuntimeOverrides,
) -> crate::agent::subagent::model_resolution::ResolvedSubagentModel {
    resolve_ordinary_model("general-purpose", overrides, definition, ctx).await
}

#[tokio::test]
async fn subagents_model_priority_covers_every_layer() {
    let (mut ctx, mut definition) = model_context();
    let mut overrides = SubagentRuntimeOverrides {
        model: Some("explicit".into()),
        persona: Some("worker".into()),
        ..Default::default()
    };
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: None,
        context_window: None,
    };
    ctx.subagent_roles.insert(
        "general-purpose".into(),
        SubagentRole {
            model: Some("role".into()),
            ..Default::default()
        },
    );
    ctx.subagent_personas.insert(
        "worker".into(),
        SubagentPersona {
            model: Some("persona".into()),
            ..Default::default()
        },
    );
    ctx.subagent_model_overrides
        .insert("general-purpose".into(), "type".into());
    ctx.subagent_default_model = Some("config".into());
    ctx.agent_config
        .get_or_insert_with(Default::default)
        .config_models
        .insert(
            "parent".into(),
            crate::agent::config::ConfigModelOverride {
                subagent_model: Some("per-parent".into()),
                ..Default::default()
            },
        );
    definition.model = ModelOverride::Override("definition".into());
    for (expected, source) in [
        ("explicit", "explicit"),
        ("session", "session"),
        ("role", "role"),
        ("persona", "persona"),
        ("type", "type_config"),
        ("per-parent", "parent_model_config"),
        ("config", "config"),
        ("definition", "definition"),
        ("parent", "parent"),
    ] {
        let resolved = ordinary(&ctx, &definition, &overrides).await;
        assert_eq!(resolved.model_id.0.as_ref(), expected);
        assert_eq!(resolved.model_source, source);
        match source {
            "explicit" => overrides.model = None,
            "session" => ctx.subagents_model = SubagentsModelSelection::Default,
            "role" => ctx.subagent_roles.clear(),
            "persona" => ctx.subagent_personas.clear(),
            "type_config" => ctx.subagent_model_overrides.clear(),
            "parent_model_config" => ctx.agent_config.as_mut().unwrap().config_models.clear(),
            "config" => ctx.subagent_default_model = None,
            "definition" => definition.model = ModelOverride::Inherit,
            _ => {}
        }
    }
}

/// 主模型目录键隔离不同连接，切换与 reset 使用当前主模型规则。
#[tokio::test]
async fn subagent_model_per_parent_defaults_follow_session_catalog_key() {
    let (mut ctx, definition) = model_context();
    let mut second = ctx.available_models["parent"].clone();
    second.info.model = "parent-wire".into();
    ctx.available_models.get_mut("parent").unwrap().info.model = "parent-wire".into();
    ctx.available_models.insert("parent-second".into(), second);
    let cfg = ctx.agent_config.get_or_insert_with(Default::default);
    for (key, child) in [("parent", "per-parent"), ("parent-second", "session")] {
        cfg.config_models.insert(
            key.into(),
            crate::agent::config::ConfigModelOverride {
                subagent_model: Some(child.into()),
                ..Default::default()
            },
        );
    }
    ctx.subagent_default_model = Some("config".into());
    ctx.subagent_default_reasoning_effort = Some("high".into());
    for (key, child) in [("parent", "per-parent"), ("parent-second", "session")] {
        ctx.model_id = acp::ModelId::new(key);
        let resolved = ordinary(&ctx, &definition, &Default::default()).await;
        assert_eq!(resolved.model_id.0.as_ref(), child);
        assert_eq!(resolved.model_source, "parent_model_config");
        assert_eq!(
            resolved.sampling.reasoning_effort,
            Some(ReasoningEffort::High)
        );
        let state = subagents_model_state(&ctx).await;
        assert_eq!(state.parent_model_id.as_deref(), Some(key));
        assert_eq!(state.parent_configured_model.as_deref(), Some(child));
        assert_eq!(state.configured_model.as_deref(), Some("config"));
    }
    ctx.subagents_model = SubagentsModelSelection::Inherit;
    assert_eq!(
        ordinary(&ctx, &definition, &Default::default())
            .await
            .model_id,
        ctx.model_id
    );
    ctx.subagents_model = SubagentsModelSelection::Default;
    assert_eq!(
        ordinary(&ctx, &definition, &Default::default())
            .await
            .model_id
            .0
            .as_ref(),
        "session"
    );
    for value in ["", "  ", "missing"] {
        ctx.agent_config
            .as_mut()
            .unwrap()
            .config_models
            .get_mut("parent-second")
            .unwrap()
            .subagent_model = Some(value.into());
        let resolved = ordinary(&ctx, &definition, &Default::default()).await;
        assert_eq!(resolved.model_id.0.as_ref(), "config");
        assert_eq!(resolved.warnings.is_empty(), value != "missing");
    }
    // 未配置的临时目录条目只使用全局默认。
    ctx.model_id = acp::ModelId::new("runtime-parent");
    ctx.available_models.insert(
        "runtime-parent".into(),
        ctx.available_models["parent"].clone(),
    );
    assert_eq!(
        ordinary(&ctx, &definition, &Default::default())
            .await
            .model_id
            .0
            .as_ref(),
        "config"
    );
}

#[tokio::test]
async fn subagents_effort_priority_is_independent_of_model_priority() {
    let (mut ctx, mut definition) = model_context();
    let mut overrides = SubagentRuntimeOverrides {
        reasoning_effort: Some("high".into()),
        persona: Some("worker".into()),
        ..Default::default()
    };
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: Some(ReasoningEffort::Low),
        context_window: None,
    };
    ctx.subagent_roles.insert(
        "general-purpose".into(),
        SubagentRole {
            reasoning_effort: Some("xhigh".into()),
            ..Default::default()
        },
    );
    ctx.subagent_personas.insert(
        "worker".into(),
        SubagentPersona {
            reasoning_effort: Some("medium".into()),
            ..Default::default()
        },
    );
    ctx.subagent_default_reasoning_effort = Some("high".into());
    ctx.agent_config
        .get_or_insert_with(Default::default)
        .config_models
        .insert(
            "parent".into(),
            crate::agent::config::ConfigModelOverride {
                subagent_model_reasoning_effort: Some("xhigh".into()),
                ..Default::default()
            },
        );
    definition.effort = Some(Effort::Low);
    for (source, expected) in [
        ("explicit", ReasoningEffort::High),
        ("session", ReasoningEffort::Low),
        ("role", ReasoningEffort::Xhigh),
        ("persona", ReasoningEffort::Medium),
        ("parent_model_config", ReasoningEffort::Xhigh),
        ("config", ReasoningEffort::High),
        ("definition", ReasoningEffort::Low),
        ("model_default", ReasoningEffort::Medium),
    ] {
        let resolved = ordinary(&ctx, &definition, &overrides).await;
        assert_eq!(resolved.model_id.0.as_ref(), "session");
        assert_eq!(resolved.sampling.reasoning_effort, Some(expected));
        assert_eq!(resolved.effort_source, source);
        match source {
            "explicit" => overrides.reasoning_effort = None,
            "session" => {
                ctx.subagents_model = SubagentsModelSelection::Model {
                    model_id: "session".into(),
                    reasoning_effort: None,
                    context_window: None,
                }
            }
            "role" => ctx.subagent_roles.clear(),
            "persona" => ctx.subagent_personas.clear(),
            "parent_model_config" => ctx.agent_config.as_mut().unwrap().config_models.clear(),
            "config" => ctx.subagent_default_reasoning_effort = None,
            "definition" => definition.effort = None,
            _ => {}
        }
    }
}

/// 同一上游路由的不同父模型仍使用各自配置，继承和显式模型均可应用专属 effort。
#[tokio::test]
async fn subagents_parent_effort_follows_catalog_key_and_inherit() {
    let (mut ctx, definition) = model_context();
    let mut second = ctx.available_models["parent"].clone();
    second.info.model = "parent-wire".into();
    ctx.available_models.get_mut("parent").unwrap().info.model = "parent-wire".into();
    ctx.available_models.insert("parent-second".into(), second);
    for (key, effort) in [("parent", "xhigh"), ("parent-second", "low")] {
        ctx.agent_config
            .get_or_insert_with(Default::default)
            .config_models
            .insert(
                key.into(),
                crate::agent::config::ConfigModelOverride {
                    subagent_model_reasoning_effort: Some(effort.into()),
                    ..Default::default()
                },
            );
    }
    ctx.subagent_default_reasoning_effort = Some("high".into());
    ctx.subagent_default_model = Some("config".into());
    for (key, expected) in [
        ("parent", ReasoningEffort::Xhigh),
        ("parent-second", ReasoningEffort::Low),
    ] {
        ctx.model_id = acp::ModelId::new(key);
        for (selection, target) in [
            (SubagentsModelSelection::Default, "config"),
            (SubagentsModelSelection::Inherit, key),
            (SubagentsModelSelection::Default, "config"),
        ] {
            ctx.subagents_model = selection;
            let resolved = ordinary(&ctx, &definition, &Default::default()).await;
            assert_eq!(resolved.model_id.0.as_ref(), target);
            assert_eq!(resolved.sampling.reasoning_effort, Some(expected));
            assert_eq!(resolved.effort_source, "parent_model_config");
            let state = subagents_model_state(&ctx).await;
            assert_eq!(state.reasoning_effort, Some(expected));
            assert_eq!(state.effort_source, "parent_model_config");
            assert_eq!(state.parent_configured_effort, Some(expected.to_string()));
            assert_eq!(state.configured_effort.as_deref(), Some("high"));
            assert!(state.parent_configured_model.is_none());
        }
        let resolved = ordinary(
            &ctx,
            &definition,
            &SubagentRuntimeOverrides {
                model: Some("explicit".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(resolved.model_id.0.as_ref(), "explicit");
        assert_eq!(resolved.sampling.reasoning_effort, Some(expected));
    }
    // 临时目录条目与来源路由相同，也不能借用来源的专属 effort。
    ctx.available_models.insert(
        "runtime-parent".into(),
        ctx.available_models["parent"].clone(),
    );
    ctx.model_id = acp::ModelId::new("runtime-parent");
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(resolved.effort_source, "config");
    assert!(
        subagents_model_state(&ctx)
            .await
            .parent_configured_effort
            .is_none()
    );
}

/// 空白和非法值回退全局，合法但不受目标支持的档位保留基础值。
#[tokio::test]
async fn subagents_parent_effort_fallback_and_target_validation() {
    let (mut ctx, definition) = model_context();
    ctx.subagent_default_model = Some("config".into());
    ctx.subagent_default_reasoning_effort = Some("high".into());
    for (raw, expected, source, warnings) in [
        (None, ReasoningEffort::High, "config", 0),
        (Some("  "), ReasoningEffort::High, "config", 0),
        (Some("invalid"), ReasoningEffort::High, "config", 1),
        (Some("max"), ReasoningEffort::Medium, "model_default", 1),
        (
            Some(" low "),
            ReasoningEffort::Low,
            "parent_model_config",
            0,
        ),
    ] {
        ctx.agent_config
            .get_or_insert_with(Default::default)
            .config_models
            .insert(
                "parent".into(),
                crate::agent::config::ConfigModelOverride {
                    subagent_model_reasoning_effort: raw.map(str::to_owned),
                    ..Default::default()
                },
            );
        let resolved = ordinary(&ctx, &definition, &Default::default()).await;
        assert_eq!(
            resolved.sampling.reasoning_effort,
            Some(expected),
            "{raw:?}"
        );
        assert_eq!(resolved.effort_source, source);
        assert_eq!(resolved.warnings.len(), warnings);
        let state = subagents_model_state(&ctx).await;
        assert_eq!(state.reasoning_effort, resolved.sampling.reasoning_effort);
        assert_eq!(
            state.parent_configured_effort.as_deref(),
            raw.map(str::trim).filter(|v| !v.is_empty())
        );
    }
    let entry = ctx.available_models.get_mut("config").unwrap();
    entry.info.supports_reasoning_effort = false;
    entry.info.reasoning_effort = None;
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.sampling.reasoning_effort, None);
    assert_eq!(resolved.effort_source, "model_default");
    assert_eq!(resolved.warnings.len(), 1);
}

#[tokio::test]
async fn subagents_inherit_uses_live_parent_and_keeps_global_effort() {
    let (mut ctx, definition) = model_context();
    ctx.subagents_model = SubagentsModelSelection::Inherit;
    ctx.subagent_default_model = Some("config".into());
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.model_id.0.as_ref(), "parent");
    assert_eq!(resolved.effort_source, "parent");
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    assert!(resolved.sampling.fast_mode);
    ctx.subagent_default_reasoning_effort = Some("high".into());
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.model_source, "session");
    assert_eq!(resolved.effort_source, "config");
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    ctx.subagents_model = SubagentsModelSelection::Default;
    assert_eq!(
        ordinary(&ctx, &definition, &Default::default())
            .await
            .model_id
            .0
            .as_ref(),
        "config"
    );
}

#[tokio::test]
async fn subagents_inherit_reads_parent_actor_not_process_baseline() {
    let (mut ctx, definition) = model_context();
    ctx.sampling_config.model = "process-baseline".into();
    let chat = spawn_test_parent_chat_state("parent");
    let mut live = test_sampling_config("parent");
    live.reasoning_effort = Some(ReasoningEffort::Xhigh);
    live.context_window = NonZeroU64::new(128_000).unwrap();
    live.fast_mode = true;
    live.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
    live.responses_compact_threshold = NonZeroU64::new(100_000);
    chat.update_sampling_config(live);
    ctx.parent_chat_state = Some(chat);
    ctx.subagents_model = SubagentsModelSelection::Inherit;
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.model_id.0.as_ref(), "parent");
    assert_eq!(resolved.sampling.model, "parent");
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::Xhigh)
    );
    assert_eq!(resolved.sampling.context_window, 128_000);
    assert!(resolved.sampling.fast_mode);
    assert_eq!(
        resolved.sampling.effective_responses_compact_threshold(),
        NonZeroU64::new(100_000)
    );
    let mut reduced = resolved.sampling;
    reduced.context_window = 100_000;
    assert_eq!(reduced.effective_responses_compact_threshold(), None);
}

#[tokio::test]
async fn subagents_parent_effort_uses_route_when_legacy_catalog_key_is_missing() {
    let (mut ctx, definition) = model_context();
    ctx.model_id = acp::ModelId::new("legacy-auto");
    ctx.subagent_default_reasoning_effort = Some("high".into());
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.model_id.0.as_ref(), "legacy-auto");
    assert_eq!(resolved.sampling.model, "parent");
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert!(resolved.warnings.is_empty());
}

#[tokio::test]
async fn subagents_global_effort_only_inherits_parent_model() {
    let (mut ctx, definition) = model_context();
    ctx.subagent_default_reasoning_effort = Some("high".into());
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.model_id.0.as_ref(), "parent");
    assert_eq!(resolved.model_source, "parent");
    assert_eq!(resolved.effort_source, "config");
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::High)
    );
}

#[tokio::test]
async fn subagents_session_parameters_apply_only_to_same_catalog_target() {
    let (mut ctx, definition) = model_context();
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        context_window: NonZeroU64::new(128_000),
    };
    for target in ["session", "explicit"] {
        let overrides = SubagentRuntimeOverrides {
            model: Some(target.into()),
            ..Default::default()
        };
        let resolved = ordinary(&ctx, &definition, &overrides).await;
        if target == "session" {
            assert_eq!(
                resolved.sampling.reasoning_effort,
                Some(ReasoningEffort::High)
            );
            assert_eq!(resolved.sampling.context_window, 128_000);
        } else {
            assert_eq!(
                resolved.sampling.reasoning_effort,
                Some(ReasoningEffort::Medium)
            );
            assert_eq!(resolved.sampling.context_window, 256_000);
        }
    }
    let mut duplicate_route = ctx.available_models["session"].clone();
    duplicate_route.api_key = Some("other-key".into());
    ctx.available_models
        .insert("other-connection".into(), duplicate_route);
    let overrides = SubagentRuntimeOverrides {
        model: Some("other-connection".into()),
        ..Default::default()
    };
    let resolved = ordinary(&ctx, &definition, &overrides).await;
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(resolved.sampling.context_window, 256_000);
    assert_eq!(resolved.sampling.api_key.as_deref(), Some("other-key"));
}

#[tokio::test]
async fn subagents_effort_menu_window_and_alias_route_are_validated() {
    let (mut ctx, definition) = model_context();
    let entry = ctx.available_models.get_mut("session").unwrap();
    entry.info.model = "route-base".into();
    entry.api_key = Some("session-model-key".into());
    entry.info.reasoning_efforts =
        serde_json::from_value(serde_json::json!(["low", "high"])).unwrap();
    entry.info.variants = vec![crate::agent::config::ModelVariant {
        effort: ReasoningEffort::High,
        model_id: "route-high".into(),
    }];
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        context_window: NonZeroU64::new(128_000),
    };
    let overrides = SubagentRuntimeOverrides {
        model: Some("route-high".into()),
        ..Default::default()
    };
    let resolved = ordinary(&ctx, &definition, &overrides).await;
    assert_eq!(resolved.model_id.0.as_ref(), "session");
    assert_eq!(resolved.sampling.model, "route-high");
    assert_eq!(
        resolved.sampling.api_key.as_deref(),
        Some("session-model-key")
    );
    assert_eq!(resolved.sampling.context_window, 128_000);
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: Some(ReasoningEffort::Max),
        context_window: NonZeroU64::new(999_999),
    };
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(resolved.sampling.context_window, 256_000);
    assert_eq!(resolved.effort_source, "model_default");
    assert_eq!(resolved.warnings.len(), 2);
}

#[tokio::test]
async fn subagents_no_reasoning_model_ignores_effort_with_warning() {
    let (mut ctx, definition) = model_context();
    let entry = ctx.available_models.get_mut("config").unwrap();
    entry.info.supports_reasoning_effort = false;
    entry.info.reasoning_effort = None;
    ctx.subagent_default_model = Some("config".into());
    ctx.subagent_default_reasoning_effort = Some("high".into());
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(resolved.sampling.reasoning_effort, None);
    assert_eq!(resolved.effort_source, "model_default");
    assert_eq!(resolved.warnings.len(), 1);
}

#[tokio::test]
async fn subagents_invalid_static_config_falls_through_and_state_matches_spawn() {
    let (mut ctx, _) = model_context();
    ctx.subagent_model_overrides
        .insert("general-purpose".into(), "missing-type".into());
    ctx.subagent_default_model = Some("missing-global".into());
    ctx.subagent_default_reasoning_effort = Some("invalid".into());
    let definition = resolve_agent_definition("general-purpose", &ctx).unwrap();
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    let state = subagents_model_state(&ctx).await;
    assert_eq!(state.model_id, resolved.model_id.0.as_ref());
    assert_eq!(state.model_source, resolved.model_source);
    assert_eq!(state.reasoning_effort, resolved.sampling.reasoning_effort);
    assert_eq!(state.effort_source, resolved.effort_source);
    assert_eq!(state.context_window, resolved.sampling.context_window);
    assert_eq!(state.configured_model.as_deref(), Some("missing-global"));
    assert_eq!(state.configured_effort.as_deref(), Some("invalid"));
    let warning = state.warning.unwrap();
    for value in ["missing-type", "missing-global", "invalid"] {
        assert!(warning.contains(value));
    }
    assert!(state.available_models.is_empty());
}

#[tokio::test]
async fn subagents_invalid_effort_falls_through_to_definition() {
    let (mut ctx, mut definition) = model_context();
    ctx.subagent_default_reasoning_effort = Some("invalid".into());
    definition.effort = Some(Effort::High);
    let resolved = ordinary(&ctx, &definition, &Default::default()).await;
    assert_eq!(
        resolved.sampling.reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(resolved.effort_source, "definition");
    assert_eq!(resolved.warnings.len(), 1);
}

#[tokio::test]
async fn subagents_resume_and_fork_ignore_new_model_overrides() {
    let (mut ctx, _) = model_context();
    ctx.agent_config
        .get_or_insert_with(Default::default)
        .config_models
        .insert(
            "parent".into(),
            crate::agent::config::ConfigModelOverride {
                subagent_model: Some("per-parent".into()),
                subagent_model_reasoning_effort: Some("xhigh".into()),
                ..Default::default()
            },
        );
    ctx.subagent_default_model = Some("config".into());
    ctx.subagent_default_reasoning_effort = Some("high".into());
    ctx.subagents_model = SubagentsModelSelection::Model {
        model_id: "session".into(),
        reasoning_effort: Some(ReasoningEffort::Xhigh),
        context_window: NonZeroU64::new(128_000),
    };
    let fork = resolve_fixed_model(None, None, "general-purpose", &ModelOverride::Inherit, &ctx)
        .await
        .unwrap();
    assert_eq!(fork.model_id.0.as_ref(), "parent");
    assert_eq!(
        fork.sampling.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(fork.sampling.context_window, 256_000);
    let mut source = ResumeSourceData {
        subagent_id: "saved-child".into(),
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: Some("definition".into()),
        child_cwd: String::new(),
        worktree_path: None,
        snapshot_ref: None,
        child_session_id: "saved-child".into(),
    };
    let resumed = resolve_fixed_model(
        Some(&source),
        None,
        "general-purpose",
        &ModelOverride::Inherit,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(resumed.model_id.0.as_ref(), "definition");
    assert_eq!(
        resumed.sampling.reasoning_effort,
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(resumed.sampling.context_window, 256_000);
    source.model_id = Some("parent".into());
    ctx.available_models.shift_remove("parent");
    let resumed = resolve_fixed_model(
        Some(&source),
        None,
        "general-purpose",
        &ModelOverride::Inherit,
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(resumed.model_id.0.as_ref(), "parent");
    assert_eq!(
        resumed.sampling.reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    source.model_id = Some("missing".into());
    assert!(
        resolve_fixed_model(
            Some(&source),
            None,
            "general-purpose",
            &ModelOverride::Inherit,
            &ctx
        )
        .await
        .is_err()
    );
}

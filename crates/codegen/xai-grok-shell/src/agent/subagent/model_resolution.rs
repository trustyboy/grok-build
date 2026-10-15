//! 子代理模型、推理强度和窗口的统一解析；状态查询复用普通启动路径。
use super::*;
use crate::session::subagents_model::{SubagentsModelSelection, SubagentsModelState};
use xai_grok_agent::config::AgentDefinition;
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_subagent_resolution::config::{SubagentPersona, SubagentRole};

/// 来源标识用于状态协议，不随展示语言变化。
pub(super) struct ResolvedSubagentModel {
    /// 带目标连接认证的最终采样配置。
    pub sampling: xai_grok_sampler::SamplerConfig,
    /// 目录键，不随 effort 路由名称变化。
    pub model_id: acp::ModelId,
    /// 胜出的模型选择层。
    pub model_source: &'static str,
    /// 实际应用的 effort 层或基础配置来源。
    pub effort_source: &'static str,
    /// 去重后的配置降级原因。
    pub warnings: Vec<String>,
}

impl ResolvedSubagentModel {
    /// 同一原因只保留一次，并记录启动告警。
    fn warn(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            tracing::warn!(%warning, "子代理模型配置回退");
            self.warnings.push(warning);
        }
    }
}

/// 空白配置不占用优先级。
fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// 目录键优先；路由别名沿用模型配置的匹配顺序。
fn catalog_key<'a>(ctx: &'a SubagentSpawnContext, model_id: &str) -> Option<&'a str> {
    ctx.available_models
        .get_key_value(model_id)
        .or_else(|| {
            ctx.available_models
                .iter()
                .find(|(_, entry)| entry.info.has_model_id(model_id))
        })
        .map(|(key, _)| key.as_str())
}

/// 不跨连接比较路由名称，避免不同配置键使用相同上游名称时误应用参数。
fn same_target(ctx: &SubagentSpawnContext, requested: &str, selected: &str) -> bool {
    match (catalog_key(ctx, requested), catalog_key(ctx, selected)) {
        (Some(requested), Some(selected)) => requested == selected,
        _ => requested == selected,
    }
}

/// 与角色和 persona 提示词解析使用同一选择规则。
fn role_and_persona<'a>(
    subagent_type: &str,
    overrides: &SubagentRuntimeOverrides,
    ctx: &'a SubagentSpawnContext,
) -> (Option<&'a SubagentRole>, Option<&'a SubagentPersona>) {
    let (role, _) =
        xai_grok_subagent_resolution::select_role(subagent_type, overrides, &ctx.subagent_roles);
    let persona = overrides
        .persona
        .as_ref()
        .and_then(|name| ctx.subagent_personas.get(name));
    (role, persona)
}

/// 按当前父会话目录键读取，临时模型不会借用连接来源的专属默认值。
fn parent_configured_model(ctx: &SubagentSpawnContext) -> Option<&str> {
    let config = ctx.agent_config.as_ref()?;
    let parent_id = ctx.model_id.0.as_ref();
    let key = if config.config_models.contains_key(parent_id) {
        parent_id
    } else {
        catalog_key(ctx, parent_id)?
    };
    nonblank(config.config_models.get(key)?.subagent_model.as_deref())
}

/// 普通模型优先级：单次 > 会话 > role/persona > 类型配置 > 主模型配置 > 全局 > 定义 > 父模型。
pub(super) async fn resolve_model(
    subagent_type: &str,
    overrides: &SubagentRuntimeOverrides,
    definition_model: &ModelOverride,
    ctx: &SubagentSpawnContext,
) -> ResolvedSubagentModel {
    let (parent, parent_id) = read_parent_sampling_config(ctx).await;
    let mut resolved = ResolvedSubagentModel {
        sampling: parent,
        model_id: parent_id,
        model_source: "parent",
        effort_source: "parent",
        warnings: Vec::new(),
    };
    // 即使临时选择遮盖配置，状态仍报告无效的静态默认值。
    let parent_default = parent_configured_model(ctx);
    for (source, model) in [
        ("parent_model_config", parent_default),
        ("config", ctx.subagent_default_model.as_deref()),
        (
            "type_config",
            ctx.subagent_model_overrides
                .get(subagent_type)
                .map(String::as_str),
        ),
    ] {
        if let Some(model) = nonblank(model)
            && model_override_entry(model, ctx).is_none()
        {
            resolved.warn(format!("{source} 模型 '{model}' 不存在或不可用，已回退"));
        }
    }
    if let Some(raw) = nonblank(ctx.subagent_default_reasoning_effort.as_deref())
        && raw.parse::<ReasoningEffort>().is_err()
    {
        resolved.warn(format!("config reasoning_effort '{raw}' 无效，已回退"));
    }
    let (role, persona) = role_and_persona(subagent_type, overrides, ctx);
    let session_model = match &ctx.subagents_model {
        SubagentsModelSelection::Model { model_id, .. } => Some(model_id.as_str()),
        _ => None,
    };
    let definition_model = match definition_model {
        ModelOverride::Override(model) => Some(model.as_str()),
        ModelOverride::Inherit => None,
    };
    for (source, model) in [
        ("explicit", overrides.model.as_deref()),
        ("session", session_model),
        ("role", role.and_then(|role| role.model.as_deref())),
        (
            "persona",
            persona.and_then(|persona| persona.model.as_deref()),
        ),
        (
            "type_config",
            ctx.subagent_model_overrides
                .get(subagent_type)
                .map(String::as_str),
        ),
        ("parent_model_config", parent_default),
        ("config", ctx.subagent_default_model.as_deref()),
        ("definition", definition_model),
    ] {
        if source == "session" && matches!(ctx.subagents_model, SubagentsModelSelection::Inherit) {
            resolved.model_source = "session";
            return resolved;
        }
        let Some(model) = nonblank(model) else {
            continue;
        };
        if let Some((sampling, model_id)) = resolve_model_override_to_config(model, ctx) {
            log_subagent_model_resolution(
                subagent_type,
                source,
                &sampling,
                &model_id,
                &resolved.sampling,
            );
            resolved.sampling = sampling;
            resolved.model_id = model_id;
            resolved.model_source = source;
            resolved.effort_source = "model_default";
            return resolved;
        }
        resolved.warn(format!("{source} 模型 '{model}' 不存在或不可用，已回退"));
    }
    resolved
}

/// 校验具体档位；无菜单的旧模型只支持既有四档。
fn supports_effort(entry: &crate::agent::config::ModelEntry, effort: ReasoningEffort) -> bool {
    let info = entry.info();
    if info.reasoning_efforts.is_empty() {
        info.supports_reasoning_effort
            && matches!(
                effort,
                ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::Xhigh
            )
    } else {
        info.reasoning_efforts
            .iter()
            .any(|option| option.value == effort)
    }
}

/// 父会话的历史目录键可能缺失，此时按其实际路由名称校验能力。
fn selected_entry<'a>(
    resolved: &ResolvedSubagentModel,
    ctx: &'a SubagentSpawnContext,
) -> Option<&'a crate::agent::config::ModelEntry> {
    crate::agent::config::find_model_by_id(&ctx.available_models, resolved.model_id.0.as_ref())
        .or_else(|| {
            crate::agent::config::find_model_by_id(&ctx.available_models, &resolved.sampling.model)
        })
}

/// 不支持目标档位时保留基础配置，不猜测其他档位。
fn apply_effort(
    resolved: &mut ResolvedSubagentModel,
    effort: ReasoningEffort,
    source: &'static str,
    ctx: &SubagentSpawnContext,
) {
    let entry = selected_entry(resolved, ctx);
    if let Some(entry) = entry.filter(|entry| supports_effort(entry, effort)) {
        resolved.sampling.model = entry.info.model_at(effort).to_owned();
        resolved.sampling.reasoning_effort = Some(effort);
        resolved.effort_source = source;
    } else {
        resolved.warn(format!(
            "{source} reasoning_effort '{effort}' 不受模型 '{}' 支持，保留基础配置",
            resolved.model_id.0
        ));
    }
}

/// 临时参数必须与最终目录目标相同，显式切换到其他连接时不泄漏。
fn session_parameters(
    resolved: &ResolvedSubagentModel,
    ctx: &SubagentSpawnContext,
) -> Option<(Option<ReasoningEffort>, Option<std::num::NonZeroU64>)> {
    match &ctx.subagents_model {
        SubagentsModelSelection::Model {
            model_id,
            reasoning_effort,
            context_window,
        } if same_target(ctx, model_id, resolved.model_id.0.as_ref()) => {
            Some((*reasoning_effort, *context_window))
        }
        _ => None,
    }
}

/// 普通启动与状态查询共同使用的完整解析入口。
pub(super) async fn resolve_ordinary_model(
    subagent_type: &str,
    overrides: &SubagentRuntimeOverrides,
    definition: &AgentDefinition,
    ctx: &SubagentSpawnContext,
) -> ResolvedSubagentModel {
    let mut resolved = resolve_model(subagent_type, overrides, &definition.model, ctx).await;
    let (role, persona) = role_and_persona(subagent_type, overrides, ctx);
    let parameters = session_parameters(&resolved, ctx);
    let session_effort = parameters
        .and_then(|(effort, _)| effort)
        .map(|effort| effort.to_string());
    let definition_effort = definition
        .effort
        .map(|effort| <&str>::from(effort).to_owned());
    for (source, raw) in [
        ("explicit", overrides.reasoning_effort.as_deref()),
        ("session", session_effort.as_deref()),
        (
            "role",
            role.and_then(|role| role.reasoning_effort.as_deref()),
        ),
        (
            "persona",
            persona.and_then(|persona| persona.reasoning_effort.as_deref()),
        ),
        ("config", ctx.subagent_default_reasoning_effort.as_deref()),
        ("definition", definition_effort.as_deref()),
    ] {
        let Some(raw) = nonblank(raw) else { continue };
        match raw.parse::<ReasoningEffort>() {
            Ok(effort) => {
                apply_effort(&mut resolved, effort, source, ctx);
                break;
            }
            Err(_) => resolved.warn(format!("{source} reasoning_effort '{raw}' 无效，已回退")),
        }
    }
    if let Some(window) = parameters.and_then(|(_, window)| window) {
        if crate::agent::config::find_model_by_id(
            &ctx.available_models,
            resolved.model_id.0.as_ref(),
        )
        .is_some_and(|entry| entry.info.supports_context_window(window))
        {
            resolved.sampling.context_window = window.get();
        } else {
            resolved.warn(format!(
                "session context_window '{window}' 不受模型 '{}' 支持，保留基础配置",
                resolved.model_id.0
            ));
        }
    }
    resolved
}

/// 恢复和内部 fork 沿用固定模型规则，不读取新增全局或会话覆盖。
pub(super) async fn resolve_fixed_model(
    source: Option<&ResumeSourceData>,
    runtime_effort: Option<&str>,
    subagent_type: &str,
    definition_model: &ModelOverride,
    ctx: &SubagentSpawnContext,
) -> Result<ResolvedSubagentModel, String> {
    let parent = read_parent_sampling_config(ctx).await;
    let definition_model = match definition_model {
        ModelOverride::Override(model) => Some(model.as_str()),
        ModelOverride::Inherit => None,
    };
    let legacy_candidates = if source.is_some() {
        [
            ctx.subagent_model_overrides
                .get(subagent_type)
                .map(String::as_str),
            definition_model,
        ]
    } else {
        [Some(ctx.model_id.0.as_ref()), None]
    };
    let baseline = legacy_candidates
        .into_iter()
        .flatten()
        .find_map(|model| resolve_model_override_to_config(model, ctx))
        .unwrap_or(parent);
    let (sampling, model_id) = if let Some(source_model) =
        source.and_then(|source| source.model_id.as_deref())
    {
        // 已解析为源模型时保留既有基础配置，允许继承不在目录的父模型。
        if source_model == baseline.1.0.as_ref() {
            baseline
        } else {
            resolve_model_override_to_config(source_model, ctx).ok_or_else(|| format!(
                "Cannot resume from subagent '{}': source model '{source_model}' is no longer available in the model catalogue.",
                source.map(|source| source.subagent_id.as_str()).unwrap_or_default(),
            ))?
        }
    } else {
        baseline
    };
    let model_source = if source.is_some() { "resume" } else { "parent" };
    let mut resolved = ResolvedSubagentModel {
        sampling,
        model_id,
        model_source,
        effort_source: "model_default",
        warnings: Vec::new(),
    };
    if let Some(raw) = nonblank(runtime_effort) {
        match raw.parse::<ReasoningEffort>() {
            Ok(effort) => apply_effort(&mut resolved, effort, "runtime", ctx),
            Err(_) => resolved.warn(format!("runtime reasoning_effort '{raw}' 无效，已回退")),
        }
    }
    Ok(resolved)
}

/// 返回默认 general-purpose 的真实解析结果；可选模型由 handler 填充。
pub(crate) async fn subagents_model_state(ctx: &SubagentSpawnContext) -> SubagentsModelState {
    let definition = resolve_agent_definition("general-purpose", ctx);
    let mut resolved = if let Some(definition) = definition {
        resolve_ordinary_model(
            "general-purpose",
            &SubagentRuntimeOverrides::default(),
            &definition,
            ctx,
        )
        .await
    } else {
        let mut resolved = resolve_model(
            "general-purpose",
            &SubagentRuntimeOverrides::default(),
            &ModelOverride::Inherit,
            ctx,
        )
        .await;
        resolved.warn("general-purpose 定义不可用".to_owned());
        resolved
    };
    SubagentsModelState {
        model_id: resolved.model_id.0.to_string(),
        model_source: resolved.model_source.to_owned(),
        reasoning_effort: resolved.sampling.reasoning_effort,
        effort_source: resolved.effort_source.to_owned(),
        context_window: resolved.sampling.context_window,
        parent_model_id: Some(ctx.model_id.0.to_string()),
        parent_configured_model: parent_configured_model(ctx).map(str::to_owned),
        configured_model: ctx.subagent_default_model.clone(),
        configured_effort: ctx.subagent_default_reasoning_effort.clone(),
        warning: (!resolved.warnings.is_empty())
            .then(|| std::mem::take(&mut resolved.warnings).join("；")),
        available_models: Vec::new(),
    }
}

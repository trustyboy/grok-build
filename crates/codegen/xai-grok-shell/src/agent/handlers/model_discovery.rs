//! 当前会话上游的显式模型发现；不修改用户配置或全局默认模型。

use agent_client_protocol as acp;
use serde::Deserialize;

use crate::agent::config::{ModelEntry, find_model_by_id, resolve_model_list};
use crate::agent::mvp_agent::MvpAgent;
use crate::agent::remote_config::resolve_catalog_key;
use crate::extensions::{ExtResult, parse_params};
use crate::session::ExtMethodResult;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoverRequest {
    session_id: String,
    model_id: String,
    expected_model_id: String,
}

/// 只接受精确模型 ID，避免把未知思考参数或自由文本变成网络查询。
fn validate_id(id: &str) -> Result<(), acp::Error> {
    if id.is_empty() || id.len() > 512 || id.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(acp::Error::invalid_params().data("请输入无空白的完整上游模型 ID"));
    }
    Ok(())
}

/// 比较来源配置时只保留摘要，不记录凭据或配置正文。
pub(super) fn source_fingerprint(entry: &ModelEntry) -> Result<blake3::Hash, acp::Error> {
    let bytes = serde_json::to_vec(entry)
        .map_err(|_| acp::Error::internal_error().data("无法校验模型连接来源"))?;
    let mut hash = blake3::Hasher::new();
    hash.update(&bytes);
    if let Some(provider) = entry.auth_provider.as_ref() {
        let trusted = serde_json::to_vec(&(
            &provider.config.command,
            &provider.config.args,
            provider.config.token_ttl_secs,
            provider.config.timeout_secs,
            &provider.config.cwd,
        ))
        .map_err(|_| acp::Error::internal_error().data("无法校验认证来源"))?;
        hash.update(&trusted);
    }
    if let Some(key) = entry.own_credential() {
        hash.update(key.as_bytes());
    }
    for name in entry.info.env_http_headers.values() {
        if let Ok(value) = std::env::var(name) {
            hash.update(value.as_bytes());
        }
    }
    Ok(hash.finalize())
}

/// 完成发现后重新校验会话与来源，再复用现有会话切换流程。
pub(crate) async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: DiscoverRequest = parse_params(args)?;
    validate_id(&req.model_id)?;
    if crate::agent::chat_modes::process_chat_mode_enabled() {
        return Err(acp::Error::invalid_params().data("当前聊天模式不支持自定义上游模型发现"));
    }
    if !crate::util::config::explicit_model_discovery_allowed() {
        return Err(acp::Error::invalid_params().data("管理策略禁止远程模型查询，或无法读取该策略"));
    }
    let sid = acp::SessionId::new(req.session_id.clone());
    let handle = agent
        .session_handle_waiting_for_load(&sid)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("当前会话不存在"))?;
    if handle.model_id.0.as_ref() != req.expected_model_id {
        return Err(acp::Error::invalid_params().data("当前模型已改变，请重新执行命令"));
    }
    let target = acp::ModelId::new(req.model_id.clone());
    let catalog = agent.models_manager.models();
    if let Some(key) = resolve_catalog_key(&catalog, &target) {
        if key != target {
            return Err(acp::Error::invalid_params()
                .data("该上游 ID 已配置为另一个目录名称，请使用已有模型名称切换"));
        }
        if !agent.models_manager.available().contains_key(&key) {
            return Err(acp::Error::invalid_params().data("该模型已存在，但不允许在当前目录中选择"));
        }
        return switch_and_report(agent, sid, key, &req.expected_model_id, None).await;
    }
    // 过滤前的目录用于识别被显式禁用的条目，不能把它们重新发现为临时模型。
    {
        let cfg = agent.cfg.borrow();
        if find_model_by_id(&resolve_model_list(&cfg, None), &req.model_id).is_some() {
            return Err(
                acp::Error::invalid_params().data("该模型已配置但不可选，请检查模型过滤规则")
            );
        }
    }
    let source_key = resolve_catalog_key(&catalog, &handle.model_id)
        .ok_or_else(|| acp::Error::invalid_params().data("当前模型没有可用的上游连接"))?;
    if !agent.models_manager.available().contains_key(&source_key) {
        return Err(acp::Error::invalid_params().data("当前上游模型已被隐藏或禁止选择"));
    }
    let source = agent.resolve_model_id(&source_key)?;
    let before = source_fingerprint(&source)?;
    if source.own_credential().is_none()
        && source.auth_provider.is_none()
        && (source.api_key.is_some() || source.env_key.is_some())
    {
        return Err(acp::Error::invalid_params()
            .data("当前上游的显式凭据不可用，请检查 API Key 或环境变量"));
    }
    if let Some(provider) = source.effective_auth_provider() {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.ensure_fresh_token(None),
        )
        .await
        .map_err(|_| acp::Error::invalid_params().data("上游认证刷新超时"))?;
        if matches!(
            outcome,
            xai_grok_login::auth_provider::ProviderRefreshOutcome::Unusable
                | xai_grok_login::auth_provider::ProviderRefreshOutcome::MintFailed
        ) {
            return Err(acp::Error::invalid_params().data("上游认证刷新失败"));
        }
    }
    let config = agent.prepare_sampling_config_for_model(&source, handle.origin_client.clone());
    // 未显式配置凭据的第三方地址不能通过发现接口接收登录会话 token。
    if !source.has_own_credentials()
        && !crate::util::is_xai_api_bearer_url(&config.base_url)
        && agent.auth_manager.current_or_expired().is_some()
    {
        return Err(acp::Error::invalid_params()
            .data("请为当前第三方上游显式配置 API Key 或认证提供者后再发现模型"));
    }
    let ids = xai_grok_sampler::SamplingClient::discover_model_ids(config)
        .await
        .map_err(|message| acp::Error::invalid_params().data(message))?;
    if !ids.iter().any(|id| id == &req.model_id) {
        return Err(acp::Error::invalid_params().data(format!(
            "当前上游模型列表未找到 {}，当前模型未改变",
            req.model_id
        )));
    }
    if !crate::util::config::explicit_model_discovery_allowed() {
        return Err(acp::Error::invalid_params().data("查询期间远程访问策略已改变"));
    }
    let current = agent
        .resident_handle(&sid)
        .ok_or_else(|| acp::Error::invalid_params().data("查询期间会话已关闭"))?;
    if current.model_id.0.as_ref() != req.expected_model_id
        || source_fingerprint(&agent.resolve_model_id(&source_key)?)? != before
    {
        return Err(acp::Error::invalid_params().data("查询期间模型或上游连接已改变，请重试"));
    }
    let registered = agent
        .models_manager
        .register_runtime_model(source_key.0.as_ref(), &req.model_id)
        .map_err(|message| acp::Error::invalid_params().data(message))?;
    switch_and_report(
        agent,
        sid,
        registered,
        &req.expected_model_id,
        Some((source_key.0.to_string(), before.to_hex().to_string())),
    )
    .await
}

/// 成功响应携带权威会话模型状态，客户端无须等待目录广播再猜测状态。
async fn switch_and_report(
    agent: &MvpAgent,
    session_id: acp::SessionId,
    model_id: acp::ModelId,
    expected: &str,
    source: Option<(String, String)>,
) -> ExtResult {
    let mut meta = acp::Meta::new();
    meta.insert(
        "expectedModelId".into(),
        serde_json::Value::String(expected.to_owned()),
    );
    if let Some((source_id, fingerprint)) = source {
        meta.insert(
            "discoverySourceId".into(),
            serde_json::Value::String(source_id),
        );
        meta.insert(
            "discoverySourceFingerprint".into(),
            serde_json::Value::String(fingerprint),
        );
    }
    agent
        .set_model_gated(acp::SetSessionModelRequest::new(session_id.clone(), model_id).meta(meta))
        .await?;
    // 目录与会话选择分别通知，其他客户端只更新目录而不切换自己的模型。
    if let Ok(params) = serde_json::value::to_raw_value(&agent.model_state(None)) {
        agent
            .gateway
            .forward_fire_and_forget(acp::ExtNotification::new(
                "x.ai/models/update",
                params.into(),
            ));
    }
    ExtMethodResult::success(serde_json::json!({"models": agent.model_state(Some(&session_id))}))
        .to_ext_response()
        .map_err(|_| acp::Error::internal_error().data("无法返回模型切换结果"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_requires_exact_id() {
        for value in ["", "model high", " model", "model\n", "model\u{0000}"] {
            assert!(validate_id(value).is_err());
        }
        for value in ["vendor/model-v2", "Model:revision", "model-high"] {
            assert!(validate_id(value).is_ok());
        }
    }
}

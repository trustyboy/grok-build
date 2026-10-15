//! 优先当前连接，再查询其他已配置上游；临时模型不写入用户配置。

use std::collections::HashSet;
use std::time::Duration;

use agent_client_protocol as acp;
use futures::{StreamExt, stream};
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
    let environment = serde_json::to_vec(&(
        entry.own_credential(),
        entry
            .info
            .env_http_headers
            .values()
            .map(|name| (name, std::env::var(name).ok()))
            .collect::<Vec<_>>(),
    ))
    .map_err(|_| acp::Error::internal_error().data("无法校验环境凭据"))?;
    hash.update(&environment);
    Ok(hash.finalize())
}

/// 连接摘要排除模型名称与能力，保留认证来源以隔离同地址的不同租户。
fn connection_fingerprint(entry: &ModelEntry) -> Result<blake3::Hash, acp::Error> {
    let mut connection = entry.clone();
    connection.info = crate::agent::config::ModelInfo::fallback("discovery-connection");
    connection.info.base_url = entry.info.base_url.trim_end_matches('/').to_owned();
    connection.info.api_backend = entry.info.api_backend.clone();
    connection.info.auth_scheme = entry.info.auth_scheme;
    connection.info.extra_headers = entry.info.extra_headers.clone();
    connection.info.query_params = entry.info.query_params.clone();
    connection.info.env_http_headers = entry.info.env_http_headers.clone();
    sort_distinct_headers(&mut connection.info.extra_headers);
    connection.info.query_params.sort_keys();
    sort_distinct_headers(&mut connection.info.env_http_headers);
    source_fingerprint(&connection)
}

/// HTTP 头忽略大小写；重复拼写的覆盖顺序会影响认证，必须保留。
fn sort_distinct_headers(headers: &mut indexmap::IndexMap<String, String>) {
    let mut names = HashSet::new();
    if headers
        .keys()
        .all(|name| names.insert(name.to_ascii_lowercase()))
    {
        headers.sort_keys();
    }
}

/// 单次发现固定来源快照，提交时重新校验完整配置。
struct DiscoverySource {
    /// 可用于切换来源模型的本地目录名称。
    key: acp::ModelId,
    /// 每个请求独立使用对应连接和凭据。
    entry: ModelEntry,
    /// 检测查询期间发生的来源变化。
    fingerprint: blake3::Hash,
    /// 自动扫描的候选必须持续存在于本地配置。
    require_configured: bool,
}

impl DiscoverySource {
    /// 记录完整来源摘要，摘要及错误信息均不包含明文凭据。
    fn new(key: acp::ModelId, entry: ModelEntry) -> Result<Self, acp::Error> {
        let fingerprint = source_fingerprint(&entry)?;
        Ok(Self {
            key,
            entry,
            fingerprint,
            require_configured: false,
        })
    }

    /// 错误提示使用实际连接地址，隐藏 URL 中的凭据及查询参数。
    fn display_address(&self, agent: &MvpAgent) -> String {
        let config = agent.prepare_sampling_config_for_model(&self.entry, None);
        let Ok(mut url) = reqwest::Url::parse(&config.base_url) else {
            return "无效上游地址".into();
        };
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return "无效上游地址".into();
        }
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    }

    /// 排队执行和认证等待后都检查来源，避免继续使用已撤销的配置。
    fn validate(&self, agent: &MvpAgent) -> Result<(), String> {
        if !crate::util::config::explicit_model_discovery_allowed() {
            return Err("管理策略禁止远程模型查询，或无法读取该策略".into());
        }
        if self.require_configured
            && !agent
                .cfg
                .borrow()
                .config_models
                .contains_key(self.key.0.as_ref())
        {
            return Err("上游来源配置已删除，请重新发现".into());
        }
        if !agent.models_manager.available().contains_key(&self.key) {
            return Err("上游来源已隐藏或禁止选择".into());
        }
        let current = agent
            .resolve_model_id(&self.key)
            .map_err(|_| "上游来源已失效".to_owned())?;
        if source_fingerprint(&current).map_err(|_| "无法校验上游来源".to_owned())?
            != self.fingerprint
        {
            return Err("上游连接在查询前已改变，请重新发现".into());
        }
        Ok(())
    }
}

/// 查询单个来源；失败交给调用方汇总，不能换用其他来源的认证。
async fn query_source(
    agent: &MvpAgent,
    candidate: &DiscoverySource,
    target: &str,
    origin_client: Option<crate::http::OriginClientInfo>,
) -> Result<bool, String> {
    candidate.validate(agent)?;
    let source = &candidate.entry;
    if source.own_credential().is_none()
        && source.auth_provider.is_none()
        && (source.api_key.is_some() || source.env_key.is_some())
    {
        return Err("显式凭据不可用，请检查 API Key 或环境变量".into());
    }
    if let Some(provider) = source.effective_auth_provider() {
        let outcome =
            tokio::time::timeout(Duration::from_secs(10), provider.ensure_fresh_token(None))
                .await
                .map_err(|_| "上游认证刷新超时".to_owned())?;
        if matches!(
            outcome,
            xai_grok_login::auth_provider::ProviderRefreshOutcome::Unusable
                | xai_grok_login::auth_provider::ProviderRefreshOutcome::MintFailed
        ) {
            return Err("上游认证刷新失败".into());
        }
    }
    candidate.validate(agent)?;
    let config = agent.prepare_sampling_config_for_model(source, origin_client);
    validate_discovery_credentials(source, &config)?;
    let ids = xai_grok_sampler::SamplingClient::discover_model_ids(config).await?;
    Ok(ids.iter().any(|id| id == target))
}

/// 第三方可匿名或使用自身凭据，禁止自动继承全局密钥和登录 token。
fn validate_discovery_credentials(
    source: &ModelEntry,
    config: &xai_grok_sampler::SamplerConfig,
) -> Result<(), String> {
    if !source.has_own_credentials()
        && !crate::util::is_xai_api_bearer_url(&config.base_url)
        && (config.api_key.is_some() || config.bearer_resolver.is_some())
    {
        return Err("请为第三方上游显式配置 API Key 或认证提供者后再发现模型".into());
    }
    Ok(())
}

/// 按配置顺序选择首个命中；全部未命中时汇总失败原因。
fn select_fallback(
    target: &str,
    matches: Vec<DiscoverySource>,
    misses: usize,
    mut failures: Vec<String>,
) -> Result<DiscoverySource, acp::Error> {
    if let Some(source) = matches.into_iter().next() {
        return Ok(source);
    }
    failures.sort_unstable();
    let details = if failures.is_empty() {
        String::new()
    } else {
        format!("；查询失败：{}", failures.join("；"))
    };
    Err(acp::Error::invalid_params().data(format!(
        "已查询上游未找到 {target}（{misses} 个来源未命中，{} 个来源失败）{details}，当前模型未改变",
        failures.len()
    )))
}

/// 发现与模型选择分别提交，供主会话和子代理设置共用。
pub(crate) struct DiscoveredModel {
    /// 已注册的模型目录键。
    pub model_id: acp::ModelId,
    /// 提交前校验的连接来源与摘要。
    pub source: Option<(String, String)>,
}

impl DiscoveredModel {
    /// 网络等待结束后重新校验来源和可选状态。
    pub(crate) fn validate(&self, agent: &MvpAgent) -> Result<(), acp::Error> {
        if !agent
            .models_manager
            .available()
            .contains_key(&self.model_id)
        {
            return Err(acp::Error::invalid_params().data("目标模型已不可选，请重试"));
        }
        if let Some((source_id, fingerprint)) = &self.source {
            if !crate::util::config::explicit_model_discovery_allowed() {
                return Err(acp::Error::invalid_params().data("查询期间远程访问策略已改变"));
            }
            let source = agent.resolve_model_id(&acp::ModelId::new(source_id.clone()))?;
            if source_fingerprint(&source)?.to_hex().as_str() != fingerprint {
                return Err(acp::Error::invalid_params().data("查询期间上游连接已改变，请重试"));
            }
        }
        Ok(())
    }
}

/// 完成发现后重新校验会话与来源，再复用现有会话切换流程。
pub(crate) async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: DiscoverRequest = parse_params(args)?;
    let sid = acp::SessionId::new(req.session_id);
    let found = discover_model(agent, &sid, &req.model_id, &req.expected_model_id).await?;
    switch_and_report(
        agent,
        sid,
        found.model_id,
        &req.expected_model_id,
        found.source,
    )
    .await
}

/// 查询并注册临时模型，保持主会话的模型选择。
pub(crate) async fn discover_model(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    model_id: &str,
    expected_model_id: &str,
) -> Result<DiscoveredModel, acp::Error> {
    let req = DiscoverRequest {
        session_id: sid.0.to_string(),
        model_id: model_id.to_owned(),
        expected_model_id: expected_model_id.to_owned(),
    };
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
        return Ok(DiscoveredModel {
            model_id: key,
            source: None,
        });
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
    let current_source =
        DiscoverySource::new(source_key.clone(), agent.resolve_model_id(&source_key)?)?;
    let current_connection = connection_fingerprint(&current_source.entry)?;
    let origin_client = handle.origin_client.clone();
    let current_result =
        query_source(agent, &current_source, &req.model_id, origin_client.clone()).await;
    let selected = if matches!(current_result, Ok(true)) {
        current_source
    } else {
        let mut misses = usize::from(matches!(current_result, Ok(false)));
        let mut failures = Vec::new();
        if let Err(error) = current_result {
            failures.push(format!(
                "{}：{error}",
                current_source.display_address(agent)
            ));
        }
        let available = agent.models_manager.available();
        let configured: Vec<String> = agent.cfg.borrow().config_models.keys().cloned().collect();
        let catalog = agent.models_manager.models();
        let mut seen = HashSet::from([current_connection]);
        let mut sources = Vec::new();
        for key in configured {
            let id = acp::ModelId::new(key.clone());
            let Some(entry) = catalog.get(&key) else {
                continue;
            };
            if available.contains_key(&id) && seen.insert(connection_fingerprint(entry)?) {
                let mut candidate = DiscoverySource::new(id, entry.clone())?;
                candidate.require_configured = true;
                sources.push(candidate);
            }
        }
        // 并发查询按配置顺序收集，网络响应快慢不影响首个命中的选择。
        let results = stream::iter(sources.into_iter().map(|source| {
            let target = &req.model_id;
            let origin_client = origin_client.clone();
            async move {
                let result = query_source(agent, &source, target, origin_client).await;
                (source, result)
            }
        }))
        .buffered(4)
        .collect::<Vec<_>>();
        let results = tokio::time::timeout(Duration::from_secs(25), results)
            .await
            .map_err(|_| {
                acp::Error::invalid_params()
                    .data("跨上游查询超时，请检查上游连接后重试，当前模型未改变")
            })?;
        let mut matches = Vec::new();
        for (source, result) in results {
            match result {
                Ok(true) => matches.push(source),
                Ok(false) => misses += 1,
                Err(error) => failures.push(format!("{}：{error}", source.display_address(agent))),
            }
        }
        select_fallback(&req.model_id, matches, misses, failures)?
    };
    selected
        .validate(agent)
        .map_err(|message| acp::Error::invalid_params().data(message))?;
    let source_key = selected.key;
    let before = selected.fingerprint;
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
    Ok(DiscoveredModel {
        model_id: registered,
        source: Some((source_key.0.to_string(), before.to_hex().to_string())),
    })
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

    /// 构造仅用于摘要和选择逻辑的本地来源。
    fn source(key: &str) -> DiscoverySource {
        let mut entry = ModelEntry::fallback(key, &Default::default());
        entry.info.base_url = "http://127.0.0.1:8317/v1".into();
        entry.api_key = Some("fixture-key".into());
        DiscoverySource::new(acp::ModelId::new(key.to_owned()), entry).unwrap()
    }

    #[test]
    fn connection_fingerprint_deduplicates_model_aliases() {
        let a = source("first").entry;
        let mut b = source("second").entry;
        b.info.supports_vision = !a.info.supports_vision;
        b.info.context_window = std::num::NonZeroU64::new(256_000).unwrap();
        b.info.base_url.push('/');
        assert_eq!(
            connection_fingerprint(&a).unwrap(),
            connection_fingerprint(&b).unwrap()
        );
        assert_ne!(
            source_fingerprint(&a).unwrap(),
            source_fingerprint(&b).unwrap()
        );
    }

    #[test]
    fn connection_fingerprint_preserves_auth_and_routing() {
        let entry = source("first").entry;
        let expected = connection_fingerprint(&entry).unwrap();
        let mut variants = vec![entry.clone(); 8];
        variants[0].api_key = Some("another-key".into());
        variants[1].info.api_backend = crate::sampling::ApiBackend::Responses;
        variants[2]
            .info
            .extra_headers
            .insert("X-Tenant".into(), "other".into());
        variants[3]
            .info
            .query_params
            .insert("tenant".into(), "other".into());
        variants[4]
            .info
            .env_http_headers
            .insert("X-Tenant".into(), "TENANT_ENV".into());
        variants[5].mtls_cert_dir = Some("other-identity".into());
        variants[6].api_base_url = Some("http://127.0.0.1:8318/v1".into());
        variants[7].info.auth_scheme = xai_grok_sampler::AuthScheme::XApiKey;
        for variant in variants {
            assert_ne!(expected, connection_fingerprint(&variant).unwrap());
        }
    }

    #[test]
    fn connection_fingerprint_ignores_header_order() {
        let mut a = source("first").entry;
        a.info.extra_headers.insert("X-A".into(), "a".into());
        a.info.extra_headers.insert("X-B".into(), "b".into());
        let mut b = a.clone();
        b.info.extra_headers.reverse();
        assert_eq!(
            connection_fingerprint(&a).unwrap(),
            connection_fingerprint(&b).unwrap()
        );
    }

    #[test]
    fn connection_fingerprint_preserves_case_insensitive_header_overrides() {
        let mut a = source("first").entry;
        a.info
            .extra_headers
            .insert("Authorization".into(), "first-key".into());
        a.info
            .extra_headers
            .insert("authorization".into(), "second-key".into());
        let mut b = a.clone();
        b.info.extra_headers.reverse();
        assert_ne!(
            connection_fingerprint(&a).unwrap(),
            connection_fingerprint(&b).unwrap()
        );
    }

    #[test]
    fn discovery_rejects_global_credentials_for_unconfigured_third_party_auth() {
        let mut entry = source("third-party").entry;
        entry.api_key = None;
        let mut config = xai_grok_sampler::SamplerConfig {
            base_url: entry.info.base_url.clone(),
            api_key: Some("global-or-session-secret".into()),
            ..Default::default()
        };
        assert!(validate_discovery_credentials(&entry, &config).is_err());
        config.api_key = None;
        assert!(validate_discovery_credentials(&entry, &config).is_ok());
        config.api_key = Some("explicit-secret".into());
        entry.api_key = config.api_key.clone();
        assert!(validate_discovery_credentials(&entry, &config).is_ok());
        entry.api_key = None;
        config.base_url = "https://api.x.ai/v1".into();
        assert!(validate_discovery_credentials(&entry, &config).is_ok());
    }

    #[test]
    fn fallback_accepts_unique_match_despite_other_failures() {
        let chosen = select_fallback(
            "target",
            vec![source("second")],
            1,
            vec!["first：HTTP 503".into()],
        )
        .unwrap();
        assert_eq!(chosen.key.0.as_ref(), "second");
    }

    #[test]
    fn fallback_selects_first_match_and_reports_all_misses() {
        let chosen =
            select_fallback("target", vec![source("second"), source("third")], 1, vec![]).unwrap();
        assert_eq!(chosen.key.0.as_ref(), "second");
        let error = select_fallback("target", vec![], 2, vec!["first：HTTP 503".into()])
            .err()
            .unwrap();
        let message = error.data.unwrap().to_string();
        assert!(message.contains("2 个来源未命中，1 个来源失败"));
        assert!(message.contains("first：HTTP 503"));
    }

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

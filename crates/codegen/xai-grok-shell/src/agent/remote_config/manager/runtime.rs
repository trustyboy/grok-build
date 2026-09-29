//! 临时模型只保存保守能力和目录来源，不复制凭据缓存。

use std::sync::{LazyLock, Weak};

use super::*;
use crate::agent::remote_config::resolution::validate_runtime_selection;

/// 跨旧式鉴权入口只共享弱引用，不延长目录生命周期。
static RUNTIME_MANAGERS: LazyLock<RwLock<Vec<Weak<Inner>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub(super) struct RuntimeModel {
    /// 归一到原始目录键，避免临时条目形成引用链。
    source_catalog_id: String,
    /// 本地来源删除后，不接受同名远端条目接管。
    source_was_configured: bool,
    /// 显式认证被删除后不能降级为登录认证。
    source_requires_own_auth: bool,
    /// 仅目标能力，不保存来源的密钥和模型能力。
    info: config::ModelInfo,
}

/// 旧式逐轮鉴权入口也必须识别临时目录，多个来源同名时拒绝。
pub(crate) fn lookup_runtime_model(id: &str) -> Result<Option<ModelEntry>, String> {
    let managers: Vec<_> = RUNTIME_MANAGERS
        .read()
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let mut found = None;
    for inner in managers {
        let manager = ModelsManager { inner };
        if let Some(entry) = manager.runtime_model_entry(id)? {
            if found.is_some() {
                return Err(format!("临时模型 {id} 存在多个运行时来源"));
            }
            found = Some(entry);
        }
    }
    Ok(found)
}

/// 失效条目的旧快照仍标记为临时，防止被保存为默认模型。
pub(crate) fn runtime_entry_metadata(id: &str) -> bool {
    let managers: Vec<_> = RUNTIME_MANAGERS
        .read()
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    managers
        .iter()
        .any(|inner| inner.catalog.read().runtime.contains_key(id))
}

impl ModelsManager {
    /// 登记目标，不切换会话或修改默认模型。
    pub(crate) fn register_runtime_model(
        &self,
        source_catalog_id: &str,
        target_id: &str,
    ) -> Result<acp::ModelId, String> {
        if target_id.is_empty()
            || target_id.len() > 512
            || target_id.chars().any(char::is_whitespace)
            || target_id.chars().any(char::is_control)
        {
            return Err("模型 ID 为空或含无效字符".to_owned());
        }
        let cfg = self.inner.cfg.read();
        let mut cat = self.inner.catalog.write();
        let source_id = if let Some(runtime) = cat.runtime.get(source_catalog_id) {
            self.resolve_runtime_entry(&cfg, &cat, source_catalog_id, runtime)?;
            runtime.source_catalog_id.clone()
        } else {
            source_catalog_id.to_owned()
        };
        let source = cat
            .models
            .get(&source_id)
            .ok_or_else(|| format!("模型来源 {source_id} 已失效"))?;
        validate_runtime_selection(&cfg, &source_id, source, self.is_session_auth())?;
        validate_source_auth(source)?;
        if let Some(runtime) = cat.runtime.get(target_id) {
            if runtime.source_catalog_id != source_id {
                return Err(format!("模型 {target_id} 已绑定其他来源"));
            }
            self.resolve_runtime_entry(&cfg, &cat, target_id, runtime)?;
            return Ok(acp::ModelId::new(target_id.to_owned()));
        }
        let mut info = config::ModelInfo::fallback(target_id);
        info.id = Some(target_id.to_owned());
        info.context_window = std::num::NonZeroU64::new(256_000).unwrap();
        info.supports_vision = false;
        let runtime = RuntimeModel {
            source_catalog_id: source_id.clone(),
            source_was_configured: cfg.config_models.contains_key(&source_id),
            source_requires_own_auth: source.has_own_credentials(),
            info,
        };
        self.resolve_runtime_entry(&cfg, &cat, target_id, &runtime)?;
        cat.runtime.insert(target_id.to_owned(), runtime);
        drop(cat);
        drop(cfg);
        let mut managers = RUNTIME_MANAGERS.write();
        managers.retain(|manager| manager.strong_count() > 0);
        let weak = Arc::downgrade(&self.inner);
        if !managers.iter().any(|manager| manager.ptr_eq(&weak)) {
            managers.push(weak);
        }
        Ok(acp::ModelId::new(target_id.to_owned()))
    }

    /// 非临时 ID 返回 None；已登记但失效的来源明确报错。
    pub(crate) fn runtime_model_entry(&self, id: &str) -> Result<Option<ModelEntry>, String> {
        let cfg = self.inner.cfg.read();
        let cat = self.inner.catalog.read();
        cat.runtime
            .get(id)
            .map(|runtime| self.resolve_runtime_entry(&cfg, &cat, id, runtime))
            .transpose()
    }

    /// 在同一配置、目录快照下解析当前来源并重新检查目标权限。
    fn resolve_runtime_entry(
        &self,
        cfg: &config::Config,
        cat: &CatalogState,
        id: &str,
        runtime: &RuntimeModel,
    ) -> Result<ModelEntry, String> {
        if let Some(error) = cat.runtime_error.as_ref() {
            return Err(format!("临时模型目录的配置不可用：{error}"));
        }
        let source_id = &runtime.source_catalog_id;
        if runtime.source_was_configured && !cfg.config_models.contains_key(source_id) {
            return Err(format!("临时模型 {id} 的来源 {source_id} 已删除"));
        }
        let source = cat
            .models
            .get(source_id)
            .ok_or_else(|| format!("临时模型 {id} 的来源 {source_id} 已失效"))?;
        validate_runtime_selection(cfg, source_id, source, self.is_session_auth())?;
        validate_source_auth(source)?;
        if runtime.source_requires_own_auth && !source.has_own_credentials() {
            return Err(format!("临时模型 {id} 的来源认证已删除"));
        }
        if cfg.grok_com_config.api_key_auth_disabled()
            && source.has_own_credentials()
            && (crate::util::is_xai_api_url(&source.info.base_url)
                || source
                    .api_base_url
                    .as_deref()
                    .is_some_and(crate::util::is_xai_api_url))
        {
            return Err("临时模型来源的 API key 认证已被策略禁止".to_owned());
        }
        let entry = entry_from_source(runtime.info.clone(), source);
        validate_runtime_selection(cfg, id, &entry, self.is_session_auth())?;
        // 使用未过滤目录，防止已禁用或隐藏的别名被重新暴露。
        let raw = config::resolve_model_list(cfg, cat.prefetched.clone());
        if raw
            .iter()
            .chain(cat.models.iter())
            .any(|(key, candidate)| key == id || candidate.info.has_model_id(id))
        {
            return Err(format!("模型 {id} 已有目录条目或同名路由，不能重复发现"));
        }
        Ok(entry)
    }

    /// 有效条目加入投影，失效引用保留以便会话得到明确错误。
    pub(super) fn models_with_runtime(&self) -> IndexMap<String, ModelEntry> {
        let cfg = self.inner.cfg.read();
        let cat = self.inner.catalog.read();
        let mut models = cat.models.clone();
        models.extend(cat.runtime.iter().filter_map(|(id, runtime)| {
            self.resolve_runtime_entry(&cfg, &cat, id, runtime)
                .ok()
                .map(|entry| (id.clone(), entry))
        }));
        models
    }
}

/// 仅继承连接字段，模型能力始终来自独立的保守描述。
fn entry_from_source(mut info: config::ModelInfo, source: &ModelEntry) -> ModelEntry {
    info.base_url = source.info.base_url.clone();
    info.api_backend = source.info.api_backend.clone();
    info.auth_scheme = source.info.auth_scheme;
    info.extra_headers = source.info.extra_headers.clone();
    info.query_params = source.info.query_params.clone();
    info.env_http_headers = source.info.env_http_headers.clone();
    ModelEntry {
        info,
        mtls_cert_dir: source.mtls_cert_dir.clone(),
        api_key: source.api_key.clone(),
        env_key: source.env_key.clone(),
        auth_provider: source.auth_provider.clone(),
        api_base_url: source.api_base_url.clone(),
    }
}

/// 不返回 Result 的兼容入口使用不可发送、无凭据的配置。
pub(super) fn unavailable_entry(id: &str) -> ModelEntry {
    let mut entry = ModelEntry::fallback(id, &config::EndpointsConfig::default());
    entry.info.base_url.clear();
    entry.info.supports_vision = false;
    entry.auth_provider = Some(xai_grok_login::AuthProviderRef::fail_closed(
        "runtime-source-unavailable".to_owned(),
    ));
    entry
}

/// 显式凭据不可用时禁止退回登录凭据或全局密钥。
fn validate_source_auth(source: &ModelEntry) -> Result<(), String> {
    if source.own_credential().is_some() {
        return Ok(());
    }
    if let Some(provider) = source.auth_provider.as_ref() {
        if provider.is_fail_closed() || provider.config.command.trim().is_empty() {
            return Err("临时模型来源的凭据提供者不可用".to_owned());
        }
    } else if source.api_key.is_some() || source.env_key.is_some() {
        return Err("临时模型来源的显式凭据不可用".to_owned());
    }
    Ok(())
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

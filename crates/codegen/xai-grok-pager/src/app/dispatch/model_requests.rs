//! 模型发现和视觉请求的会话绑定、并发保护及确认后提交。

use super::{maybe_drain_queue_and_note_peek, refresh_open_settings_modals};
use crate::app::actions::{Effect, ModelRequestContext, SupportVisionState};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use agent_client_protocol as acp;
use std::sync::Arc;

/// 复用模型切换屏障，阻止并发修改及提示词提前发送。
fn begin_request(app: &mut AppView) -> Option<ModelRequestContext> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        app.show_toast("请先进入会话，再操作当前模型。");
        return None;
    };
    let agent = app.agents.get_mut(&agent_id)?;
    if agent.session.model_switch_pending {
        agent.show_toast("模型设置请求正在处理中，请完成后重试。");
        return None;
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast("请先进入已就绪的会话，再操作当前模型。");
        return None;
    };
    let Some(expected_model_id) = agent.session.models.current.clone() else {
        agent.show_toast("当前模型尚未就绪，请稍后重试。");
        return None;
    };
    let token = Arc::new(());
    agent.pending_model_request = Some(token.clone());
    agent.session.model_switch_pending = true;
    Some(ModelRequestContext {
        agent_id,
        session_id,
        expected_model_id,
        token,
    })
}

/// 发现请求只接受一个无空白的精确 ID。
pub(super) fn discover_and_switch(app: &mut AppView, model_id: acp::ModelId) -> Vec<Effect> {
    if model_id.0.is_empty() || model_id.0.chars().any(char::is_whitespace) {
        app.show_toast("模型发现需要单个无空白的精确模型 ID。");
        return vec![];
    }
    begin_request(app)
        .map(|request| vec![Effect::DiscoverAndSwitchModel { request, model_id }])
        .unwrap_or_default()
}

/// 查询和修改均等待服务端返回有效状态。
pub(super) fn support_vision(app: &mut AppView, enabled: Option<bool>) -> Vec<Effect> {
    begin_request(app)
        .map(|request| vec![Effect::SupportVision { request, enabled }])
        .unwrap_or_default()
}

/// 仅持有同一请求标识的会话才能解除屏障。
fn finish_request(app: &mut AppView, request: &ModelRequestContext) -> bool {
    let Some(agent) = app.agents.get_mut(&request.agent_id) else {
        return false;
    };
    if agent.session.session_id.as_ref() != Some(&request.session_id)
        || !agent
            .pending_model_request
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &request.token))
    {
        return false;
    }
    agent.pending_model_request = None;
    agent.session.model_switch_pending = false;
    true
}

/// 成功且响应属于当前请求时才替换目录和当前模型。
pub(super) fn discover_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    model_id: acp::ModelId,
    result: Result<acp::SessionModelState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let agent = app
        .agents
        .get_mut(&request.agent_id)
        .expect("request agent");
    let result = result.and_then(|models| {
        if agent.session.models.current.as_ref() != Some(&request.expected_model_id) {
            return Err("当前模型已变化，已忽略过期的发现响应。".into());
        }
        if models.current_model_id != model_id
            || !models
                .available_models
                .iter()
                .any(|info| info.model_id == model_id)
        {
            return Err("服务端返回的模型目录与发现目标不一致。".into());
        }
        Ok(models)
    });
    match result {
        Ok(models) => {
            agent.session.models = Some(models).into();
            agent.session.user_model_preference = Some(model_id.clone());
            app.models
                .update_catalog(agent.session.models.available.clone());
            agent.scrollback.push_block(RenderBlock::system(format!(
                "已切换到 {}（仅当前会话，不保存默认模型）",
                agent.session.models.display_name_for(&model_id)
            )));
        }
        Err(error) => {
            agent
                .scrollback
                .push_block(RenderBlock::system(format!("模型发现或切换失败：{error}")));
        }
    }
    refresh_open_settings_modals(app);
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}

/// 只展示服务端确认值，不修改共享模型能力或附件行为。
pub(super) fn support_vision_complete(
    app: &mut AppView,
    request: ModelRequestContext,
    result: Result<SupportVisionState, String>,
) -> Vec<Effect> {
    if !finish_request(app, &request) {
        return vec![];
    }
    let agent = app
        .agents
        .get_mut(&request.agent_id)
        .expect("request agent");
    let result = result.and_then(|state| {
        if agent.session.models.current.as_ref() != Some(&request.expected_model_id)
            || state.model_id != request.expected_model_id.0.as_ref()
        {
            return Err("当前模型已变化，已忽略过期的视觉状态响应。".into());
        }
        Ok(state)
    });
    let message = match result {
        Ok(state) => format!(
            "{} 原生视觉：{}（仅当前会话；不影响附件添加）",
            state.model_id,
            if state.enabled { "on" } else { "off" }
        ),
        Err(error) => format!("视觉状态查询或修改失败：{error}"),
    };
    agent.scrollback.push_block(RenderBlock::system(message));
    maybe_drain_queue_and_note_peek(app, request.agent_id)
}

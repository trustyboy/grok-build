use super::*;
use crate::app::actions::{FastModeAction, FastModeState, ModelRequestContext, SupportVisionState};
use xai_grok_shell::session::subagents_model::{SubagentsModelAction, SubagentsModelState};

/// 初始化有当前模型的会话。
fn model_app() -> AppView {
    let mut app = test_app_with_agent();
    let id = acp::ModelId::new("original");
    app.models
        .available
        .insert(id.clone(), acp::ModelInfo::new(id.clone(), "Original"));
    app.models.set_current(id, None);
    app.agents.get_mut(&AgentId(0)).unwrap().session.models = app.models.clone();
    app
}

/// 从发现 effect 中提取回包上下文。
fn start_discovery(app: &mut AppView) -> ModelRequestContext {
    let effects = dispatch(
        Action::DiscoverAndSwitchModel {
            model_id: acp::ModelId::new("new"),
        },
        app,
    );
    let [Effect::DiscoverAndSwitchModel { request, model_id }] = &effects[..] else {
        panic!("expected discovery, got {effects:?}");
    };
    assert_eq!(model_id.0.as_ref(), "new");
    assert_eq!(request.expected_model_id.0.as_ref(), "original");
    request.clone()
}

/// 构造带运行时标记的成功目录。
fn discovered() -> acp::SessionModelState {
    serde_json::from_value(serde_json::json!({
        "currentModelId": "new",
        "availableModels": [
            {"modelId":"original", "name":"Original"},
            {"modelId":"new", "name":"New", "_meta":{"runtimeDiscovered":true, "totalContextTokens":256000}}
        ]
    }))
    .unwrap()
}

/// 发现成功前不更新目录；并发修改被挡住，排队提示在确认后发送。
#[test]
fn model_request_discovery_success_and_queue() {
    let mut app = model_app();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .apply_context_used(1_000, 128_000);
    let request = start_discovery(&mut app);
    assert!(app.agents[&AgentId(0)].session.model_switch_pending);
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("original")
    );
    assert!(!app.models.available.contains_key(&acp::ModelId::new("new")));
    for action in [
        Action::DiscoverAndSwitchModel {
            model_id: acp::ModelId::new("other"),
        },
        Action::SwitchModel(ModelChoice::new(acp::ModelId::new("original"))),
        Action::SetDefaultModel(acp::ModelId::new("original")),
        Action::SupportVision {
            enabled: Some(true),
        },
        Action::FastMode {
            action: FastModeAction::Toggle,
        },
    ] {
        assert!(dispatch(action, &mut app).is_empty());
    }
    assert!(dispatch(Action::SendPrompt("hello".into()), &mut app).is_empty());
    let effects = dispatch(
        Action::TaskComplete(TaskResult::DiscoverAndSwitchModelComplete {
            request,
            model_id: acp::ModelId::new("new"),
            result: Ok(discovered()),
        }),
        &mut app,
    );
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("new")
    );
    assert_eq!(app.models.current_model_id_str(), Some("original"));
    assert_eq!(
        app.agents[&AgentId(0)]
            .context_state
            .as_ref()
            .unwrap()
            .total,
        256000
    );
    assert!(app.models.is_runtime_discovered(&acp::ModelId::new("new")));
    assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::SendPrompt { .. }))
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::PersistSetting { .. } | Effect::PersistPreferredModel { .. }
    )));
}

/// 发现错误或响应不匹配均保留原选择，不透传模型切换。
#[test]
fn model_request_failure_leaves_state_and_drains_queue() {
    for result in [
        Err("401 unauthorized".into()),
        Ok(acp::SessionModelState::new(
            acp::ModelId::new("wrong"),
            vec![],
        )),
    ] {
        let mut app = model_app();
        let request = start_discovery(&mut app);
        enqueue_local(&mut app, AgentId(0), "hello");
        let effects = dispatch(
            Action::TaskComplete(TaskResult::DiscoverAndSwitchModelComplete {
                request,
                model_id: acp::ModelId::new("new"),
                result,
            }),
            &mut app,
        );
        let agent = &app.agents[&AgentId(0)];
        assert_eq!(
            agent.session.models.current_model_id_str(),
            Some("original")
        );
        assert_eq!(agent.session.models.available.len(), 1);
        assert_eq!(app.models.available.len(), 1);
        assert!(!agent.session.model_switch_pending);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SendPrompt { .. }))
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::SwitchModel { .. }
                | Effect::PersistSetting { .. }
                | Effect::PersistPreferredModel { .. }
        )));
    }
}

/// 重连后的新请求不会被旧回包解除或覆盖。
#[test]
fn model_request_stale_completion_is_ignored() {
    let mut app = model_app();
    let old = start_discovery(&mut app);
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.begin_session_reload(1);
    assert!(agent.pending_model_request.is_none());
    let current = start_discovery(&mut app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::DiscoverAndSwitchModelComplete {
            request: old,
            model_id: acp::ModelId::new("new"),
            result: Ok(discovered()),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let agent = &app.agents[&AgentId(0)];
    assert!(agent.session.model_switch_pending);
    assert!(Arc::ptr_eq(
        agent.pending_model_request.as_ref().unwrap(),
        &current.token
    ));
    assert_eq!(
        agent.session.models.current_model_id_str(),
        Some("original")
    );
}

/// 会话重绑定或当前模型变化使旧响应失效。
#[test]
fn model_request_revalidates_session_and_model() {
    for change_session in [false, true] {
        let mut app = model_app();
        let request = start_discovery(&mut app);
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        if change_session {
            agent.bind_session_id(acp::SessionId::new("replacement"));
            assert!(!agent.session.model_switch_pending);
            assert!(agent.pending_model_request.is_none());
        } else {
            agent
                .session
                .models
                .set_current(acp::ModelId::new("replacement"), None);
        }
        dispatch(
            Action::TaskComplete(TaskResult::DiscoverAndSwitchModelComplete {
                request,
                model_id: acp::ModelId::new("new"),
                result: Ok(discovered()),
            }),
            &mut app,
        );
        assert!(!app.models.available.contains_key(&acp::ModelId::new("new")));
        assert_ne!(
            app.agents[&AgentId(0)]
                .session
                .models
                .current_model_id_str(),
            Some("new")
        );
    }
}

/// 设置页误发 SetDefaultModel 时，临时条目仍只走普通会话切换。
#[test]
fn model_request_runtime_selection_never_persists() {
    let mut app = model_app();
    app.agents.get_mut(&AgentId(0)).unwrap().session.models = Some(discovered()).into();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .models
        .set_current(acp::ModelId::new("original"), None);
    let effects = dispatch(Action::SetDefaultModel(acp::ModelId::new("new")), &mut app);
    assert!(matches!(&effects[..], [Effect::SwitchModel { .. }]));
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("original")
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SwitchModelComplete {
            agent_id: AgentId(0),
            choice: ModelChoice::new(acp::ModelId::new("new")),
            result: Ok(()),
            prev_model_id: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("new")
    );
}

/// 视觉查询和修改共享屏障，成功、失败和错误模型均释放队列。
#[test]
fn model_request_vision_completion_paths() {
    for (enabled, result) in [
        (
            None,
            Ok(SupportVisionState {
                model_id: "original".into(),
                enabled: true,
            }),
        ),
        (
            Some(false),
            Ok(SupportVisionState {
                model_id: "original".into(),
                enabled: false,
            }),
        ),
        (Some(true), Err("denied".into())),
        (
            None,
            Ok(SupportVisionState {
                model_id: "wrong".into(),
                enabled: true,
            }),
        ),
    ] {
        let mut app = model_app();
        let effects = dispatch(Action::SupportVision { enabled }, &mut app);
        let [
            Effect::SupportVision {
                request,
                enabled: actual,
            },
        ] = &effects[..]
        else {
            panic!("expected vision request")
        };
        assert_eq!(*actual, enabled);
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .model_changed_during_switch = true;
        enqueue_local(&mut app, AgentId(0), "hello");
        let effects = dispatch(
            Action::TaskComplete(TaskResult::SupportVisionComplete {
                request: request.clone(),
                result,
            }),
            &mut app,
        );
        assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
        assert!(
            !app.agents[&AgentId(0)]
                .session
                .models
                .model_changed_during_switch
        );
        assert_eq!(
            app.agents[&AgentId(0)]
                .session
                .models
                .current_model_id_str(),
            Some("original")
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SendPrompt { .. }))
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::PersistSetting { .. } | Effect::PersistPreferredModel { .. }
        )));
    }
}

/// 完成结果回到发起会话，而不是当前正在查看的另一会话。
#[test]
fn model_request_vision_reports_confirmed_value_to_origin() {
    let mut app = model_app();
    let effects = dispatch(
        Action::SupportVision {
            enabled: Some(false),
        },
        &mut app,
    );
    let [Effect::SupportVision { request, .. }] = &effects[..] else {
        panic!("expected vision request")
    };
    insert_placeholder_agent(&mut app, AgentId(1));
    app.active_view = ActiveView::Agent(AgentId(1));
    dispatch(
        Action::TaskComplete(TaskResult::SupportVisionComplete {
            request: request.clone(),
            result: Ok(SupportVisionState {
                model_id: "original".into(),
                enabled: false,
            }),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("原生视觉：off"));
    assert!(app.agents[&AgentId(1)].scrollback.is_empty());
    assert!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_accepts_images()
    );
}

/// 错误详情和错误模型提示必须可见，不能伪装成成功。
#[test]
fn model_request_errors_are_visible() {
    let mut app = model_app();
    let request = start_discovery(&mut app);
    dispatch(
        Action::TaskComplete(TaskResult::DiscoverAndSwitchModelComplete {
            request,
            model_id: acp::ModelId::new("new"),
            result: Err("401 unauthorized".into()),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("401 unauthorized"));
    let effects = dispatch(Action::SupportVision { enabled: None }, &mut app);
    let [Effect::SupportVision { request, .. }] = &effects[..] else {
        panic!("expected vision request")
    };
    dispatch(
        Action::TaskComplete(TaskResult::SupportVisionComplete {
            request: request.clone(),
            result: Ok(SupportVisionState {
                model_id: "other".into(),
                enabled: true,
            }),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("忽略过期"));
}

/// 未知 ID 不能绕过 SetDefaultModel 的目录校验。
#[test]
fn model_request_set_default_still_rejects_unknown() {
    let mut app = model_app();
    assert!(
        dispatch(
            Action::SetDefaultModel(acp::ModelId::new("unknown")),
            &mut app
        )
        .is_empty()
    );
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("original")
    );
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id = None;
    assert!(dispatch(Action::SupportVision { enabled: None }, &mut app).is_empty());
    assert!(
        dispatch(
            Action::DiscoverAndSwitchModel {
                model_id: acp::ModelId::new("new")
            },
            &mut app
        )
        .is_empty()
    );
}

/// 子代理状态的模型不同于主模型，目录包含新发现的条目。
fn subagents_state() -> SubagentsModelState {
    SubagentsModelState {
        model_id: "new".into(),
        model_source: "session".into(),
        reasoning_effort: Some(xai_grok_shell::sampling::types::ReasoningEffort::High),
        effort_source: "parent_model_config".into(),
        context_window: 256000,
        parent_model_id: Some("original".into()),
        parent_configured_model: Some("parent-child".into()),
        parent_configured_effort: Some("high".into()),
        configured_model: Some("configured".into()),
        configured_effort: Some("low".into()),
        warning: Some("配置回退提示".into()),
        available_models: discovered().available_models,
    }
}

/// 提取子代理请求上下文，禁止转换为主模型切换或持久化 effect。
fn start_subagents_model(app: &mut AppView, action: SubagentsModelAction) -> ModelRequestContext {
    let effects = dispatch(
        Action::SubagentsModel {
            action: action.clone(),
        },
        app,
    );
    let [
        Effect::SubagentsModel {
            request,
            action: actual,
        },
    ] = &effects[..]
    else {
        panic!("expected subagents model request: {effects:?}");
    };
    assert_eq!(*actual, action);
    assert_eq!(request.expected_model_id.0.as_ref(), "original");
    request.clone()
}

/// 无参、控制词、已知选择与发现选择均进入同一个服务端接口。
#[test]
fn subagents_model_slash_routes_all_operations() {
    for (input, expected) in [
        ("/subagent-model", SubagentsModelAction::Status),
        ("/subagent-model status", SubagentsModelAction::Status),
        ("/subagent-model inherit", SubagentsModelAction::Inherit),
        ("/subagent-model reset", SubagentsModelAction::Reset),
        (
            "/subagents-model status",
            SubagentsModelAction::Status,
        ),
        (
            "/subagent-model Original",
            SubagentsModelAction::Set {
                model_id: "original".into(),
                reasoning_effort: None,
                context_window: None,
            },
        ),
        (
            "/subagent-model Vendor/Exact-ID",
            SubagentsModelAction::Set {
                model_id: "Vendor/Exact-ID".into(),
                reasoning_effort: None,
                context_window: None,
            },
        ),
    ] {
        let mut app = model_app();
        let effects = dispatch(Action::SendPrompt(input.into()), &mut app);
        assert!(
            matches!(&effects[..], [Effect::SubagentsModel { action, .. }] if *action == expected),
            "{input}: {effects:?}"
        );
        assert_eq!(app.models.current_model_id_str(), Some("original"));
        assert_eq!(
            app.agents[&AgentId(0)]
                .session
                .models
                .current_model_id_str(),
            Some("original")
        );
    }
}

/// 确认后广播目录但不广播选择或提示；不动任何会话的主模型参数与偏好。
#[test]
fn subagents_model_completion_isolated_and_catalog_only() {
    use xai_grok_shell::sampling::types::ReasoningEffort;
    let mut app = model_app();
    app.models.reasoning_effort = Some(ReasoningEffort::Low);
    app.models.context_window_selection = Some(500000);
    app.agents.get_mut(&AgentId(0)).unwrap().session.models = app.models.clone();
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .user_model_preference = Some(acp::ModelId::new("original"));
    let request = start_subagents_model(
        &mut app,
        SubagentsModelAction::Set {
            model_id: "new".into(),
            reasoning_effort: None,
            context_window: None,
        },
    );
    assert!(!app.models.available.contains_key(&acp::ModelId::new("new")));
    insert_placeholder_agent(&mut app, AgentId(1));
    let other = app.agents.get_mut(&AgentId(1)).unwrap();
    other.bind_session_id(acp::SessionId::new("other"));
    other.session.models = app.models.clone();
    app.active_view = ActiveView::Agent(AgentId(1));
    let other_request = start_subagents_model(&mut app, SubagentsModelAction::Status);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SubagentsModelComplete {
            request,
            result: Ok(subagents_state()),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let text = last_system_text(&app, AgentId(0));
    for expected in [
        "默认子代理模型：new",
        "当前会话设置",
        "high",
        "推理强度（effort）：high（来源：主模型专属配置）",
        "256000",
        "新建子代理",
        "配置回退提示",
        "配置模型：configured",
        "当前主模型：original",
        "主模型专属子代理配置：parent-child",
        "主模型专属子代理推理强度：high",
        "配置推理强度：low",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(app.agents[&AgentId(1)].scrollback.is_empty());
    for models in [
        &app.models,
        &app.agents[&AgentId(0)].session.models,
        &app.agents[&AgentId(1)].session.models,
    ] {
        assert_eq!(models.current_model_id_str(), Some("original"));
        assert_eq!(models.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(models.context_window_selection, Some(500000));
        assert!(models.is_runtime_discovered(&acp::ModelId::new("new")));
    }
    assert_eq!(
        app.agents[&AgentId(0)].session.user_model_preference,
        Some(acp::ModelId::new("original"))
    );
    assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
    assert!(app.agents[&AgentId(1)].session.model_switch_pending);
    assert!(Arc::ptr_eq(
        app.agents[&AgentId(1)]
            .pending_model_request
            .as_ref()
            .unwrap(),
        &other_request.token
    ));
}

/// 错误也释放屏障和提示队列，不触发主模型切换或保存默认值。
#[test]
fn subagents_model_completion_unlocks_and_drains_queue() {
    for result in [Ok(subagents_state()), Err("拒绝设置".into())] {
        let failed = result.is_err();
        let mut app = model_app();
        let request = start_subagents_model(&mut app, SubagentsModelAction::Reset);
        for action in [
            Action::SubagentsModel {
                action: SubagentsModelAction::Status,
            },
            Action::SwitchModel(ModelChoice::new(acp::ModelId::new("original"))),
            Action::SetDefaultModel(acp::ModelId::new("original")),
            Action::DiscoverAndSwitchModel {
                model_id: acp::ModelId::new("other"),
            },
            Action::FastMode {
                action: FastModeAction::Toggle,
            },
            Action::SupportVision { enabled: None },
        ] {
            assert!(dispatch(action, &mut app).is_empty());
        }
        assert!(dispatch(Action::SendPrompt("hello".into()), &mut app).is_empty());
        let effects = dispatch(
            Action::TaskComplete(TaskResult::SubagentsModelComplete { request, result }),
            &mut app,
        );
        let agent = &app.agents[&AgentId(0)];
        assert!(!agent.session.model_switch_pending);
        assert!(agent.pending_model_request.is_none());
        assert_eq!(
            agent.session.models.current_model_id_str(),
            Some("original")
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SendPrompt { .. }))
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::SwitchModel { .. }
                | Effect::PersistSetting { .. }
                | Effect::PersistPreferredModel { .. }
        )));
        if failed {
            assert!(agent.scrollback.iter_entries().any(|(_, entry)| matches!(
                &entry.block, RenderBlock::System(message) if message.text.contains("失败：拒绝设置")
            )));
            assert_eq!(agent.session.models.available.len(), 1);
            assert_eq!(app.models.available.len(), 1);
        }
    }
}

/// 重连、会话重绑定与界面关闭后，迟到响应不能更新目录或解锁新请求。
#[test]
fn subagents_model_stale_completion_is_ignored() {
    for change in ["reload", "rebind", "removed"] {
        for result in [Ok(subagents_state()), Err("late failure".into())] {
            let mut app = model_app();
            let old = start_subagents_model(&mut app, SubagentsModelAction::Inherit);
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            if change == "rebind" {
                agent.bind_session_id(acp::SessionId::new("replacement"));
            } else {
                agent.begin_session_reload(1);
            }
            let current = start_subagents_model(&mut app, SubagentsModelAction::Status);
            let scrollback_len = app.agents[&AgentId(0)].scrollback.len();
            if change == "removed" {
                app.agents.shift_remove(&AgentId(0));
            }
            let effects = dispatch(
                Action::TaskComplete(TaskResult::SubagentsModelComplete {
                    request: old,
                    result,
                }),
                &mut app,
            );
            assert!(effects.is_empty());
            assert_eq!(app.models.available.len(), 1);
            if let Some(agent) = app.agents.get(&AgentId(0)) {
                assert_eq!(agent.scrollback.len(), scrollback_len);
                assert!(agent.session.model_switch_pending);
                assert!(Arc::ptr_eq(
                    agent.pending_model_request.as_ref().unwrap(),
                    &current.token
                ));
            }
        }
    }
}

/// 主模型已变化时忽略状态和目录；重复回包不能再次提交。
#[test]
fn subagents_model_revalidates_current_model_and_request_token() {
    let mut app = model_app();
    let request = start_subagents_model(&mut app, SubagentsModelAction::Status);
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .models
        .set_current(acp::ModelId::new("replacement"), None);
    dispatch(
        Action::TaskComplete(TaskResult::SubagentsModelComplete {
            request: request.clone(),
            result: Ok(subagents_state()),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("忽略过期"));
    assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
    assert_eq!(app.models.available.len(), 1);
    let len = app.agents[&AgentId(0)].scrollback.len();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SubagentsModelComplete {
            request,
            result: Ok(subagents_state()),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(app.agents[&AgentId(0)].scrollback.len(), len);
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("replacement")
    );
}

/// 从 Fast effect 提取归属，操作不能在前端丢失或被改成查询。
fn start_fast_mode(app: &mut AppView, action: FastModeAction) -> ModelRequestContext {
    let effects = dispatch(Action::FastMode { action }, app);
    let [
        Effect::FastMode {
            request,
            action: actual,
        },
    ] = &effects[..]
    else {
        panic!("expected fast mode request, got {effects:?}");
    };
    assert_eq!(*actual, action);
    assert_eq!(request.expected_model_id.0.as_ref(), "original");
    request.clone()
}

/// 成功、后端拒绝和错误模型均释放屏障；只显示服务端确认值。
#[test]
fn fast_mode_completion_paths() {
    for (action, result, expected) in [
        (
            FastModeAction::Toggle,
            Ok(FastModeState {
                model_id: "original".into(),
                enabled: true,
                supported: true,
            }),
            "快速模式：已开启",
        ),
        (
            FastModeAction::Off,
            Ok(FastModeState {
                model_id: "original".into(),
                enabled: false,
                supported: true,
            }),
            "快速模式：已关闭",
        ),
        (
            FastModeAction::Status,
            Ok(FastModeState {
                model_id: "original".into(),
                enabled: false,
                supported: false,
            }),
            "仅 Responses API 支持",
        ),
        (
            FastModeAction::On,
            Err("后端拒绝修改".into()),
            "失败：后端拒绝修改",
        ),
        (
            FastModeAction::Status,
            Ok(FastModeState {
                model_id: "wrong".into(),
                enabled: true,
                supported: true,
            }),
            "忽略过期",
        ),
    ] {
        let mut app = model_app();
        let request = start_fast_mode(&mut app, action);
        assert!(app.agents[&AgentId(0)].scrollback.is_empty());
        assert!(
            dispatch(
                Action::FastMode {
                    action: FastModeAction::Status
                },
                &mut app
            )
            .is_empty()
        );
        assert!(
            dispatch(
                Action::SupportVision {
                    enabled: Some(true)
                },
                &mut app
            )
            .is_empty()
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .models
            .model_changed_during_switch = true;
        assert!(dispatch(Action::SendPrompt("hello".into()), &mut app).is_empty());
        let effects = dispatch(
            Action::TaskComplete(TaskResult::FastModeComplete { request, result }),
            &mut app,
        );
        let agent = &app.agents[&AgentId(0)];
        assert!(!agent.session.model_switch_pending);
        assert!(agent.pending_model_request.is_none());
        assert!(!agent.session.models.model_changed_during_switch);
        assert_eq!(
            agent.session.models.current_model_id_str(),
            Some("original")
        );
        assert_eq!(app.models.current_model_id_str(), Some("original"));
        assert!(agent.scrollback.iter_entries().any(|(_, entry)| matches!(
            &entry.block, RenderBlock::System(message) if message.text.contains(expected)
        )));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SendPrompt { .. }))
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::PersistSetting { .. } | Effect::PersistPreferredModel { .. }
        )));
    }
}

/// 查看另一会话不改变结果归属，也不解除另一会话的请求屏障。
#[test]
fn fast_mode_result_stays_with_origin() {
    let mut app = model_app();
    let origin = start_fast_mode(&mut app, FastModeAction::Toggle);
    insert_placeholder_agent(&mut app, AgentId(1));
    let other = app.agents.get_mut(&AgentId(1)).unwrap();
    other.bind_session_id(acp::SessionId::new("other-session"));
    other.session.models = app.models.clone();
    app.active_view = ActiveView::Agent(AgentId(1));
    let other_request = start_fast_mode(&mut app, FastModeAction::Status);
    dispatch(
        Action::TaskComplete(TaskResult::FastModeComplete {
            request: origin,
            result: Ok(FastModeState {
                model_id: "original".into(),
                enabled: true,
                supported: true,
            }),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("快速模式：已开启"));
    assert!(app.agents[&AgentId(1)].scrollback.is_empty());
    assert!(app.agents[&AgentId(1)].session.model_switch_pending);
    assert!(Arc::ptr_eq(
        app.agents[&AgentId(1)]
            .pending_model_request
            .as_ref()
            .unwrap(),
        &other_request.token
    ));
}

/// 旧请求不能解锁重连后的新请求或写入重绑定会话。
#[test]
fn fast_mode_stale_completion_is_ignored() {
    for rebind in [false, true] {
        let mut app = model_app();
        let old = start_fast_mode(&mut app, FastModeAction::Toggle);
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        if rebind {
            agent.bind_session_id(acp::SessionId::new("replacement"));
        } else {
            agent.begin_session_reload(1);
        }
        let current = start_fast_mode(&mut app, FastModeAction::Status);
        let scrollback_len = app.agents[&AgentId(0)].scrollback.len();
        let effects = dispatch(
            Action::TaskComplete(TaskResult::FastModeComplete {
                request: old,
                result: Ok(FastModeState {
                    model_id: "original".into(),
                    enabled: true,
                    supported: true,
                }),
            }),
            &mut app,
        );
        assert!(effects.is_empty());
        let agent = &app.agents[&AgentId(0)];
        assert_eq!(agent.scrollback.len(), scrollback_len);
        assert!(agent.session.model_switch_pending);
        assert!(Arc::ptr_eq(
            agent.pending_model_request.as_ref().unwrap(),
            &current.token
        ));
    }
}

/// 当前模型变化时，响应即使匹配发起模型也不能显示成功。
#[test]
fn fast_mode_revalidates_current_model() {
    let mut app = model_app();
    let request = start_fast_mode(&mut app, FastModeAction::Status);
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .models
        .set_current(acp::ModelId::new("replacement"), None);
    dispatch(
        Action::TaskComplete(TaskResult::FastModeComplete {
            request,
            result: Ok(FastModeState {
                model_id: "original".into(),
                enabled: true,
                supported: true,
            }),
        }),
        &mut app,
    );
    assert!(last_system_text(&app, AgentId(0)).contains("忽略过期"));
    assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
    assert_eq!(
        app.agents[&AgentId(0)]
            .session
            .models
            .current_model_id_str(),
        Some("replacement")
    );
}

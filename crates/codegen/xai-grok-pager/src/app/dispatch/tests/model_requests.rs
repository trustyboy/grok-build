use super::*;
use crate::app::actions::{ModelRequestContext, SupportVisionState};

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
            {"modelId":"new", "name":"New", "_meta":{"runtimeDiscovered":true}}
        ]
    }))
    .unwrap()
}

/// 发现成功前不更新目录；并发修改被挡住，排队提示在确认后发送。
#[test]
fn model_request_discovery_success_and_queue() {
    let mut app = model_app();
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
        Action::SwitchModel {
            model_id: acp::ModelId::new("original"),
            effort: None,
        },
        Action::SetDefaultModel(acp::ModelId::new("original")),
        Action::SupportVision {
            enabled: Some(true),
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
            model_id: acp::ModelId::new("new"),
            effort: None,
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
        enqueue_local(&mut app, AgentId(0), "hello");
        let effects = dispatch(
            Action::TaskComplete(TaskResult::SupportVisionComplete {
                request: request.clone(),
                result,
            }),
            &mut app,
        );
        assert!(!app.agents[&AgentId(0)].session.model_switch_pending);
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

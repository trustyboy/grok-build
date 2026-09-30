use axum::{Json, Router, extract::Request, http::StatusCode, response::Redirect, routing::get};
use xai_grok_sampler::AuthScheme;
use xai_grok_sampler::{SamplerConfig, SamplingClient};

/// 本地列表服务不执行任何推理请求。
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn discovery_preserves_auth_headers_and_query_but_ignores_remote_routing() {
    let router = Router::new().route("/v1/models", get(|req: Request| async move {
        assert_eq!(req.headers()["authorization"], "Bearer fixture-key");
        assert_eq!(req.headers()["x-tenant"], "fixture-tenant");
        assert_eq!(req.uri().query(), Some("keep=yes&version=new"));
        Json(serde_json::json!({"data":[
            {"id":"vendor/new-model", "baseUrl":"https://do-not-follow.invalid", "apiKey":"ignored"},
            {"id":" spaced id "}, {"name":"not-an-id"}
        ]}))
    }));
    let (base, task) = serve(router).await;
    let mut cfg = SamplerConfig {
        base_url: format!("{base}?version=old&keep=yes"),
        api_key: Some("fixture-key".into()),
        ..Default::default()
    };
    cfg.extra_headers
        .insert("x-tenant".into(), "fixture-tenant".into());
    cfg.query_params.insert("version".into(), "new".into());
    assert_eq!(
        SamplingClient::discover_model_ids(cfg).await.unwrap(),
        ["vendor/new-model"]
    );
    task.abort();
}

#[tokio::test]
async fn discovery_supports_x_api_key_and_verbatim_header_overrides() {
    let router = Router::new().route(
        "/v1/models",
        get(|req: Request| async move {
            assert_eq!(req.headers()["x-api-key"], "header-key");
            assert!(!req.headers().contains_key("authorization"));
            Json(serde_json::json!({"data":[]}))
        }),
    );
    let (base, task) = serve(router).await;
    let mut cfg = SamplerConfig {
        base_url: base,
        api_key: Some("initial-key".into()),
        auth_scheme: AuthScheme::XApiKey,
        ..Default::default()
    };
    cfg.extra_headers
        .insert("x-api-key".into(), "header-key".into());
    assert!(
        SamplingClient::discover_model_ids(cfg)
            .await
            .unwrap()
            .is_empty()
    );
    task.abort();
}

/// 发现请求必须等待刷新，并与推理共用动态认证来源。
#[tokio::test]
async fn discovery_uses_prepared_bearer_for_both_auth_schemes() {
    #[derive(Debug)]
    struct Resolver(std::sync::atomic::AtomicBool);

    impl xai_grok_sampler::BearerResolver for Resolver {
        fn current_bearer(&self) -> Option<String> {
            Some(
                if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    "fresh-key"
                } else {
                    "unprepared-key"
                }
                .to_owned(),
            )
        }

        fn prepare_for_send(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        }
    }

    for scheme in [AuthScheme::Bearer, AuthScheme::XApiKey] {
        let (base, task) = serve(Router::new().route(
            "/v1/models",
            get(move |req: Request| async move {
                match scheme {
                    AuthScheme::Bearer => {
                        assert_eq!(req.headers()["authorization"], "Bearer fresh-key");
                        assert!(!req.headers().contains_key("x-api-key"));
                    }
                    AuthScheme::XApiKey => {
                        assert_eq!(req.headers()["x-api-key"], "fresh-key");
                        assert!(!req.headers().contains_key("authorization"));
                    }
                }
                Json(serde_json::json!({"data":[{"id":"prepared"}]}))
            }),
        ))
        .await;
        assert_eq!(
            SamplingClient::discover_model_ids(SamplerConfig {
                base_url: base,
                api_key: Some("stale-key".into()),
                auth_scheme: scheme,
                bearer_resolver: Some(std::sync::Arc::new(Resolver(false.into()))),
                ..Default::default()
            })
            .await
            .unwrap(),
            ["prepared"]
        );
        task.abort();
    }
}

/// 无 Content-Length 的分块响应同样受 4 MiB 上限约束。
#[tokio::test]
async fn discovery_rejects_oversized_chunked_body() {
    let (base, task) = serve(Router::new().route(
        "/v1/models",
        get(|| async {
            axum::body::Body::from_stream(futures_util::stream::iter(
                (0..5).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024 * 1024])),
            ))
        }),
    ))
    .await;
    assert!(
        SamplingClient::discover_model_ids(SamplerConfig {
            base_url: base,
            ..Default::default()
        })
        .await
        .unwrap_err()
        .contains("大小限制")
    );
    task.abort();
}

/// 认证刷新也计入发现请求的八秒预算。
#[tokio::test(start_paused = true)]
async fn discovery_bounds_bearer_preparation_to_eight_seconds() {
    #[derive(Debug)]
    struct PendingResolver;

    impl xai_grok_sampler::BearerResolver for PendingResolver {
        fn current_bearer(&self) -> Option<String> {
            None
        }

        fn prepare_for_send(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    let start = tokio::time::Instant::now();
    let error = SamplingClient::discover_model_ids(SamplerConfig {
        base_url: "http://127.0.0.1:1/v1".into(),
        bearer_resolver: Some(std::sync::Arc::new(PendingResolver)),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(error.contains("超时"));
    assert!(start.elapsed() >= std::time::Duration::from_secs(8));
    assert!(start.elapsed() < std::time::Duration::from_secs(9));
}

#[tokio::test]
async fn discovery_rejects_redirects_and_does_not_echo_error_body() {
    let redirected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = redirected.clone();
    let router = Router::new()
        .route(
            "/v1/models",
            get(|| async { Redirect::temporary("/unexpected") }),
        )
        .route(
            "/unexpected",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
    let (base, task) = serve(router).await;
    let error = SamplingClient::discover_model_ids(SamplerConfig {
        base_url: base,
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(error.contains("307"));
    assert!(!redirected.load(std::sync::atomic::Ordering::SeqCst));
    task.abort();

    let (base, task) = serve(Router::new().route(
        "/v1/models",
        get(|| async { (StatusCode::UNAUTHORIZED, "secret-from-upstream") }),
    ))
    .await;
    let error = SamplingClient::discover_model_ids(SamplerConfig {
        base_url: base,
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(error.contains("401"));
    assert!(!error.contains("secret-from-upstream"));
    task.abort();
}

#[tokio::test]
async fn discovery_rejects_non_catalog_and_oversized_response() {
    let (base, task) = serve(Router::new().route(
        "/v1/models",
        get(|| async { Json(serde_json::json!({"models":[]})) }),
    ))
    .await;
    assert!(
        SamplingClient::discover_model_ids(SamplerConfig {
            base_url: base,
            ..Default::default()
        })
        .await
        .unwrap_err()
        .contains("data")
    );
    task.abort();
    let (base, task) = serve(Router::new().route(
        "/v1/models",
        get(|| async { "x".repeat(4 * 1024 * 1024 + 1) }),
    ))
    .await;
    assert!(
        SamplingClient::discover_model_ids(SamplerConfig {
            base_url: base,
            ..Default::default()
        })
        .await
        .unwrap_err()
        .contains("大小限制")
    );
    task.abort();
}

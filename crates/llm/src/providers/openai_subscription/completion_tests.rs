use super::tests::{empty_request, spawn_capturing_endpoint};
use super::*;
use crate::providers::openai_subscription::token_bundle::ChatGptConnection;
use baybo_security::test_support::MemorySecretStore;
use baybo_security::{EncryptionKey, SecretVault};

async fn sharing_model(
    origin: &str,
    enabled: bool,
) -> (OpenAiSubscriptionCompletionModel, OAuthTokenBundle) {
    let store = VaultTokenStore::new(Arc::new(SecretVault::new(
        EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
        Arc::new(MemorySecretStore::new()),
    )));
    let model = OpenAiSubscriptionCompletionModel::new(
        "test-model".into(),
        Some(origin.into()),
        None,
        store.clone(),
        reqwest::Client::builder().no_proxy().build().unwrap(),
        BackgroundRefresh::Disabled,
    );
    let now = chrono::Utc::now().timestamp();
    let bundle = OAuthTokenBundle {
        connection: Some(ChatGptConnection {
            client_id: "oaiapp_test".into(),
            subject: "user".into(),
            scopes: if enabled {
                vec![crate::providers::openai_subscription::oauth::SHARING_SCOPE.into()]
            } else {
                Vec::new()
            },
        }),
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        id_token: "id".into(),
        account_id: Some("old-account-header-must-not-leak".into()),
        expires_at: now + 3600,
        obtained_at: now,
    };
    store.save(&bundle).await.unwrap();
    (model, bundle)
}

#[tokio::test]
async fn sharing_requests_use_public_responses_and_no_codex_headers() {
    let sse = "event: response.completed\ndata: {\"response\":{}}\n\n";
    let (origin, captured) = spawn_capturing_endpoint(sse, "text/event-stream").await;
    let (model, _) = sharing_model(&origin, true).await;
    let mut stream = model
        .stream(empty_request(), Default::default())
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    let request = captured.await.unwrap();
    assert!(request.starts_with("post /responses "));
    assert!(request.contains("authorization: bearer access"));
    assert!(!request.contains("originator:"));
    assert!(!request.contains("chatgpt-account-id:"));
    assert!(!request.contains("openai-beta:"));
    let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
}

#[tokio::test]
async fn missing_plan_permission_blocks_requests_before_network_io() {
    let (model, bundle) = sharing_model("http://127.0.0.1:1", false).await;
    assert!(matches!(
        model.send(&bundle, &json!({}), &HeaderMap::new()).await,
        Err(LlmError::Auth(_))
    ));
    assert!(matches!(
        model.list_remote_models().await,
        Err(LlmError::Auth(_))
    ));
}

#[tokio::test]
async fn sharing_catalog_only_exposes_available_visible_models() {
    let body = r#"{"models":[{"slug":"allowed","visibility":"list"},{"slug":"hidden","visibility":"hide"}]}"#;
    let (origin, captured) = spawn_capturing_endpoint(body, "application/json").await;
    let (model, _) = sharing_model(&origin, true).await;
    let models = model.list_remote_models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "allowed");
    assert!(captured.await.unwrap().starts_with("get /models "));
}

#[tokio::test]
async fn stream_requires_completed_and_accepts_crlf_events() {
    for (body, succeeds) in [
        (
            "event: response.output_text.delta\ndata: {\"delta\":\"partial\"}\n\n",
            false,
        ),
        (
            "event: response.completed\r\ndata: {\"response\":{}}\r\n\r\n",
            true,
        ),
        ("event: response.completed\ndata: invalid-json\n\n", false),
        (
            "event: response.incomplete\ndata: {\"response\":{}}\n\n",
            false,
        ),
    ] {
        let (origin, captured) = spawn_capturing_endpoint(body, "text/event-stream").await;
        let (model, bundle) = sharing_model(&origin, true).await;
        let response = model
            .send(&bundle, &json!({}), &HeaderMap::new())
            .await
            .unwrap();
        let events: Vec<_> = model.adapt_response(response, None).collect().await;
        assert_eq!(events.iter().all(Result::is_ok), succeeds, "{body}");
        captured.await.unwrap();
    }
}

#[test]
fn sharing_limit_is_terminal_but_usage_unavailable_is_retriable() {
    for (code, quota) in [
        ("subscription_sharing_usage_limit_exceeded", true),
        ("subscription_sharing_usage_unavailable", false),
    ] {
        let mut calls = HashMap::new();
        let events = translate_event(
            &mut calls,
            SseEvent {
                event_type: "response.failed".into(),
                data: json!({"response":{"error":{"code":code}}}).to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            matches!(&events[0], Err(LlmError::QuotaExhausted { .. })),
            quota
        );
        assert!(events[0].is_err());
    }
}

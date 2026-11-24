use moon_telegram::{IncomingKind, MAX_TEXT, TelegramClient, TelegramError, truncate_text};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> TelegramClient {
    TelegramClient::new("TEST_TOKEN", 42).with_base_url(server.uri())
}

async fn body_of(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.expect("requests");
    serde_json::from_slice(&reqs[0].body).expect("json body")
}

fn ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": result}))
}

#[tokio::test]
async fn send_text_body_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/sendMessage"))
        .respond_with(ok(json!({"message_id": 77})))
        .mount(&server)
        .await;
    let id = client(&server).send_text("hola").await.unwrap();
    assert_eq!(id, 77);
    let b = body_of(&server).await;
    assert_eq!(b["chat_id"], 42);
    assert_eq!(b["text"], "hola");
    assert!(b.get("parse_mode").is_none());
}

#[tokio::test]
async fn send_with_buttons_has_inline_keyboard() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/sendMessage"))
        .respond_with(ok(json!({"message_id": 5})))
        .mount(&server)
        .await;
    let buttons = vec![
        ("Aprobar".to_string(), "approve:1".to_string()),
        ("Rechazar".to_string(), "reject:1".to_string()),
    ];
    client(&server)
        .send_with_buttons("¿Ok?", &buttons)
        .await
        .unwrap();
    let b = body_of(&server).await;
    assert_eq!(
        b["reply_markup"],
        json!({"inline_keyboard": [[
            {"text": "Aprobar", "callback_data": "approve:1"},
            {"text": "Rechazar", "callback_data": "reject:1"}
        ]]})
    );
}

#[tokio::test]
async fn edit_text_removes_keyboard() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .respond_with(ok(json!(true)))
        .mount(&server)
        .await;
    client(&server).edit_text(9, "listo").await.unwrap();
    let b = body_of(&server).await;
    assert_eq!(b["message_id"], 9);
    assert_eq!(b["chat_id"], 42);
    assert_eq!(b["reply_markup"], json!({"inline_keyboard": []}));
}

#[tokio::test]
async fn answer_callback_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/answerCallbackQuery"))
        .respond_with(ok(json!(true)))
        .mount(&server)
        .await;
    client(&server).answer_callback("cb1", "ok").await.unwrap();
    let b = body_of(&server).await;
    assert_eq!(b["callback_query_id"], "cb1");
    assert_eq!(b["text"], "ok");
}

#[tokio::test]
async fn poll_parses_command_and_callback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/getUpdates"))
        .respond_with(ok(json!([
            {"update_id": 10, "message": {"message_id": 1, "chat": {"id": 42}, "text": "/estado"}},
            {"update_id": 11, "callback_query": {"id": "cb9", "from": {"id": 42}, "data": "approve:1",
                "message": {"message_id": 33, "chat": {"id": 42}}}},
            {"update_id": 12, "message": {"message_id": 2, "chat": {"id": 42}, "text": "hola"}}
        ])))
        .mount(&server)
        .await;
    let ups = client(&server).poll(10, 0).await.unwrap();
    assert_eq!(ups.len(), 3);
    assert_eq!(ups[0].update_id, 10);
    assert_eq!(
        ups[0].kind,
        IncomingKind::Command {
            name: "estado".into(),
            args: "".into()
        }
    );
    assert_eq!(
        ups[1].kind,
        IncomingKind::Callback {
            id: "cb9".into(),
            data: "approve:1".into(),
            message_id: Some(33)
        }
    );
    assert_eq!(ups[2].kind, IncomingKind::Other);
    let b = body_of(&server).await;
    assert_eq!(b["offset"], 10);
    assert_eq!(b["timeout"], 0);
    assert_eq!(b["allowed_updates"], json!(["message", "callback_query"]));
}

#[tokio::test]
async fn poll_ignores_other_chats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/getUpdates"))
        .respond_with(ok(json!([
            {"update_id": 20, "message": {"message_id": 1, "chat": {"id": 999}, "text": "/pausa"}},
            {"update_id": 21, "callback_query": {"id": "x", "data": "approve:1",
                "message": {"message_id": 3, "chat": {"id": 999}}}}
        ])))
        .mount(&server)
        .await;
    let ups = client(&server).poll(0, 0).await.unwrap();
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].update_id, 20);
    assert_eq!(ups[0].kind, IncomingKind::Other);
    assert_eq!(ups[1].kind, IncomingKind::Other);
}

#[tokio::test]
async fn callbacks_from_another_user_are_ignored() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/getUpdates"))
        .respond_with(ok(json!([
            {"update_id": 30, "callback_query": {"id": "a", "from": {"id": 7}, "data": "approve:1",
                "message": {"message_id": 3, "chat": {"id": 42}}}},
            {"update_id": 31, "callback_query": {"id": "b", "data": "approve:1",
                "message": {"message_id": 3, "chat": {"id": 42}}}}
        ])))
        .mount(&server)
        .await;
    let ups = client(&server).poll(0, 0).await.unwrap();
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].kind, IncomingKind::Other);
    assert_eq!(ups[1].kind, IncomingKind::Other);
}

#[tokio::test]
async fn malformed_update_does_not_block_the_rest() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/getUpdates"))
        .respond_with(ok(json!([
            {"update_id": 40, "message": {"message_id": "not-a-number", "chat": 5}},
            {"update_id": "garbage", "message": {"message_id": 1, "chat": {"id": 42}, "text": "/x"}},
            {"message": {"message_id": 1, "chat": {"id": 42}, "text": "/y"}},
            {"update_id": 42, "message": {"message_id": 2, "chat": {"id": 42}, "text": "/estado"}}
        ])))
        .mount(&server)
        .await;
    let ups = client(&server).poll(0, 0).await.unwrap();
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].update_id, 40);
    assert_eq!(ups[0].kind, IncomingKind::Other);
    assert_eq!(ups[1].update_id, 42);
    assert!(matches!(ups[1].kind, IncomingKind::Command { .. }));
    // The caller derives the next offset from the highest readable id.
    assert_eq!(ups.iter().map(|u| u.update_id).max(), Some(42));
}

#[tokio::test]
async fn command_strips_bot_suffix_and_args() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/getUpdates"))
        .respond_with(ok(json!([
            {"update_id": 1, "message": {"message_id": 1, "chat": {"id": 42}, "text": "/estado@moon_bot arg uno"}}
        ])))
        .mount(&server)
        .await;
    let ups = client(&server).poll(0, 0).await.unwrap();
    assert_eq!(
        ups[0].kind,
        IncomingKind::Command {
            name: "estado".into(),
            args: "arg uno".into()
        }
    );
}

#[tokio::test]
async fn api_error_maps() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/sendMessage"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"ok": false, "error_code": 400, "description": "Bad Request: chat not found"}),
        ))
        .mount(&server)
        .await;
    let err = client(&server).send_text("x").await.unwrap_err();
    match err {
        TelegramError::Api { code, description } => {
            assert_eq!(code, 400);
            assert_eq!(description, "Bad Request: chat not found");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn http_error_display_hides_token() {
    // Nothing listens on this port: connection error embeds the URL unless stripped.
    let c = TelegramClient::new("TEST_TOKEN", 42).with_base_url("http://127.0.0.1:1");
    let err = c.send_text("x").await.unwrap_err();
    assert!(matches!(err, TelegramError::Http(_)));
    assert!(!err.to_string().contains("TEST_TOKEN"));
    assert!(!format!("{err:?}").contains("TEST_TOKEN"));
}

#[test]
fn truncate_long_text() {
    let long = "á".repeat(5000);
    let t = truncate_text(&long);
    assert_eq!(t.chars().count(), MAX_TEXT);
    assert!(t.ends_with('…'));
    assert_eq!(truncate_text("corto"), "corto");
    let exact = "a".repeat(MAX_TEXT);
    assert_eq!(truncate_text(&exact), exact);
}

#[test]
fn debug_hides_token() {
    let c = TelegramClient::new("TEST_TOKEN", 42);
    let s = format!("{c:?}");
    assert!(!s.contains("TEST_TOKEN"));
    assert!(s.contains("***"));
}

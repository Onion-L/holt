mod common;

use base64::Engine as _;
use common::{Fixture, ScriptedProvider, ScriptedReply};
use holt_rpc::{RpcReply, RpcService, methods};
use pi_core::ai::types::{BlockContent, Message};
use serde_json::json;

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

#[tokio::test]
async fn ordinary_text_that_quotes_an_image_tool_result_is_not_an_error() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project_dir.path().join("log.txt"),
        "Read image file [image/png]\nexample output",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("text", "read", json!({"path":"log.txt"})),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read log.txt").await;
    drained(&engine).await;
    let requests = provider.requests();
    let result = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    assert!(
        matches!(&result.content[0], BlockContent::Text(t) if t.text == "Read image file [image/png]\nexample output")
    );
}

async fn drained(engine: &holt_engine::LocalEngine) {
    let RpcReply::Stream(mut watch) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!("queue watch")
    };
    loop {
        let queue = common::next_frame(&mut watch).await;
        if queue["activeMessageId"].is_null() && queue["pending"] == json!([]) {
            return;
        }
    }
}

#[tokio::test]
async fn read_resizes_model_input_and_replays_the_actual_image_after_restart() {
    let fixture = Fixture::new();
    let source = png(4096, 1024);
    let path = fixture.project_dir.path().join("wide.png");
    std::fs::write(&path, &source).unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("image-read", "read", json!({"path":path})),
        ScriptedReply::text("viewed"),
        ScriptedReply::text("remembered"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    engine
        .handle(methods::READ_IMAGE, json!({"path":path}))
        .await
        .unwrap();
    assert!(
        provider.requests().is_empty(),
        "preview never invokes the model"
    );
    common::run_prompt(
        &engine,
        "chat-1",
        &fixture.cwd(),
        &format!("Referenced paths:\n- {}", path.display()),
    )
    .await;
    drained(&engine).await;
    let requests = provider.requests();
    let result = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    let hints = result
        .content
        .iter()
        .filter_map(|b| match b {
            BlockContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        hints.contains("4096x1024") && hints.contains("2048x512"),
        "{hints}"
    );
    let image = result
        .content
        .iter()
        .find_map(|b| match b {
            BlockContent::Image(i) => Some(i),
            _ => None,
        })
        .unwrap();
    assert!(wire(&requests[1]).to_string().contains("input_image"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2048, 512));
    assert_eq!(std::fs::read(&path).unwrap(), source);
    let saved = result.clone();
    drop(engine);
    let engine = fixture.engine(&provider);
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "remember the image").await;
    drained(&engine).await;
    let replay = provider.requests()[2]
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .expect("replayed tool result");
    assert_eq!(replay.content, saved.content);
}

fn wire(request: &common::RecordedRequest) -> serde_json::Value {
    let context = pi_core::ai::types::Context {
        messages: request.messages.clone(),
        ..Default::default()
    };
    json!(
        pi_core::ai::api::openai_responses_shared::convert_responses_messages(
            &request.core_model,
            &context,
            &Default::default(),
            None
        )
        .unwrap()
    )
}

async fn value(
    engine: &holt_engine::LocalEngine,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let RpcReply::Value(value) = engine.handle(method, params).await.unwrap() else {
        panic!("value reply")
    };
    value
}

async fn stage(engine: &holt_engine::LocalEngine, bytes: &[u8]) -> String {
    value(
        engine,
        methods::STAGE_IMAGE,
        json!({"data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
    .await["path"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn run_model(
    engine: &holt_engine::LocalEngine,
    fixture: &Fixture,
    id: &str,
    model: &str,
    prompt: &str,
) {
    engine.handle(methods::QUEUE_COMMAND, json!({"chatId":"chat-1","command":{"kind":"run","messageId":id,"request":{"prompt":prompt,"provider":"openai","model":model,"cwd":fixture.cwd()}}})).await.unwrap();
}

#[tokio::test]
async fn unknown_custom_models_attempt_images_and_report_rejections_without_retrying() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::tool_call("view", "read", json!({"path":"image.png"})),
        ScriptedReply::text("Context summary"),
        ScriptedReply::Failed("This model rejects image input".into()),
    ]);
    std::fs::write(fixture.project_dir.path().join("image.png"), png(4, 2)).unwrap();
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    value(
        &engine,
        methods::ADD_PROVIDER_MODEL,
        json!({"providerId":"openai","modelId":"custom-image-model"}),
    )
    .await;
    let models = value(
        &engine,
        methods::LIST_MODELS,
        json!({"providerId":"openai"}),
    )
    .await;
    let custom = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "openai/custom-image-model")
        .unwrap();
    assert_eq!(custom["imageCapability"], "unknown");
    run_model(
        &engine,
        &fixture,
        "custom",
        "openai/custom-image-model",
        "Read image.png",
    )
    .await;
    drained(&engine).await;
    let requests: Vec<_> = provider
        .requests()
        .into_iter()
        .filter(|r| r.tools > 0)
        .collect();
    assert_eq!(requests.len(), 2, "no silent text-only retry");
    assert!(requests.iter().all(|r| r.model == "custom-image-model"));
    assert!(wire(&requests[1]).to_string().contains("input_image"));
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    assert!(
        transcript
            .to_string()
            .contains("This model rejects image input")
    );
    let models = value(
        &engine,
        methods::LIST_MODELS,
        json!({"providerId":"openai"}),
    )
    .await;
    assert_eq!(
        models
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "openai/custom-image-model")
            .unwrap()["imageCapability"],
        "unknown"
    );
}

#[tokio::test]
async fn nonvisual_models_accept_paths_but_image_reads_fail_and_text_reads_work() {
    let fixture = Fixture::new();
    std::fs::write(fixture.project_dir.path().join("image.png"), png(4, 2)).unwrap();
    std::fs::write(
        fixture.project_dir.path().join("notes.txt"),
        "ordinary text",
    )
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![
            common::tool_call("image", "read", json!({"path":"image.png"})),
            common::tool_call("text", "read", json!({"path":"notes.txt"})),
        ]),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let models = value(
        &engine,
        methods::LIST_MODELS,
        json!({"providerId":"openai"}),
    )
    .await;
    let model = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["imageCapability"] == "unsupported")
        .expect("nonvisual catalog model")["id"]
        .as_str()
        .unwrap();
    run_model(
        &engine,
        &fixture,
        "nonvisual",
        model,
        "Referenced paths:\n- \"image.png\"",
    )
    .await;
    drained(&engine).await;
    let requests = provider.requests();
    for result in requests[1].messages.iter().filter_map(|m| match m {
        Message::ToolResult(r) => Some(r),
        _ => None,
    }) {
        if result.tool_call_id == "image" {
            assert!(result.is_error);
            assert!(
                serde_json::to_string(result)
                    .unwrap()
                    .contains("does not support image input")
            );
            assert!(
                !result
                    .content
                    .iter()
                    .any(|b| matches!(b, BlockContent::Image(_)))
            );
        } else {
            assert!(!result.is_error);
        }
    }
}

#[tokio::test]
async fn accepted_managed_images_survive_lost_ack_restart_and_repeated_release() {
    let fixture = Fixture::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::gated(gate.clone(), "interrupted"),
        ScriptedReply::text("done"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    let accepted = stage(&engine, &png(3, 2)).await;
    let abandoned = stage(&engine, &png(2, 2)).await;
    let external = fixture.project_dir.path().join("external.png");
    std::fs::write(&external, png(2, 1)).unwrap();
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "block").await;
    common::wait_for_requests(&provider, 1).await;
    let prompt = format!("Referenced paths:\n- \"{accepted}\"");
    run_model(
        &engine,
        &fixture,
        "accepted-image",
        "openai/gpt-5.4",
        &prompt,
    )
    .await;
    // Lost acknowledgement: retry the same identity with its original payload.
    run_model(
        &engine,
        &fixture,
        "accepted-image",
        "openai/gpt-5.4",
        &prompt,
    )
    .await;
    assert_eq!(
        value(&engine, methods::RELEASE_IMAGE, json!({"path":accepted})).await["released"],
        false
    );
    assert_eq!(
        value(&engine, methods::RELEASE_IMAGE, json!({"path":external})).await["released"],
        false
    );
    engine
        .handle(
            methods::QUEUE_COMMAND,
            json!({"chatId":"chat-1","command":{"kind":"interrupt"}}),
        )
        .await
        .unwrap();
    gate.notify_one();
    // The queue consumer publishes idle after cancellation settles.
    let RpcReply::Stream(mut queue) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!()
    };
    while !common::next_frame(&mut queue).await["activeMessageId"].is_null() {}
    drop(engine);
    let engine = fixture.engine(&provider);
    value(&engine, methods::READ_IMAGE, json!({"path":accepted})).await;
    assert!(!std::path::Path::new(&abandoned).exists());
    assert!(external.exists());
    engine
        .handle(methods::CONTINUE_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap();
    drained(&engine).await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(
        value(&engine, methods::RELEASE_IMAGE, json!({"path":accepted})).await["released"],
        false
    );
    drop(engine);
    let engine = fixture.engine(&provider);
    value(&engine, methods::READ_IMAGE, json!({"path":accepted})).await;
}

#[tokio::test]
async fn invalid_images_and_byte_limits_fail_honestly_without_model_pixels() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::new(vec![
        ScriptedReply::ToolCalls(vec![
            common::tool_call("corrupt", "read", json!({"path":"bad.png"})),
            common::tool_call("large", "read", json!({"path":"large.png"})),
        ]),
        ScriptedReply::text("failed"),
    ]);
    let engine = fixture.engine(&provider);
    common::setup_chat(&engine, "chat-1").await;
    std::fs::write(fixture.project_dir.path().join("bad.png"), &png(2, 2)[..40]).unwrap();
    let large = std::fs::File::create(fixture.project_dir.path().join("large.png")).unwrap();
    large.set_len(25 * 1024 * 1024 + 1).unwrap();
    assert!(
        engine
            .handle(
                methods::STAGE_IMAGE,
                json!({"data":base64::engine::general_purpose::STANDARD.encode(b"invalid")})
            )
            .await
            .is_err()
    );
    common::run_prompt(&engine, "chat-1", &fixture.cwd(), "read both").await;
    drained(&engine).await;
    let requests = provider.requests();
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| {
        r.is_error
            && !r
                .content
                .iter()
                .any(|b| matches!(b, BlockContent::Image(_)))
    }));
}

#[tokio::test]
#[ignore = "uses configured Kimi credentials; set HOLT_IMAGE_LIVE_CREDENTIALS"]
async fn live_kimi_reads_image_pixels() {
    let fixture = Fixture::new();
    let credentials = std::env::var("HOLT_IMAGE_LIVE_CREDENTIALS").expect("credential file path");
    std::fs::copy(
        credentials,
        fixture.data_dir.path().join("provider-credentials.json"),
    )
    .unwrap();
    let engine = holt_engine::LocalEngine::assemble(&holt_engine::EngineConfig {
        data_dir: fixture.data_dir.path().to_path_buf(),
        personal_skills_dir: Some(fixture.personal_dir.path().to_path_buf()),
        stream_fn: None,
        search_backend_resolver: None,
    })
    .unwrap();
    let image = fixture.project_dir.path().join("color.png");
    std::fs::write(&image, png(64, 32)).unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({"op":"createChat","chatId":"chat-1"}),
        )
        .await
        .unwrap();
    let RpcReply::Stream(mut queue) = engine
        .handle(methods::WATCH_MESSAGE_QUEUE, json!({"chatId":"chat-1"}))
        .await
        .unwrap()
    else {
        panic!()
    };
    common::next_frame(&mut queue).await;
    engine.handle(methods::QUEUE_COMMAND, json!({"chatId":"chat-1","command":{"kind":"run","messageId":"live-image","request":{
        "provider":"kimi-coding","model":"kimi-coding/kimi-for-coding","cwd":fixture.cwd(),
        "prompt":format!("Use the read tool to inspect the image at {}. Then reply with only its dominant color, in English.",image.display())
    }}})).await.unwrap();
    use futures::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let q = queue.next().await.unwrap();
            if q["activeMessageId"].is_null() && q["pending"] == json!([]) {
                break;
            }
        }
    })
    .await
    .expect("live provider timeout");
    let transcript = common::transcript_snapshot(&engine, "chat-1").await;
    let raw = transcript.to_string();
    assert!(
        raw.to_ascii_lowercase().contains("red"),
        "live provider did not identify red: {raw}"
    );
    assert!(
        raw.contains("readFile") && raw.contains("\"isError\":false"),
        "successful image read missing: {raw}"
    );
    println!("Kimi live image read succeeded (64x32 red image).");
}

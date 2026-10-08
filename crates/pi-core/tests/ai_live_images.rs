//! Rust entry point for the credential-gated live suite
//! `pi-core/ai/test/images.test.ts` ("Images E2E Tests", 3 TS cases).
//!
//! The shared TS bodies (`basicImageGeneration`, `handleTextAndImageOutput`,
//! `handleImageInput`) run through `pi_core::ai::images::generate_images`
//! with the builtin openrouter images model from
//! `pi_core::ai::providers::builtin::builtin_images_models` (the
//! `getImageModel` catalog read).
//!
//! Gate translated verbatim: `describe.skipIf(!process.env.OPENROUTER_API_KEY)`.
//! Without the key the suite prints `SKIP: ...` and the test passes.
//!
//! Faithfulness note: the TS suite never passes an `apiKey` in the options,
//! so `generateImages` throws "No API key for provider: openrouter" (and the
//! live run fails after retries) even though the gate checks
//! `OPENROUTER_API_KEY`. The Rust port reproduces this exactly — the same
//! error surfaces as `stopReason: "error"` in the returned
//! `AssistantImages` — rather than silently injecting the key.
//!
//! Deviation: vitest `{ retry: 3 }` has no Rust equivalent.

mod common;

use common::live::{live_env, red_circle_base64, skip};
use pi_core::ai::images::generate_images;
use pi_core::ai::providers::builtin::builtin_images_models;
use pi_core::ai::types::{
    BlockContent, ImagesContext, ImagesModel, ImagesStopReason, ModelInput, TextContent,
};

fn images_model() -> ImagesModel {
    builtin_images_models(Default::default())
        .get_model("openrouter", "google/gemini-2.5-flash-image")
        .expect("images model not found: openrouter/google/gemini-2.5-flash-image")
}

fn text_input(text: &str) -> BlockContent {
    BlockContent::Text(TextContent {
        text: text.to_string(),
        ..Default::default()
    })
}

/// basicImageGeneration from images.test.ts.
async fn basic_image_generation(model: &ImagesModel, case: &str) {
    let context = ImagesContext {
        input: vec![text_input(
            "Generate a simple red circle on a plain white background. No text.",
        )],
    };

    let response = generate_images(model, &context, None)
        .await
        .unwrap_or_else(|error| panic!("{case}: generateImages rejected: {error}"));

    assert_eq!(
        response.stop_reason,
        ImagesStopReason::Stop,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "{case}: errorMessage: {:?}",
        response.error_message
    );
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, BlockContent::Image(_))),
        "{case}: image output"
    );
    assert!(response.timestamp > 0, "{case}: timestamp");
}

/// handleTextAndImageOutput from images.test.ts.
async fn handle_text_and_image_output(model: &ImagesModel, case: &str) {
    if !model.output.contains(&ModelInput::Text) {
        println!(
            "Skipping text+image output test - model {} doesn't support text output",
            model.id
        );
        return;
    }

    let context = ImagesContext {
        input: vec![text_input(
            "Generate a red circle and include a brief description of the image.",
        )],
    };

    let response = generate_images(model, &context, None)
        .await
        .unwrap_or_else(|error| panic!("{case}: generateImages rejected: {error}"));

    assert_eq!(
        response.stop_reason,
        ImagesStopReason::Stop,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, BlockContent::Image(_))),
        "{case}: image output"
    );
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, BlockContent::Text(text) if !text.text.trim().is_empty())),
        "{case}: non-empty text output"
    );
}

/// handleImageInput from images.test.ts.
async fn handle_image_input(model: &ImagesModel, case: &str) {
    if !model.input.contains(&ModelInput::Image) {
        println!(
            "Skipping image input test - model {} doesn't support image input",
            model.id
        );
        return;
    }

    let image_content = BlockContent::Image(pi_core::ai::types::ImageContent {
        data: red_circle_base64(),
        mime_type: "image/png".to_string(),
        ..Default::default()
    });

    let context = ImagesContext {
        input: vec![
            text_input("Create a variation of this image with a blue background."),
            image_content,
        ],
    };

    let response = generate_images(model, &context, None)
        .await
        .unwrap_or_else(|error| panic!("{case}: generateImages rejected: {error}"));

    assert_eq!(
        response.stop_reason,
        ImagesStopReason::Stop,
        "{case}: stopReason, error: {:?}",
        response.error_message
    );
    assert!(
        response
            .output
            .iter()
            .any(|item| matches!(item, BlockContent::Image(_))),
        "{case}: image output"
    );
}

#[tokio::test]
async fn openrouter_images_google_gemini_2_5_flash_image() {
    // TS: describe.skipIf(!process.env.OPENROUTER_API_KEY)
    if live_env("OPENROUTER_API_KEY").is_none() {
        skip(
            "OpenRouter Images Provider (google/gemini-2.5-flash-image)",
            "OPENROUTER_API_KEY",
        );
        return;
    }
    let model = images_model();
    basic_image_generation(&model, "OpenRouter Images: should generate a basic image").await;
    handle_text_and_image_output(
        &model,
        "OpenRouter Images: should handle text plus image output",
    )
    .await;
    handle_image_input(&model, "OpenRouter Images: should handle image input").await;
}

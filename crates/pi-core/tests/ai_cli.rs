//! Port of the observable behavior of `pi-core/ai/src/cli.ts` — the
//! `pi-ai` bin entry. There is no TypeScript test suite for cli.ts, so the
//! expected bytes come from the oracle goldens produced by
//! `scripts/oracle/generate-cli-goldens.mts`, which drives the real cli.ts
//! (with a scripted `fetch` for the network-free GitHub Copilot login).
//!
//! TS case → Rust test mapping:
//! - `main()` no args / `help` / `--help` / `-h` usage →
//!   `usage_matches_the_typescript_oracle_for_every_help_spelling`
//! - `!command` falsy check (empty-string argument prints usage) →
//!   `empty_command_argument_prints_usage_like_the_falsy_ts_check`
//! - `list` → `list_matches_the_typescript_oracle`
//! - unknown command throw → `unknown_command_is_reported_like_the_typescript_oracle`
//! - `login <unknown>` throw → `login_with_unknown_provider_id_errors`
//! - `!providerId` falsy check (empty argument opens the menu) →
//!   `login_with_empty_provider_argument_opens_the_menu`
//! - `login` numbered menu → `login_menu_matches_the_typescript_oracle_bytes`,
//!   `login_menu_selects_by_number`, `login_menu_parses_numbers_like_number_parseint`
//! - `answerPrompt` select rendering + `Invalid selection` →
//!   `select_prompts_match_the_typescript_oracle_rendering`
//! - prompt/notify message formats + `Credentials saved` →
//!   `login_renders_prompts_and_notify_events_like_the_typescript_cli`
//! - `loadAuth`/`saveAuth` merge (order preserved, unrelated entries kept,
//!   malformed JSON replaced) → `copilot_login_writes_the_exact_oracle_bytes`,
//!   `invalid_auth_json_is_replaced_like_the_typescript_cli`,
//!   `login_failures_leave_auth_json_untouched`
//! - full `login github-copilot` run (stdout + auth.json bytes) →
//!   `copilot_login_writes_the_exact_oracle_bytes`

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_core::ai::auth::oauth::github_copilot::GitHubCopilotOAuth;
use pi_core::ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthPromptKind, AuthPromptOption, AuthStorageError,
    OAuthAuth, OAuthCredential,
};
use pi_core::ai::cli::{self, CliIo, CliOAuthProvider, run_cli, run_cli_with};
use pi_core::ai::types::FetchFunction;
use pi_core::ai::utils::http::{HttpFetch, HttpFetchError, HttpRequest, HttpResponse};

fn golden(name: &str) -> String {
    let path = format!("{}/tests/goldens/ai/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("missing golden {name}: {error}"))
}

/// The tempdir's `auth.json` path.
fn auth_json(dir: &common::TempDirGuard) -> PathBuf {
    std::path::Path::new(dir.as_ref()).join("auth.json")
}

// ---------------------------------------------------------------------------
// Test seam: scripted stdin, captured stdout/stderr, tempdir auth.json.
// ---------------------------------------------------------------------------

struct ScriptIo {
    input: Mutex<VecDeque<String>>,
    stdout: Mutex<String>,
    stderr: Mutex<String>,
    auth_path: PathBuf,
}

impl ScriptIo {
    fn new(lines: &[&str], auth_path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            input: Mutex::new(lines.iter().map(|line| line.to_string()).collect()),
            stdout: Mutex::new(String::new()),
            stderr: Mutex::new(String::new()),
            auth_path: auth_path.into(),
        })
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl CliIo for ScriptIo {
    // An exhausted script models stdin EOF.
    fn read_line(&self) -> Option<String> {
        self.input.lock().unwrap().pop_front()
    }

    fn write_stdout(&self, text: &str) {
        self.stdout.lock().unwrap().push_str(text);
    }

    fn write_stderr(&self, text: &str) {
        self.stderr.lock().unwrap().push_str(text);
    }

    fn auth_file_path(&self) -> PathBuf {
        self.auth_path.clone()
    }
}

async fn run_with(
    args: &[&str],
    io: Arc<ScriptIo>,
    providers: Vec<CliOAuthProvider>,
) -> Result<(), String> {
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    run_cli_with(&args, io, providers).await
}

async fn run_real(args: &[&str], io: Arc<ScriptIo>) -> Result<(), String> {
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    run_cli(&args, io).await
}

// ---------------------------------------------------------------------------
// Fake OAuth flow: a scripted sequence of prompts and notify events.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Step {
    Prompt(AuthPromptKind),
    Notify(AuthEvent),
}

struct FakeFlow {
    steps: Vec<Step>,
    credential: OAuthCredential,
    answers: Arc<Mutex<Vec<String>>>,
}

impl FakeFlow {
    fn flow(self) -> Arc<dyn OAuthAuth> {
        Arc::new(self)
    }
}

impl OAuthAuth for FakeFlow {
    fn name(&self) -> &str {
        "Fake OAuth"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        let steps = self.steps.clone();
        let credential = self.credential.clone();
        let answers = Arc::clone(&self.answers);
        Box::pin(async move {
            for step in steps {
                match step {
                    Step::Prompt(kind) => {
                        let answer = interaction
                            .prompt(AuthPrompt { signal: None, kind })
                            .await?;
                        answers.lock().unwrap().push(answer);
                    }
                    Step::Notify(event) => interaction.notify(event),
                }
            }
            Ok(credential)
        })
    }

    fn refresh(
        &self,
        _credential: &OAuthCredential,
        _signal: tokio_util::sync::CancellationToken,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not supported".to_string(),
        ))))
    }

    fn to_auth(
        &self,
        _credential: &OAuthCredential,
    ) -> pi_core::ai::auth::types::AuthFuture<
        Result<pi_core::ai::auth::types::ModelAuth, AuthStorageError>,
    > {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not supported".to_string(),
        ))))
    }
}

fn select_step(message: &str, options: &[(&str, &str)]) -> Step {
    Step::Prompt(AuthPromptKind::Select {
        message: message.to_string(),
        options: options
            .iter()
            .map(|(id, label)| AuthPromptOption {
                id: id.to_string(),
                label: label.to_string(),
                description: None,
            })
            .collect(),
    })
}

fn text_step(message: &str, placeholder: Option<&str>) -> Step {
    Step::Prompt(AuthPromptKind::Text {
        message: message.to_string(),
        placeholder: placeholder.map(str::to_string),
    })
}

fn credential() -> OAuthCredential {
    OAuthCredential {
        refresh: "fake-refresh".to_string(),
        access: "fake-access".to_string(),
        expires: 42,
        extra: serde_json::Map::from_iter([(
            "enterpriseUrl".to_string(),
            serde_json::Value::String("example.ghe.com".to_string()),
        )]),
    }
}

fn fake_provider(id: &str, name: &str, flow: Arc<dyn OAuthAuth>) -> CliOAuthProvider {
    CliOAuthProvider {
        id: id.to_string(),
        name: name.to_string(),
        oauth: flow,
    }
}

/// A no-prompt flow that always succeeds and announces itself.
fn succeeding_flow(marker: &str) -> Arc<dyn OAuthAuth> {
    FakeFlow {
        steps: vec![Step::Notify(AuthEvent::Progress {
            message: marker.to_string(),
        })],
        credential: credential(),
        answers: Arc::new(Mutex::new(Vec::new())),
    }
    .flow()
}

// ---------------------------------------------------------------------------
// Scripted HTTP fetch for the real GitHub Copilot flow (same routes as
// tests/ai_oauth_github_copilot.rs).
// ---------------------------------------------------------------------------

type Reply = (u16, Vec<(String, String)>, String);

struct ScriptedFetch {
    handler: Arc<dyn Fn(&str) -> Reply + Send + Sync>,
}

impl HttpFetch for ScriptedFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures::future::BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        let (status, headers, body) = (self.handler)(&request.url);
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from(body))])),
            })
        })
    }
}

fn json_reply(body: serde_json::Value) -> Reply {
    (
        200,
        vec![("content-type".to_string(), "application/json".to_string())],
        serde_json::to_string(&body).unwrap(),
    )
}

fn copilot_routes() -> Arc<dyn Fn(&str) -> Reply + Send + Sync> {
    Arc::new(|url: &str| {
        if url.ends_with("/login/device/code") {
            json_reply(serde_json::json!({
                "device_code": "device-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://github.com/login/device",
                "interval": 1,
                "expires_in": 900,
            }))
        } else if url.ends_with("/login/oauth/access_token") {
            json_reply(serde_json::json!({"access_token": "ghu_refresh_token"}))
        } else if url.contains("/copilot_internal/v2/token") {
            json_reply(serde_json::json!({
                "token": "tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;",
                "expires_at": 9_999_999_999i64,
            }))
        } else if url.ends_with("/models") {
            json_reply(serde_json::json!({"data": []}))
        } else {
            panic!("unexpected fetch URL: {url}")
        }
    })
}

const FROZEN_NOW_MS: i64 = 1_772_966_400_000;

// ---------------------------------------------------------------------------
// Usage / list (cli.ts main: no command, help, --help, -h, list).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_matches_the_typescript_oracle_for_every_help_spelling() {
    let dir = common::create_temp_dir();
    for args in [
        vec!["help"],
        vec!["--help"],
        vec!["-h"],
        vec![], // no command
    ] {
        let io = ScriptIo::new(&[], auth_json(&dir));
        run_real(&args, io.clone()).await.expect("help exits 0");
        assert_eq!(
            io.stdout(),
            golden("cli-help.txt"),
            "usage output for {args:?} must match the oracle"
        );
        assert_eq!(io.stderr(), "");
    }
}

#[tokio::test]
async fn empty_command_argument_prints_usage_like_the_falsy_ts_check() {
    // cli.ts: `!command` sends "" to the usage branch, not the unknown
    // command error.
    let dir = common::create_temp_dir();
    let io = ScriptIo::new(&[], auth_json(&dir));
    run_real(&[""], io.clone())
        .await
        .expect("empty arg exits 0");
    assert_eq!(io.stdout(), golden("cli-help.txt"));
}

#[tokio::test]
async fn login_with_empty_provider_argument_opens_the_menu() {
    // cli.ts: `!providerId` treats "" as absent and shows the menu.
    let dir = common::create_temp_dir();
    let providers = vec![
        fake_provider("alpha", "Alpha One", succeeding_flow("alpha ran")),
        fake_provider("beta", "Beta Two", succeeding_flow("beta ran")),
    ];
    let io = ScriptIo::new(&["99"], auth_json(&dir));
    let error = run_with(&["login", ""], io.clone(), providers)
        .await
        .expect_err("invalid menu choice fails");
    assert_eq!(error, "Unknown provider: ");
    assert_eq!(
        io.stdout(),
        "  1. Alpha One\n  2. Beta Two\nEnter number (1-2): "
    );
}

#[tokio::test]
async fn list_matches_the_typescript_oracle() {
    let dir = common::create_temp_dir();
    let io = ScriptIo::new(&[], auth_json(&dir));
    run_real(&["list"], io.clone()).await.expect("list exits 0");
    assert_eq!(io.stdout(), golden("cli-list.txt"));
    assert_eq!(io.stderr(), "");

    // The list is exactly the builtin providers with OAuth auth.
    let ids = cli::oauth_providers()
        .into_iter()
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "anthropic",
            "github-copilot",
            "kimi-coding",
            "openai-codex",
            "openrouter",
            "radius",
            "xai",
        ]
    );
}

#[tokio::test]
async fn unknown_command_is_reported_like_the_typescript_oracle() {
    let dir = common::create_temp_dir();
    let io = ScriptIo::new(&[], auth_json(&dir));
    let error = run_real(&["bogus"], io.clone())
        .await
        .expect_err("unknown command fails");
    assert_eq!(error, "Unknown command: bogus");
    assert_eq!(io.stdout(), "");
    // The binary prints the oracle stderr line (`main().catch`) for this
    // error and exits 1.
    assert_eq!(
        format!("Error: {error}\n"),
        "Error: Unknown command: bogus\n"
    );
}

// ---------------------------------------------------------------------------
// Provider selection (cli.ts main: login without a provider id).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_menu_matches_the_typescript_oracle_bytes() {
    let dir = common::create_temp_dir();
    // The oracle run feeds "99": menu bytes plus the un-terminated question,
    // then the "Unknown provider: " error.
    let io = ScriptIo::new(&["99"], auth_json(&dir));
    let error = run_real(&["login"], io.clone())
        .await
        .expect_err("out-of-range menu choice fails");
    assert_eq!(io.stdout(), golden("cli-menu.txt"));
    assert_eq!(format!("Error: {error}\n"), golden("cli-menu-error.txt"));
    assert_eq!(error, "Unknown provider: ");
}

#[tokio::test]
async fn login_menu_selects_by_number() {
    let dir = common::create_temp_dir();
    let providers = vec![
        fake_provider("alpha", "Alpha One", succeeding_flow("alpha ran")),
        fake_provider("beta", "Beta Two", succeeding_flow("beta ran")),
    ];
    let io = ScriptIo::new(&["2"], auth_json(&dir));
    run_with(&["login"], io.clone(), providers)
        .await
        .expect("menu selection succeeds");

    assert_eq!(
        io.stdout(),
        "  1. Alpha One\n  2. Beta Two\nEnter number (1-2): beta ran\n\nCredentials saved to auth.json\n"
    );
    // Only the selected provider's credential is stored.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(auth_json(&dir)).unwrap()).unwrap();
    assert!(written.get("beta").is_some());
    assert!(written.get("alpha").is_none());
}

#[tokio::test]
async fn login_menu_parses_numbers_like_number_parseint() {
    // `Number.parseInt(input, 10)`: leading whitespace, optional sign,
    // leading digit run; NaN and out-of-range indices are unknown providers.
    let cases: &[(&str, Option<usize>)] = &[
        ("1", Some(0)),
        ("2extra", Some(1)),
        ("+2", Some(1)),
        (" 2 ", Some(1)),
        ("2.9", Some(1)),
        ("3", None),
        ("0", None),
        ("-1", None),
        ("abc", None),
        ("", None),
    ];
    for (input, expected) in cases {
        let dir = common::create_temp_dir();
        let providers = vec![
            fake_provider("alpha", "Alpha One", succeeding_flow("alpha ran")),
            fake_provider("beta", "Beta Two", succeeding_flow("beta ran")),
        ];
        let io = ScriptIo::new(&[input], auth_json(&dir));
        let result = run_with(&["login"], io.clone(), providers).await;
        match expected {
            Some(index) => {
                result.expect("valid selection succeeds");
                let written: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(auth_json(&dir)).unwrap())
                        .unwrap();
                let stored = if *index == 0 { "alpha" } else { "beta" };
                assert!(written.get(stored).is_some(), "input {input:?}");
            }
            None => {
                let error = result.expect_err("invalid selection fails");
                assert_eq!(error, "Unknown provider: ", "input {input:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// login <provider> dispatch and the prompt/notify formats.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_with_unknown_provider_id_errors() {
    let dir = common::create_temp_dir();
    let io = ScriptIo::new(&[], auth_json(&dir));
    let error = run_real(&["login", "nope"], io.clone())
        .await
        .expect_err("unknown provider fails");
    assert_eq!(error, "Unknown provider: nope");
    assert_eq!(io.stdout(), "");
    assert!(!auth_json(&dir).exists());
}

#[tokio::test]
async fn select_prompts_match_the_typescript_oracle_rendering() {
    let dir = common::create_temp_dir();
    // Same select prompt the openai-codex flow asks; the invalid answer
    // ("5") reproduces the oracle capture byte for byte.
    let flow = FakeFlow {
        steps: vec![select_step(
            "Select OpenAI Codex login method:",
            &[
                ("browser", "Browser login (default)"),
                ("device", "Device code login (headless)"),
            ],
        )],
        credential: credential(),
        answers: Arc::new(Mutex::new(Vec::new())),
    };
    let io = ScriptIo::new(&["5"], auth_json(&dir));
    let error = run_with(
        &["login", "openai-codex"],
        io.clone(),
        vec![fake_provider("openai-codex", "OpenAI Codex", flow.flow())],
    )
    .await
    .expect_err("invalid select answer fails");
    assert_eq!(error, "Invalid selection");
    assert_eq!(io.stdout(), golden("cli-select.txt"));
    assert_eq!(format!("Error: {error}\n"), golden("cli-select-error.txt"));
    assert!(!auth_json(&dir).exists());
}

#[tokio::test]
async fn login_renders_prompts_and_notify_events_like_the_typescript_cli() {
    let dir = common::create_temp_dir();
    let flow = FakeFlow {
        steps: vec![
            select_step(
                "Select login method:",
                &[
                    ("browser", "Browser login (default)"),
                    ("device", "Device code login (headless)"),
                ],
            ),
            Step::Notify(AuthEvent::Info {
                message: "starting login".to_string(),
                links: Vec::new(),
            }),
            text_step("Enterprise URL", Some("company.ghe.com")),
            Step::Notify(AuthEvent::AuthUrl {
                url: "https://auth.example/authorize".to_string(),
                instructions: Some("Complete the consent screen.".to_string()),
            }),
            Step::Prompt(AuthPromptKind::Secret {
                message: "Client secret".to_string(),
                placeholder: Some("s3cret".to_string()),
            }),
            Step::Notify(AuthEvent::AuthUrl {
                url: "https://auth.example/authorize-2".to_string(),
                instructions: None,
            }),
            Step::Prompt(AuthPromptKind::ManualCode {
                message: "Paste the code".to_string(),
                placeholder: None,
            }),
            Step::Notify(AuthEvent::DeviceCode {
                user_code: "ABCD-1234".to_string(),
                verification_uri: "https://verify.example/device".to_string(),
                interval_seconds: Some(5),
                expires_in_seconds: Some(900),
            }),
            Step::Notify(AuthEvent::Progress {
                message: "exchanging tokens".to_string(),
            }),
        ],
        credential: credential(),
        answers: Arc::new(Mutex::new(Vec::new())),
    };
    let answers = Arc::clone(&flow.answers);
    let io = ScriptIo::new(
        &["2", "ent.example.com", "secret-value", "code-value"],
        auth_json(&dir),
    );
    run_with(
        &["login", "fake"],
        io.clone(),
        vec![fake_provider("fake", "Fake Provider", flow.flow())],
    )
    .await
    .expect("login succeeds");

    // Exact rendering: select prompts print a blank line, the message, the
    // numbered options, then the un-terminated question; text, secret, and
    // manual_code prompts all echo `"<message>[ (<placeholder>)]: "`;
    // notify events print the cli.ts strings; the run ends with the save
    // confirmation.
    assert_eq!(
        io.stdout(),
        "\nSelect login method:\n  1. Browser login (default)\n  2. Device code login (headless)\n\
         Enter number (1-2): starting login\n\
         Enterprise URL (company.ghe.com): \nOpen this URL in your browser:\nhttps://auth.example/authorize\nComplete the consent screen.\n\
         Client secret (s3cret): \nOpen this URL in your browser:\nhttps://auth.example/authorize-2\n\
         Paste the code: \nOpen this URL in your browser:\nhttps://verify.example/device\nEnter code: ABCD-1234\n\
         exchanging tokens\n\
         \nCredentials saved to auth.json\n"
    );
    assert_eq!(io.stderr(), "");

    // The select prompt returns the option id; other prompts return the line.
    assert_eq!(
        *answers.lock().unwrap(),
        vec!["device", "ent.example.com", "secret-value", "code-value"]
    );

    // The credential is stored under the provider id, type-tagged like the
    // TS object literal, with extension fields flattened after `expires`.
    let written = std::fs::read_to_string(auth_json(&dir)).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written,
        serde_json::json!({
            "fake": {
                "type": "oauth",
                "refresh": "fake-refresh",
                "access": "fake-access",
                "expires": 42,
                "enterpriseUrl": "example.ghe.com",
            }
        })
    );
}

#[tokio::test]
async fn login_failures_leave_auth_json_untouched() {
    let dir = common::create_temp_dir();
    let existing = "{\n  \"xai\": {\n    \"type\": \"oauth\"\n  }\n}";
    std::fs::write(auth_json(&dir), existing).unwrap();

    let io = ScriptIo::new(&[], auth_json(&dir));
    let error = run_with(
        &["login", "fake"],
        io.clone(),
        vec![fake_provider(
            "fake",
            "Fake Provider",
            Arc::new(FailingFlow),
        )],
    )
    .await
    .expect_err("flow failure propagates");
    assert_eq!(error, "device flow failed");
    assert_eq!(io.stdout(), "");
    assert_eq!(std::fs::read_to_string(auth_json(&dir)).unwrap(), existing);
}

/// A flow whose login always fails, to check error propagation.
struct FailingFlow;

impl OAuthAuth for FailingFlow {
    fn name(&self) -> &str {
        "Failing OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "device flow failed".to_string(),
        ))))
    }

    fn refresh(
        &self,
        _credential: &OAuthCredential,
        _signal: tokio_util::sync::CancellationToken,
    ) -> pi_core::ai::auth::types::AuthFuture<Result<OAuthCredential, AuthStorageError>> {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not supported".to_string(),
        ))))
    }

    fn to_auth(
        &self,
        _credential: &OAuthCredential,
    ) -> pi_core::ai::auth::types::AuthFuture<
        Result<pi_core::ai::auth::types::ModelAuth, AuthStorageError>,
    > {
        Box::pin(std::future::ready(Err(AuthStorageError(
            "not supported".to_string(),
        ))))
    }
}

// ---------------------------------------------------------------------------
// auth.json persistence (cli.ts loadAuth/saveAuth).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn copilot_login_writes_the_exact_oracle_bytes() {
    let dir = common::create_temp_dir();
    let fetch: Arc<ScriptedFetch> = Arc::new(ScriptedFetch {
        handler: copilot_routes(),
    });
    let flow = GitHubCopilotOAuth::new(fetch as FetchFunction, Arc::new(|| FROZEN_NOW_MS));
    let provider = fake_provider("github-copilot", "GitHub Copilot", Arc::new(flow));

    // Fresh login: the empty enterprise-domain answer matches the oracle.
    let io = ScriptIo::new(&[""], auth_json(&dir));
    run_with(&["login", "github-copilot"], io.clone(), vec![provider])
        .await
        .expect("login succeeds");
    assert_eq!(io.stdout(), golden("cli-login-copilot-stdout.txt"));
    assert_eq!(
        std::fs::read_to_string(auth_json(&dir)).unwrap(),
        golden("cli-login-copilot-auth.json"),
        "auth.json bytes must match the oracle saveAuth output"
    );

    // Re-run over a seeded file: the existing key is replaced in place and
    // unrelated entries keep their order and shape (oracle merge golden).
    let seeded: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&golden("cli-login-copilot-auth.json")).unwrap();
    let mut seeded = seeded;
    seeded.insert(
        "xai".to_string(),
        serde_json::json!({
            "type": "oauth",
            "access": "xai-access",
            "refresh": "xai-refresh",
            "expires": 123,
        }),
    );
    seeded.insert(
        "zai".to_string(),
        serde_json::json!({"type": "api_key", "key": "zai-key"}),
    );
    std::fs::write(
        auth_json(&dir),
        serde_json::to_string_pretty(&seeded).unwrap(),
    )
    .unwrap();

    let fetch: Arc<ScriptedFetch> = Arc::new(ScriptedFetch {
        handler: copilot_routes(),
    });
    let flow = GitHubCopilotOAuth::new(fetch as FetchFunction, Arc::new(|| FROZEN_NOW_MS));
    let io = ScriptIo::new(&[""], auth_json(&dir));
    run_with(
        &["login", "github-copilot"],
        io.clone(),
        vec![fake_provider(
            "github-copilot",
            "GitHub Copilot",
            Arc::new(flow),
        )],
    )
    .await
    .expect("second login succeeds");
    assert_eq!(io.stdout(), golden("cli-login-copilot-stdout.txt"));
    assert_eq!(
        std::fs::read_to_string(auth_json(&dir)).unwrap(),
        golden("cli-login-copilot-merge-auth.json"),
        "merge must preserve unrelated entries and rewrite in place"
    );
}

#[tokio::test]
async fn invalid_auth_json_is_replaced_like_the_typescript_cli() {
    let dir = common::create_temp_dir();
    std::fs::write(auth_json(&dir), "{invalid json").unwrap();
    let io = ScriptIo::new(&[], auth_json(&dir));
    run_with(
        &["login", "fake"],
        io.clone(),
        vec![fake_provider("fake", "Fake", succeeding_flow("done"))],
    )
    .await
    .expect("login succeeds over malformed auth.json");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(auth_json(&dir)).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::json!({
            "fake": {
                "type": "oauth",
                "refresh": "fake-refresh",
                "access": "fake-access",
                "expires": 42,
                "enterpriseUrl": "example.ghe.com",
            }
        })
    );
}

#[cfg(unix)]
#[tokio::test]
async fn auth_json_is_written_with_the_default_file_mode() {
    // Parity with cli.ts: `writeFileSync` without a mode creates the file
    // with 0666 & ~umask (typically 0644).
    let dir = common::create_temp_dir();
    let io = ScriptIo::new(&[], auth_json(&dir));
    run_with(
        &["login", "fake"],
        io.clone(),
        vec![fake_provider("fake", "Fake", succeeding_flow("done"))],
    )
    .await
    .expect("login succeeds");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(auth_json(&dir))
        .unwrap()
        .permissions()
        .mode();
    let umask = unsafe { libc::umask(0) };
    unsafe { libc::umask(umask) };
    assert_eq!(mode & 0o777, 0o666 & !u32::from(umask));
}

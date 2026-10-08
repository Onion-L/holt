//! Port of `pi-core/ai/src/cli.ts` — the package's `bin` entry
//! (`npx @earendil-works/pi-ai <command>`).
//!
//! The console/filesystem surface (stdin lines, stdout/stderr writes, and
//! the location of `auth.json`) sits behind the [`CliIo`] seam so tests can
//! drive the CLI with scripted input and captured output; the `pi-ai`
//! binary in `src/bin/` wires real stdio and the cwd-relative `auth.json`.
//!
//! Observable strings (usage text, menu numbering, prompt/notify formats,
//! error messages) are copied verbatim from cli.ts; the goldens under
//! `tests/goldens/ai/cli-*.txt` are produced by
//! `scripts/oracle/generate-cli-goldens.mts`.
//!
//! Known deviations, kept as small as possible:
//! - On stdin EOF mid-prompt the readline `question` callback resolves with
//!   `null` in Node; here [`CliIo::read_line`] yields `None`, which the
//!   prompt code treats as the empty string. The numeric paths (menu and
//!   `select`) parse both to NaN and fail identically; only a flow that
//!   receives a `null` answer and inspects it could tell them apart.
//! - `loadAuth` on a top-level non-object JSON document (e.g. `"5"`)
//!   yields an empty map here, where strict-mode Node would throw a
//!   `Cannot create property ... on number` TypeError. Malformed JSON
//!   matches exactly (empty map).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::ai::auth::types::{
    AuthEvent, AuthFuture, AuthInteraction, AuthPrompt, AuthPromptKind, AuthStorageError,
    Credential, OAuthAuth, OAuthCredential,
};
use crate::ai::providers::builtin::builtin_providers;

/// Port of `AUTH_FILE`: the cwd-relative credential store written by
/// `login`.
pub const AUTH_FILE: &str = "auth.json";

/// One row of the CLI's provider table: a builtin provider that exposes
/// OAuth login (cli.ts's filtered `PROVIDERS`).
pub struct CliOAuthProvider {
    pub id: String,
    pub name: String,
    pub oauth: Arc<dyn OAuthAuth>,
}

/// Port of the `PROVIDERS` constant: builtin providers whose
/// `auth.oauth` is defined.
pub fn oauth_providers() -> Vec<CliOAuthProvider> {
    builtin_providers()
        .into_iter()
        .filter_map(|provider| {
            let oauth = provider.auth().oauth.clone()?;
            Some(CliOAuthProvider {
                id: provider.id().to_string(),
                name: provider.name().to_string(),
                oauth,
            })
        })
        .collect()
}

/// Console and filesystem seam for the CLI. `read_line` mirrors one
/// readline question (the answer without its newline, `None` on EOF);
/// `write_stdout`/`write_stderr` write raw text with no implicit newline
/// and flush, so prompts interleave with log lines exactly like
/// `readline.question`/`console.log`.
pub trait CliIo: Send + Sync + 'static {
    /// Reads one line of stdin without the trailing newline; `None` on EOF.
    fn read_line(&self) -> Option<String>;
    /// Writes raw text to stdout.
    fn write_stdout(&self, text: &str);
    /// Writes raw text to stderr.
    fn write_stderr(&self, text: &str);
    /// Location of `auth.json` (the cwd-relative file for the real binary).
    fn auth_file_path(&self) -> PathBuf;
}

/// Runs the CLI with the real builtin OAuth provider table. `args` are the
/// arguments after the program name (`process.argv.slice(2)`); the error
/// message is what the binary prints as `Error: <message>` before exiting
/// with code 1.
pub async fn run_cli<Io>(args: &[String], io: Arc<Io>) -> Result<(), String>
where
    Io: CliIo,
{
    run_cli_with(args, io, oauth_providers()).await
}

/// `run_cli` with an injectable provider table (tests substitute scripted
/// flows).
pub async fn run_cli_with<Io>(
    args: &[String],
    io: Arc<Io>,
    providers: Vec<CliOAuthProvider>,
) -> Result<(), String>
where
    Io: CliIo,
{
    let io: Arc<dyn CliIo> = io;
    let command = args.first().map(String::as_str);
    match command {
        // `!command` in cli.ts also sends the empty string here.
        None | Some("") | Some("help") | Some("--help") | Some("-h") => {
            let provider_list = providers
                .iter()
                .map(|provider| format!("  {:<20} {}", provider.id, provider.name))
                .collect::<Vec<_>>()
                .join("\n");
            log(
                &io,
                format!(
                    "Usage: npx @earendil-works/pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:\n{provider_list}"
                ),
            );
        }
        Some("list") => {
            for provider in &providers {
                log(&io, format!("{:<20} {}", provider.id, provider.name));
            }
        }
        Some("login") => {
            // `!providerId` in cli.ts: an empty argument falls through to the
            // interactive menu.
            let mut provider_id = args.get(1).filter(|id| !id.is_empty()).cloned();
            if provider_id.is_none() {
                for (index, provider) in providers.iter().enumerate() {
                    log(&io, format!("  {}. {}", index + 1, provider.name));
                }
                let index =
                    prompt_menu_index(&io, format!("Enter number (1-{}): ", providers.len()));
                provider_id = index
                    .and_then(|index| providers.get(index).map(|provider| provider.id.to_string()));
            }
            let known = provider_id
                .as_ref()
                .is_some_and(|id| providers.iter().any(|provider| &provider.id == id));
            if !known {
                return Err(format!(
                    "Unknown provider: {}",
                    provider_id.unwrap_or_default()
                ));
            }
            let provider_id = provider_id.expect("checked above");
            let provider = providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .expect("checked above");
            login(&io, provider).await?;
        }
        Some(command) => return Err(format!("Unknown command: {command}")),
    }
    Ok(())
}

/// Port of `login(providerId)`: runs the provider's OAuth flow through the
/// readline-backed interaction, then merges the credential into `auth.json`.
async fn login(io: &Arc<dyn CliIo>, provider: &CliOAuthProvider) -> Result<(), String> {
    let interaction: Arc<dyn AuthInteraction> = Arc::new(CliAuthInteraction { io: Arc::clone(io) });
    let credential = provider
        .oauth
        .login(interaction)
        .await
        .map_err(|error| error.0)?;
    let mut auth = load_auth(io);
    auth.insert(provider.id.clone(), credential_value(&credential));
    save_auth(io, &auth)?;
    log(io, format!("\nCredentials saved to {AUTH_FILE}"));
    Ok(())
}

/// Serializes the credential the way the TS object literal does:
/// `type: "oauth"` first, then `refresh`/`access`/`expires` and any flow
/// extension fields.
fn credential_value(credential: &OAuthCredential) -> Value {
    serde_json::to_value(Credential::OAuth(credential.clone()))
        .expect("OAuthCredential serializes to JSON")
}

/// Port of `loadAuth`: missing or malformed `auth.json` yields an empty
/// map; key order is preserved so merges rewrite entries in place.
fn load_auth(io: &Arc<dyn CliIo>) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(io.auth_file_path()) else {
        return Map::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Port of `saveAuth`: `JSON.stringify(auth, null, 2)` without a trailing
/// newline, written with the platform default file mode (`writeFileSync`
/// without a mode creates 0666 & ~umask, typically 0644).
fn save_auth(io: &Arc<dyn CliIo>, auth: &Map<String, Value>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(auth).map_err(|error| error.to_string())?;
    let path = io.auth_file_path();
    std::fs::write(&path, text.as_bytes()).map_err(|error| error.to_string())?;
    Ok(())
}

/// `console.log(text)`: one line plus newline.
fn log(io: &Arc<dyn CliIo>, text: String) {
    io.write_stdout(&format!("{text}\n"));
}

/// The readline-backed `AuthInteraction` passed to provider login flows.
struct CliAuthInteraction {
    io: Arc<dyn CliIo>,
}

impl AuthInteraction for CliAuthInteraction {
    /// cli.ts passes a fresh, never-aborted `AbortController` signal.
    fn signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        None
    }

    fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<Result<String, AuthStorageError>> {
        let io = Arc::clone(&self.io);
        Box::pin(std::future::ready(answer_prompt(&io, prompt)))
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                log(&self.io, format!("\nOpen this URL in your browser:\n{url}"));
                if let Some(instructions) = instructions {
                    log(&self.io, instructions);
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                log(
                    &self.io,
                    format!("\nOpen this URL in your browser:\n{verification_uri}"),
                );
                log(&self.io, format!("Enter code: {user_code}"));
            }
            AuthEvent::Info { message, .. } => log(&self.io, message),
            AuthEvent::Progress { message } => log(&self.io, message),
        }
    }
}

/// Port of `answerPrompt`: renders `select` prompts as a numbered menu and
/// echoes every other prompt as `"<message>[ (<placeholder>)]: "`.
fn answer_prompt(io: &Arc<dyn CliIo>, prompt: AuthPrompt) -> Result<String, AuthStorageError> {
    match prompt.kind {
        AuthPromptKind::Select { message, options } => {
            log(io, format!("\n{message}"));
            for (index, option) in options.iter().enumerate() {
                log(io, format!("  {}. {}", index + 1, option.label));
            }
            let index = prompt_menu_index(io, format!("Enter number (1-{}): ", options.len()));
            match index.and_then(|index| options.get(index)) {
                Some(option) => Ok(option.id.clone()),
                None => Err(AuthStorageError("Invalid selection".to_string())),
            }
        }
        kind => {
            let (message, placeholder) = match kind {
                AuthPromptKind::Text {
                    message,
                    placeholder,
                }
                | AuthPromptKind::Secret {
                    message,
                    placeholder,
                }
                | AuthPromptKind::ManualCode {
                    message,
                    placeholder,
                } => (message, placeholder),
                AuthPromptKind::Select { .. } => unreachable!("handled above"),
            };
            let placeholder = placeholder
                .map(|placeholder| format!(" ({placeholder})"))
                .unwrap_or_default();
            io.write_stdout(&format!("{message}{placeholder}: "));
            Ok(io.read_line().unwrap_or_default())
        }
    }
}

/// Writes `question` and reads a numbered answer, returning the 0-based
/// selection; `None` mirrors the NaN/out-of-range path.
fn prompt_menu_index(io: &Arc<dyn CliIo>, question: String) -> Option<usize> {
    io.write_stdout(&question);
    let line = io.read_line().unwrap_or_default();
    js_parse_int_10(&line).and_then(|value| usize::try_from(value - 1).ok())
}

/// `Number.parseInt(input, 10)`: skips leading whitespace, one optional
/// sign, and takes the leading ASCII digit run; `None` stands in for NaN.
/// Values past the i64 range saturate (they are out of any menu range).
fn js_parse_int_10(input: &str) -> Option<i64> {
    let rest = input.trim_start();
    let (negative, rest) = match rest.as_bytes().first() {
        Some(b'+') => (false, &rest[1..]),
        Some(b'-') => (true, &rest[1..]),
        _ => (false, rest),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if digits.is_empty() {
        return None;
    }
    let magnitude = digits.parse::<i64>().unwrap_or(i64::MAX);
    Some(if negative { -magnitude } else { magnitude })
}

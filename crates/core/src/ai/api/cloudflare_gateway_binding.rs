//! Port of `pi-core/ai/src/api/cloudflare-gateway-binding.ts`: AI Gateway
//! transport over the Workers AI binding.
//!
//! pi's Cloudflare AI Gateway support speaks HTTPS
//! (`gateway.ai.cloudflare.com/v1/{account}/{gateway}/{provider}/...`, see
//! [`crate::ai::api::cloudflare`]), which needs a Cloudflare API token even
//! when the caller is a Worker in the gateway's own account.
//!
//! [`create_gateway_binding_fetch`] returns a [`FetchFunction`] that
//! translates requests under a gateway HTTPS prefix into calls to the Workers
//! AI binding's universal endpoint,
//! `env.AI.gateway(id).run({provider, endpoint, headers, query})`. Binding
//! calls are pre-authenticated in-account and return the provider's native
//! wire format as a regular (streaming) response, so API implementations
//! behave identically over either transport.
//!
//! The result is the transport for one gateway-bound client, not a
//! general-purpose fetch: requests it cannot serve — URLs outside the prefix,
//! or in-prefix requests the universal endpoint cannot express (non-POST,
//! non-JSON body) — reject with a descriptive error. Transport selection is
//! the caller's job, per client: route such traffic over HTTPS with real
//! gateway auth instead of through this shim.
//!
//! TypeScript-version behaviors that have no Rust transport equivalent: the
//! fetch `Request`/`init` split (the Rust [`HttpRequest`] is the single,
//! final request form, so init-headers-replace-request-headers and
//! `signal: null` clearing do not apply). The request's abort signal
//! forwards to the binding run like the TypeScript fetch's `init.signal`.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::ai::types::FetchFunction;
use crate::ai::utils::http::{HttpBody, HttpFetch, HttpFetchError, HttpRequest, HttpResponse};

/// Placeholder value for auth headers on binding-routed requests. API
/// implementations require an API key or a recognized auth header
/// (`authorization`, `x-api-key`, `cf-aig-authorization`) before dispatch;
/// binding calls are pre-authenticated, so pass
/// `cf-aig-authorization: Bearer ${CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}`
/// to satisfy the check. The shim strips `cf-aig-authorization` before
/// calling the binding. Pair it with `Authorization: null` / `x-api-key:
/// null` so the SDKs' placeholder auth headers never reach the gateway, which
/// would treat a request-supplied auth header as a BYOK provider key that
/// overrides its stored keys — the same as it would over HTTPS.
pub const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL: &str = "cloudflare-gateway-binding";

/// Never forwarded to the binding: hop-by-hop/derived headers, and gateway
/// auth (binding calls are pre-authenticated; the sentinel must not reach the
/// wire).
const STRIP_HEADERS: [&str; 3] = ["content-length", "host", "cf-aig-authorization"];

/// One universal-endpoint request entry, as accepted by `AiGateway.run()`.
/// Port of `AiGatewayUniversalRequestLike`.
#[derive(Clone, Debug, PartialEq)]
pub struct AiGatewayUniversalRequest {
    pub provider: String,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub query: serde_json::Value,
}

/// Port of `AiGatewayBindingGateway`: the gateway-scoped run surface of the
/// Workers AI binding (`env.AI.gateway(id)`).
pub trait AiGatewayBindingGateway: Send + Sync {
    fn run<'a>(
        &'a self,
        data: AiGatewayUniversalRequest,
        options: AiGatewayRunOptions,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>>;
}

/// Port of the binding run options (`{ signal }`).
#[derive(Clone, Debug, Default)]
pub struct AiGatewayRunOptions {
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// Structural port of the Workers AI binding's gateway surface (`env.AI`).
/// Any real Workers AI binding satisfies it.
pub trait AiGatewayBinding: Send + Sync {
    fn gateway(&self, id: &str) -> Arc<dyn AiGatewayBindingGateway>;
}

/// Port of `GatewayBindingFetchOptions`.
pub struct GatewayBindingFetchOptions {
    /// The Workers AI binding (e.g. `env.AI`).
    pub binding: Arc<dyn AiGatewayBinding>,
    /// Gateway HTTPS prefix every request must fall under, without a trailing
    /// slash: `https://gateway.ai.cloudflare.com/v1/{accountId}/{gatewayName}`.
    pub base_url: String,
    /// Gateway name passed to `binding.gateway()`. Must match the `base_url`
    /// gateway.
    pub gateway: String,
}

/// Create a `fetch` that routes AI Gateway requests through the Workers AI
/// binding. See the module docs for behavior and composition notes. Fails
/// when `base_url` is not a parseable URL (TypeScript throws from `new URL`
/// at creation time).
pub fn create_gateway_binding_fetch(
    options: GatewayBindingFetchOptions,
) -> Result<FetchFunction, String> {
    let base = url::Url::parse(&options.base_url)
        .map_err(|error| format!("createGatewayBindingFetch: invalid base URL: {error}"))?;
    let mut base_path = base.path().to_string();
    if !base_path.ends_with('/') {
        base_path.push('/');
    }
    Ok(Arc::new(GatewayBindingFetch {
        binding: options.binding,
        gateway: options.gateway,
        base_origin: origin_string(&base),
        base_path,
    }))
}

struct GatewayBindingFetch {
    binding: Arc<dyn AiGatewayBinding>,
    gateway: String,
    base_origin: String,
    base_path: String,
}

impl GatewayBindingFetch {
    fn out_of_prefix(&self, method: &str, url: &str) -> HttpFetchError {
        HttpFetchError::Request(format!(
            "createGatewayBindingFetch: {method} {url} is outside the configured gateway prefix ({}{}); this fetch only serves its gateway-bound client",
            self.base_origin, self.base_path
        ))
    }

    fn unexpressible(&self, method: &str, url: &str, reason: &str) -> HttpFetchError {
        HttpFetchError::Request(format!(
            "createGatewayBindingFetch: cannot express {method} {url} as a universal gateway request ({reason}); route it over HTTPS with gateway auth instead"
        ))
    }
}

impl HttpFetch for GatewayBindingFetch {
    fn fetch<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpFetchError>> {
        Box::pin(async move {
            let method = request.method.as_str();
            // Prefix matching runs on URL-normalized components (origin +
            // pathname), not raw strings: dot segments resolve away and
            // fragments drop, matching what real fetch would put on the wire,
            // so a lexical variant can't split provider/endpoint differently
            // than HTTPS would.
            //
            // Out-of-prefix URLs are a configuration bug, not passthrough
            // traffic: silently forwarding would ship the auth sentinel to
            // whatever host the URL names.
            let parsed = url::Url::parse(&request.url).ok();
            let in_prefix = parsed.as_ref().is_some_and(|parsed| {
                origin_string(parsed) == self.base_origin
                    && parsed.path().starts_with(&self.base_path)
            });
            if !in_prefix {
                return Err(self.out_of_prefix(method, &request.url));
            }
            let parsed = parsed.expect("in-prefix URLs parsed successfully");

            // In-prefix requests the universal endpoint cannot express always
            // reject: forwarding them over HTTPS would send the sentinel to
            // the gateway and fail with a misleading auth error instead of
            // naming the real problem. Callers that need such endpoints route
            // them over HTTPS with real gateway auth themselves.
            if method != "POST" {
                return Err(self.unexpressible(method, &request.url, "only POST is supported"));
            }

            let rest = parsed.path()[self.base_path.len()..].to_string();
            // `slash === 0` means an empty provider segment; `None` means no
            // endpoint at all. Both are unexpressible.
            let Some(slash) = rest.find('/').filter(|slash| *slash > 0) else {
                return Err(self.unexpressible(
                    method,
                    &request.url,
                    "missing provider/endpoint path",
                ));
            };
            let provider = rest[..slash].to_string();
            // Keep the query string on the endpoint — it's part of what HTTPS
            // would have sent.
            let endpoint = match parsed.query() {
                Some(query) => format!("{}?{query}", &rest[slash + 1..]),
                None => rest[slash + 1..].to_string(),
            };

            let query = match &request.body {
                HttpBody::Empty => {
                    return Err(self.unexpressible(method, &request.url, "missing body"));
                }
                HttpBody::Json(value) => value.clone(),
                body => {
                    let text = match body {
                        HttpBody::Text(text) => Some(text.as_str()),
                        HttpBody::Bytes(bytes) => std::str::from_utf8(bytes).ok(),
                        HttpBody::Empty | HttpBody::Json(_) => None,
                    };
                    let Some(text) = text.filter(|text| !text.is_empty()) else {
                        return Err(self.unexpressible(method, &request.url, "non-JSON body"));
                    };
                    match serde_json::from_str(text) {
                        Ok(value) => value,
                        Err(_) => {
                            return Err(self.unexpressible(method, &request.url, "non-JSON body"));
                        }
                    }
                }
            };

            // Header names are lowercased so case-variant duplicates collapse
            // and stripping is uniform.
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            for (name, value) in &request.headers {
                let name = name.to_lowercase();
                if STRIP_HEADERS.contains(&name.as_str()) {
                    continue;
                }
                headers.insert(name, value.clone());
            }

            let data = AiGatewayUniversalRequest {
                provider,
                endpoint,
                headers,
                query,
            };
            let gateway = self.binding.gateway(&self.gateway);
            gateway
                .run(
                    data,
                    AiGatewayRunOptions {
                        signal: request.signal.clone(),
                    },
                )
                .await
        })
    }
}

/// WHATWG origin serialization (`scheme://host[:port]`, default ports
/// omitted), matching what `new URL(...).origin` yields in TypeScript.
fn origin_string(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

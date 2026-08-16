//! Authorization middleware for the HTTP server.
//!
//! When `[auth].bearer_token` (or the `AI_MEMORY_AUTH_TOKEN` env var)
//! is set, every request to `/mcp`, `/hook`, `/handoff`, and `/web/*`
//! must present the token via one of three transports:
//!
//! - **Bearer header** (any method): MCP clients + hooks. Required
//!   on all non-GET methods.
//! - **Basic auth** (GET only): browsers — username ignored, token
//!   in the password field. Triggers the native credential dialog
//!   via the `WWW-Authenticate: Basic` challenge in 401 responses.
//! - **Session cookie** (GET only): set automatically after a
//!   successful Basic auth so the browser doesn't re-prompt every
//!   session.
//!
//! When the token is *unset*, the middleware is a no-op — preserving
//! the zero-config local-development experience and keeping the
//! existing e2e + unit tests working.
//!
//! Comparison uses [`subtle::ConstantTimeEq`] so an attacker on the
//! same LAN cannot use response-time leaks to recover the token byte
//! by byte. The constant-time guarantee depends on both sides being
//! the same length; `subtle` returns a constant-cost `Choice::from(0)`
//! when lengths differ, which is the right thing here.
//!
//! Wire shape matches the MCP authorization spec
//! (modelcontextprotocol.io/specification/.../basic/authorization):
//! 401 responses include `WWW-Authenticate: Bearer …` so MCP clients
//! detect missing/expired credentials. GET 401s ALSO include `Basic
//! …` so browsers dialog-prompt automatically.
//!
//! ## Why not OAuth
//!
//! The MCP spec mandates full OAuth 2.1 for HTTP-authenticated
//! servers. That's overkill for a single-user homelab and would
//! force every MCP client config to deal with authorization-server
//! discovery + PKCE + token refresh. A static bearer token is
//! wire-compatible with the spec's `Authorization: Bearer …` shape
//! (clients send the header the same way; they just don't run the
//! OAuth dance to obtain the token). Every supported client
//! (Claude Code, Codex, OpenCode, Cursor, Claude Desktop via
//! `mcp-remote`, Gemini CLI, OpenClaw) accepts a static
//! `Authorization` header in its config.

use std::sync::Arc;

use ai_memory_core::{ActorContext, AuthLevel, IdentityKey};
use ai_memory_store::{ReaderPool, TokenPepper, WriterHandle, hash_token};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::debug;

/// Cookie name used for browser session persistence after a
/// successful Basic auth handshake.
const AUTH_COOKIE: &str = "ai_memory_auth";
/// Realm advertised in `WWW-Authenticate` challenges. Shows up in
/// the browser's credential prompt as "Server says: <realm>".
const AUTH_REALM: &str = "ai-memory";

/// Optional multi-user resolver tier — token-hash lookup against the
/// `users` table. Populated only when both a per-server pepper and a
/// reader pool are available (i.e. after `ai-memory init` ran and a
/// store was opened). Single-user (rung-1) setups skip this entirely;
/// rung-0 (no auth) skips the whole middleware.
#[derive(Clone)]
pub struct MultiUserResolver {
    /// Hashes incoming tokens with the per-server pepper before the
    /// `users.token_hash` lookup.
    pub pepper: TokenPepper,
    /// Read-only pool used by the auth hot path
    /// (`find_active_user_by_token_hash`).
    pub reader: ReaderPool,
    /// Writer used by the fire-and-forget `last_seen_at` bump after a
    /// successful lookup — kept off the response's critical path via
    /// `tokio::spawn`. The bump is best-effort: an error is logged at
    /// `warn` and otherwise ignored.
    pub writer: WriterHandle,
}

/// Shared auth state. Cheap to clone — just an `Arc` wrapping the
/// optional configured token + the optional multi-user resolver +
/// the root actor template.
#[derive(Clone, Default)]
pub struct AuthState {
    /// Bearer token that authenticates as **root**. `None` means
    /// "auth disabled at the wire level" — rung 0.
    expected: Option<String>,
    /// Whether browser session cookies must be marked `Secure`. This is an
    /// explicit operator setting: proxy headers are untrusted input.
    secure_cookie: bool,
    /// Actor template stamped onto requests that authenticate with the
    /// `expected` token. Populated from `[auth].root_username` /
    /// `root_email` / `root_name`. When all three are unset the root
    /// actor stays anonymous (rung-1 backward-compat: bearer
    /// authenticates but the audit log records nothing identifying).
    root_actor: ActorContext,
    /// Multi-user lookup tier. `None` until both pepper and reader
    /// are available — see [`Self::with_multiuser`].
    multiuser: Option<MultiUserResolver>,
    /// Dedicated bearer credential for a trusted authenticating proxy. It is
    /// separate from the root bearer so a missing identity assertion cannot
    /// accidentally turn proxy traffic into root traffic.
    actor_proxy_bearer: Option<String>,
}

impl AuthState {
    /// Build state from the (optional) configured root token. `None`
    /// means "auth disabled, accept everything as anonymous".
    #[must_use]
    pub fn new(expected: Option<String>) -> Self {
        Self {
            // A blank configured value must never turn an absent credential
            // into root authentication (`"" == ""`). Treat placeholders and
            // whitespace-only environment values as auth-disabled instead.
            expected: expected.filter(|token| !token.trim().is_empty()),
            ..Self::default()
        }
    }

    /// Require HTTPS for browser session cookies.
    #[must_use]
    pub fn with_secure_cookie(mut self, secure_cookie: bool) -> Self {
        self.secure_cookie = secure_cookie;
        self
    }

    /// Attach the root actor template — see [`Self::root_actor`]. The
    /// auth middleware injects this on every request that authenticates
    /// with [`Self::expected`]; rung-2 (DB user) lookups override it
    /// with the row's identity, rung-0 (anonymous) leaves it empty.
    #[must_use]
    pub fn with_root_actor(mut self, actor: ActorContext) -> Self {
        self.root_actor = actor;
        self
    }

    /// Does `actor` name the operator configured as root?
    ///
    /// Used to decide whether a proxy-asserted identity keeps root capability.
    /// Only the stable OIDC pair can match; usernames are display data and
    /// never grant root.
    fn asserts_root_identity(&self, actor: &ActorContext) -> bool {
        let Some(root @ IdentityKey::Subject { .. }) = self.root_actor.identity_key() else {
            return false;
        };
        actor
            .identity_key()
            .is_some_and(|asserted| root == asserted)
    }

    /// Trust an authenticating proxy to assert WHO the caller is.
    ///
    /// A proxy that terminates SSO (validating an OIDC token, say) usually
    /// cannot forward the end user's credential upstream: it authenticates to
    /// this server with a dedicated proxy bearer and describes the human in
    /// `X-Memory-Actor-*` headers. Without a way to tell that proxy apart from
    /// any other client, those headers cannot be believed — anyone able to
    /// reach the port could claim to be anyone — so they are ignored by
    /// default.
    ///
    /// Presenting this bearer authenticates only the proxy rung. A blank value
    /// is treated as absent.
    #[must_use]
    pub fn with_trusted_proxy_bearer(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.actor_proxy_bearer = Some(token).filter(|s| !s.trim().is_empty());
        self
    }

    /// Enable multi-user lookups: a bearer that doesn't match the root
    /// token is hashed with `pepper` and checked against the `users`
    /// table; a hit attributes the request to that user's identity.
    /// Without this attach, the middleware only knows about rung 0/1
    /// and rejects unknown bearers (closing the bypass).
    #[must_use]
    pub fn with_multiuser(
        mut self,
        pepper: TokenPepper,
        reader: ReaderPool,
        writer: WriterHandle,
    ) -> Self {
        self.multiuser = Some(MultiUserResolver {
            pepper,
            reader,
            writer,
        });
        self
    }

    /// True when a token is configured (i.e. the middleware is doing
    /// anything). Useful for the startup log line so the operator
    /// sees whether their server is open or closed.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.expected.is_some()
    }
}

/// axum middleware closure. Wire with
/// `axum::middleware::from_fn_with_state(state, require_bearer)`.
///
/// Token sources, in priority order:
/// 1. `Authorization: Bearer <token>` header. Works for any method.
///    This is what MCP + hook clients send.
/// 2. **GET only:** `Authorization: Basic <base64(user:token)>`.
///    Username is ignored; the password portion is the token.
///    Browsers send this automatically after the native credential
///    prompt fires on a 401 + `WWW-Authenticate: Basic`. On success
///    we also set the `ai_memory_auth` cookie so subsequent visits
///    (including from a fresh browser session) skip the prompt.
/// 3. **GET only:** `ai_memory_auth` cookie set by the Basic handshake.
///
/// POST / PUT / DELETE / etc. require the Bearer header. Cookie and
/// Basic auth are GET-only, which confines cookie-CSRF to read-only
/// pages — `/mcp` + `/hook` are POST-only and stay header-gated.
///
/// On 401 for GET requests the response includes both `Basic` and
/// `Bearer` challenges in `WWW-Authenticate`. Browsers honour the
/// `Basic` challenge (native dialog); MCP clients honour the `Bearer`
/// challenge.
/// Every header the trusted-proxy assertion reads.
/// A repeated occurrence of any of them makes the assertion ambiguous — see
/// [`trusted_proxy_actor`].
const PROXY_ASSERTION_HEADERS: [&str; 6] = [
    "x-memory-actor-user",
    "x-memory-actor-issuer",
    "x-memory-actor-sub",
    "x-memory-actor-agent",
    "x-memory-actor-client",
    "x-memory-actor-session-id",
];

/// Why a trusted-proxy identity assertion was rejected.
#[derive(Debug)]
enum ProxyAssertionError {
    /// The proxy's headers arrived more than once, so WHO the caller is cannot
    /// be decided. The request must fail rather than pick a value.
    Ambiguous,
    /// A proxy credential must always name a human.
    MissingIdentity,
    /// OIDC issuer and subject are accepted only together.
    PartialOidcIdentity,
}

/// Parse the identity asserted by an already-authenticated proxy.
///
/// # The proxy MUST strip client-supplied `X-Memory-Actor-*` headers
///
/// This whole overlay assumes the `X-Memory-Actor-*` values on the wire are the
/// proxy's, not the caller's. An ingress configured to *append* its headers
/// rather than replace them leaves the client's value in place beside the
/// proxy's, and there is no way to tell which is which — so a duplicate is
/// treated as a spoofing attempt and the request is refused
/// ([`ProxyAssertionError::Ambiguous`]) instead of one of the two being picked.
fn trusted_proxy_actor(headers: &HeaderMap) -> Result<ActorContext, ProxyAssertionError> {
    if PROXY_ASSERTION_HEADERS.iter().any(|name| {
        headers.get_all(*name).iter().count() > 1
            || headers
                .get(*name)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains(','))
    }) {
        return Err(ProxyAssertionError::Ambiguous);
    }
    let asserted = crate::actor::actor_from_headers(headers);
    if asserted.issuer.is_some() != asserted.sub.is_some() {
        return Err(ProxyAssertionError::PartialOidcIdentity);
    }
    if asserted.identity_key().is_none() {
        return Err(ProxyAssertionError::MissingIdentity);
    }
    Ok(asserted)
}

/// axum middleware closure. Wire with
/// `axum::middleware::from_fn_with_state(state, require_bearer)`.
///
/// Token sources, in priority order:
/// 1. `Authorization: Bearer <token>` header. Works for any method.
///    This is what MCP + hook clients send.
/// 2. **GET only:** `Authorization: Basic <base64(user:token)>`.
///    Username is ignored; the password portion is the token.
///    Browsers send this automatically after the native credential
///    prompt fires on a 401 + `WWW-Authenticate: Basic`. On success
///    we also set the `ai_memory_auth` cookie so subsequent visits
///    (including from a fresh browser session) skip the prompt.
/// 3. **GET only:** `ai_memory_auth` cookie set by the Basic handshake.
///
/// POST / PUT / DELETE / etc. require the Bearer header. Cookie and
/// Basic auth are GET-only, which confines cookie-CSRF to read-only
/// pages — `/mcp` + `/hook` are POST-only and stay header-gated.
///
/// On 401 for GET requests the response includes both `Basic` and
/// `Bearer` challenges in `WWW-Authenticate`. Browsers honour the
/// `Basic` challenge (native dialog); MCP clients honour the `Bearer`
/// challenge.
pub async fn require_bearer(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Rung 0: auth disabled. Inject anonymous actor + anonymous
    // tier (so downstream handlers that read Extension<ActorContext>
    // and Extension<AuthLevel> always have one), then pass through.
    let Some(expected) = state.expected.as_deref() else {
        req.extensions_mut().insert(ActorContext::anonymous());
        req.extensions_mut().insert(AuthLevel::Anonymous);
        return next.run(req).await;
    };

    let is_get = req.method() == Method::GET;
    let from_bearer = extract_bearer_header(&req);
    let from_basic = if is_get {
        extract_basic_header(&req)
    } else {
        None
    };
    let from_cookie = if is_get { extract_cookie(&req) } else { None };

    let provided = from_bearer
        .as_deref()
        .or(from_basic.as_deref())
        .or(from_cookie.as_deref())
        .unwrap_or("");

    // Rung 1: root credential. Actor assertion headers are intentionally
    // ignored here; only the distinct proxy credential may assert them.
    if bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
        req.extensions_mut().insert(state.root_actor.clone());
        req.extensions_mut().insert(AuthLevel::Root);

        // First successful Basic-auth hit (no cookie yet) → also stamp
        // the cookie so the user doesn't get the dialog again next
        // browser session. Subsequent navigations ride the cookie alone.
        if from_basic.is_some() && from_cookie.is_none() {
            let mut resp = next.run(req).await;
            if let Ok(cookie) = build_session_cookie(provided, state.secure_cookie).parse() {
                resp.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            return resp;
        }
        return next.run(req).await;
    }

    // Rung 1b: only an explicit Bearer token can enter the trusted-proxy
    // branch. Basic auth and cookies are browser conveniences for the root
    // credential, not a way to impersonate the proxy.
    if let (Some(proxy_expected), Some(proxy_provided)) =
        (state.actor_proxy_bearer.as_deref(), from_bearer.as_deref())
        && bool::from(proxy_provided.as_bytes().ct_eq(proxy_expected.as_bytes()))
    {
        let actor = match trusted_proxy_actor(req.headers()) {
            Ok(actor) => actor,
            Err(error) => {
                debug!(
                    ?error,
                    "auth rejected: invalid trusted-proxy identity assertion"
                );
                return invalid_proxy_identity(error);
            }
        };
        let level = if state.asserts_root_identity(&actor) {
            AuthLevel::Root
        } else {
            AuthLevel::User
        };
        debug!(actor.user = ?actor.user, actor.issuer = ?actor.issuer, actor.sub = ?actor.sub, ?level, "identity asserted by trusted proxy");
        req.extensions_mut().insert(actor);
        req.extensions_mut().insert(level);
        return next.run(req).await;
    }

    // Rung 2: bearer matches neither root nor proxy. If multi-user is enabled,
    // hash + look up the token against the `users` table.
    if let Some(mu) = state.multiuser.as_ref()
        && !provided.is_empty()
    {
        let hash = hash_token(provided, &mu.pepper);
        match mu.reader.find_active_user_by_token_hash(hash).await {
            Ok(Some(user)) => {
                // NEVER log the token itself; the username + agent is
                // safe and useful for "who hit /api/v1 last".
                debug!(actor.user = %user.username, "authenticated as DB user");
                let actor = ActorContext {
                    user: Some(user.username.clone()),
                    name: user.name.clone(),
                    email: user.email.clone(),
                    ..ActorContext::default()
                };
                req.extensions_mut().insert(actor);
                req.extensions_mut().insert(user.id);
                req.extensions_mut().insert(AuthLevel::User);

                // Fire-and-forget last_seen_at bump. Errors are logged
                // but never block the response — middleware MUST stay
                // off the response's critical path. Same browser-cookie
                // dance as rung 1 above.
                let writer = mu.writer.clone();
                let user_id = user.id;
                tokio::spawn(async move {
                    if let Err(e) = writer.touch_user_last_seen(user_id).await {
                        tracing::warn!(error = %e, user_id = %user_id, "touch_user_last_seen failed");
                    }
                });

                if from_basic.is_some() && from_cookie.is_none() {
                    let mut resp = next.run(req).await;
                    if let Ok(cookie) = build_session_cookie(provided, state.secure_cookie).parse()
                    {
                        resp.headers_mut().insert(header::SET_COOKIE, cookie);
                    }
                    return resp;
                }
                return next.run(req).await;
            }
            Ok(None) => {
                // Bearer present + multi-user enabled + no match → fall
                // through to the 401 below. Critical for closing the
                // bypass: an unknown bearer MUST NOT pass even when
                // multi-user lookup is configured.
            }
            Err(e) => {
                tracing::error!(error = %e, "auth: users table lookup failed");
                return unauthorized(is_get);
            }
        }
    }

    debug!("auth rejected: invalid or missing token");
    unauthorized(is_get)
}

fn extract_bearer_header(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    // Accept both "Bearer xxx" and "bearer xxx" (case-insensitive
    // scheme per RFC 7235 §2.1).
    let (scheme, value) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") {
        Some(value.trim_start().to_string())
    } else {
        None
    }
}

fn extract_basic_header(req: &Request<axum::body::Body>) -> Option<String> {
    use base64::Engine;
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.trim_start())
        .ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    // Standard form: `user:password`. We ignore the username (the
    // browser dialog always asks for one but we don't have multi-user
    // accounts — only the password = bearer token matters).
    let (_user, pass) = s.split_once(':')?;
    Some(pass.to_string())
}

fn extract_cookie(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in h.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(&format!("{AUTH_COOKIE}=")) {
            return Some(val.to_string());
        }
    }
    None
}

fn build_session_cookie(token: &str, secure_cookie: bool) -> String {
    // 30-day Max-Age — long enough that re-entering the credential
    // every month is rare. HttpOnly hides it from inline JS and Strict keeps
    // cross-site requests from riding it. `Secure` remains opt-in so direct
    // loopback/plain-HTTP browser use works; never infer it from proxy headers.
    let secure = if secure_cookie { "; Secure" } else { "" };
    format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}")
}

/// The proxy's identity headers contradict themselves. Not a 401 — the
/// credential was fine; the request itself is malformed, and retrying it with
/// the same headers will keep failing until the ingress is fixed to REPLACE
/// `X-Memory-Actor-*` rather than append to whatever the client sent.
fn invalid_proxy_identity(error: ProxyAssertionError) -> Response {
    let message = match error {
        ProxyAssertionError::Ambiguous => {
            "ambiguous X-Memory-Actor-* header: the proxy must replace client-supplied actor headers, not append to them\n"
        }
        ProxyAssertionError::MissingIdentity => {
            "trusted proxy must assert X-Memory-Actor-User or both X-Memory-Actor-Issuer and X-Memory-Actor-Sub\n"
        }
        ProxyAssertionError::PartialOidcIdentity => {
            "trusted proxy must assert X-Memory-Actor-Issuer and X-Memory-Actor-Sub together\n"
        }
    };
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn unauthorized(include_basic_challenge: bool) -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "auth required\n").into_response();
    // Order of challenges matters: browsers parse the first challenge
    // they understand and show the dialog for it. Putting `Basic`
    // first ensures GET-from-browser triggers the native prompt; MCP
    // clients (which speak only Bearer) ignore the Basic and read
    // their challenge from the second value.
    //
    // Non-GET 401s skip the Basic challenge — sending it on a POST
    // would invite the browser to dialog-prompt for an endpoint
    // it can't authenticate this way anyway.
    let value = if include_basic_challenge {
        format!(
            "Basic realm=\"{AUTH_REALM}\", \
             Bearer realm=\"{AUTH_REALM}\", error=\"invalid_token\""
        )
    } else {
        format!("Bearer realm=\"{AUTH_REALM}\", error=\"invalid_token\"")
    };
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        value.parse().expect("static header value is valid"),
    );
    resp
}

/// Generate a fresh random bearer token, hex-encoded.
///
/// `bytes` is the entropy budget; 32 bytes (256 bits) is plenty for
/// any conceivable threat model.
///
/// # Errors
/// Propagates failures from the OS RNG.
pub fn generate_token_hex(bytes: usize) -> Result<String, getrandom::Error> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)?;
    Ok(hex_encode(&buf))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    fn router_with_auth(token: Option<&str>) -> Router {
        router_with_auth_secure_cookie(token, false)
    }

    fn router_with_auth_secure_cookie(token: Option<&str>, secure_cookie: bool) -> Router {
        let state =
            Arc::new(AuthState::new(token.map(str::to_string)).with_secure_cookie(secure_cookie));
        Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
    }

    #[tokio::test]
    async fn no_token_configured_passes_anything_through() {
        let r = router_with_auth(None);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_header_returns_401_with_www_authenticate() {
        let r = router_with_auth(Some("secret"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        // GET 401 advertises BOTH challenges so browsers (Basic) and
        // MCP clients (Bearer) each see what they understand.
        assert!(www.contains("Bearer"));
        assert!(www.contains("Basic"));
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let r = router_with_auth(Some("the-right-one"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-wrong-one")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn right_token_returns_200() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn lowercase_scheme_is_accepted() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_scheme_is_rejected() {
        // `Digest`, `OAuth`, etc. are not handled.
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Digest username=foo,response=bar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_with_right_token_passes_get() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_header_takes_precedence_over_cookie() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-token")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_with_wrong_token_fails() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_ignored_on_post() {
        // POST routes must use Bearer header; cookie auth is GET-only
        // to keep the CSRF surface confined to read paths.
        let state = Arc::new(AuthState::new(Some("right-token".to_string())));
        let r = Router::new()
            .route("/probe", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let resp = r
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Helper: build a Basic-auth header value (any username, token as password).
    fn basic_auth(token: &str) -> String {
        use base64::Engine;
        let creds = format!("any:{token}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(creds)
        )
    }

    #[tokio::test]
    async fn basic_auth_with_right_token_passes_get_and_sets_non_secure_cookie() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", basic_auth("right-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // First successful Basic hit also stamps the cookie so the
        // browser doesn't dialog-prompt every session.
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("set-cookie on first Basic-auth success")
            .to_str()
            .unwrap();
        assert_eq!(
            cookie,
            "ai_memory_auth=right-token; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"
        );
    }

    #[tokio::test]
    async fn basic_auth_sets_secure_cookie_only_when_explicitly_enabled() {
        let r = router_with_auth_secure_cookie(Some("right-token"), true);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", basic_auth("right-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::SET_COOKIE)
                .expect("set-cookie on first Basic-auth success")
                .to_str()
                .unwrap(),
            "ai_memory_auth=right-token; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000; Secure"
        );
    }

    #[tokio::test]
    async fn basic_auth_with_wrong_password_returns_401() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", basic_auth("wrong-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn basic_auth_ignored_on_post() {
        // POST routes must use Bearer header; Basic auth is GET-only.
        let state = Arc::new(AuthState::new(Some("right-token".to_string())));
        let r = Router::new()
            .route("/probe", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let resp = r
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("Authorization", basic_auth("right-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // POST 401 must NOT advertise Basic — browsers would dialog
        // for a route they can't authenticate this way.
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        assert!(www.contains("Bearer"));
        assert!(!www.contains("Basic"));
    }

    #[tokio::test]
    async fn cookie_request_does_not_re_set_cookie() {
        // Already-authed-by-cookie requests don't need a Set-Cookie
        // refresh; that's a waste of bandwidth on every navigation.
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn generated_token_is_hex_and_correct_length() {
        let t = generate_token_hex(32).unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Distinct calls produce distinct tokens (modulo OS RNG bugs).
        let t2 = generate_token_hex(32).unwrap();
        assert_ne!(t, t2);
    }

    // ── Extension<ActorContext> injection (P1.3 multi-rung resolution) ──

    use ai_memory_core::NewUser;
    use ai_memory_store::Store;
    use axum::Extension;
    use tempfile::TempDir;

    /// Route handler that echoes the injected `ActorContext` as JSON
    /// in the response body, so tests can verify which rung fired.
    async fn echo_actor(Extension(actor): Extension<ActorContext>) -> axum::Json<ActorContext> {
        axum::Json(actor)
    }

    async fn echo_auth_level(Extension(level): Extension<AuthLevel>) -> &'static str {
        match level {
            AuthLevel::Anonymous => "anonymous",
            AuthLevel::Root => "root",
            AuthLevel::User => "user",
        }
    }

    fn router_with_state(state: AuthState) -> Router {
        Router::new()
            .route("/probe", get(echo_actor))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(state),
                require_bearer,
            ))
    }

    async fn body_as_actor(resp: Response) -> ActorContext {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn rung0_anonymous_attaches_default_actor() {
        // No token configured → middleware is a no-op gate but still
        // injects an anonymous Extension<ActorContext> so handlers
        // always have one to read.
        let r = router_with_state(AuthState::new(None));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert!(!actor.has_any(), "rung 0 must inject anonymous actor");
    }

    /// A proxy-asserted human must NOT inherit root capability: the credential
    /// belongs to the proxy, the identity belongs to an ordinary person, and
    /// admin-gated operations (sweep, purge, delete) key off the level.
    #[tokio::test]
    async fn proxy_asserted_identity_is_downgraded_to_user_level() {
        let root = ActorContext {
            user: Some("root-operator".into()),
            ..ActorContext::default()
        };
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(root)
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let router = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));

        let level_for = |user: &'static str| {
            let router = router.clone();
            async move {
                let resp = router
                    .oneshot(
                        Request::builder()
                            .uri("/level")
                            .header("Authorization", "Bearer proxy-bearer-token")
                            .header("X-Memory-Actor-User", user)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };

        for user in ["alice", "root-operator"] {
            assert_eq!(
                level_for(user).await,
                "user",
                "a proxy username alone must never grant root capability"
            );
        }
    }

    /// A proxy credential without a human assertion must fail closed.
    #[tokio::test]
    async fn proxy_bearer_without_an_asserted_identity_is_refused() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Agent", "healthcheck")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// An ingress that terminates OIDC can name a human with the stable pair
    /// even when it does not forward `preferred_username`.
    #[tokio::test]
    async fn proxy_asserting_oidc_identity_is_downgraded_to_user_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .header("X-Memory-Actor-Sub", "oidc-subject-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes.to_vec()).unwrap(),
            "user",
            "an OIDC assertion names somebody who is not the root operator"
        );
    }

    #[tokio::test]
    async fn oidc_root_requires_the_exact_issuer_and_subject_pair() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    issuer: Some("https://idp.example".into()),
                    sub: Some("root-subject".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let router = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let level_for = |issuer: &'static str, subject: &'static str| {
            let router = router.clone();
            async move {
                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/level")
                            .header("Authorization", "Bearer proxy-bearer-token")
                            .header("X-Memory-Actor-User", "root-operator")
                            .header("X-Memory-Actor-Issuer", issuer)
                            .header("X-Memory-Actor-Sub", subject)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };

        assert_eq!(
            level_for("https://idp.example", "root-subject").await,
            "root"
        );
        assert_eq!(
            level_for("https://other-idp.example", "root-subject").await,
            "user"
        );
        assert_eq!(
            level_for("https://idp.example", "someone-else").await,
            "user"
        );
    }

    #[tokio::test]
    async fn partial_oidc_identity_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_trusted_proxy_bearer("proxy-bearer-token");
        for (name, value) in [
            ("X-Memory-Actor-Issuer", "https://idp.example"),
            ("X-Memory-Actor-Sub", "subject-only"),
        ] {
            let response = router_with_state(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/probe")
                        .header("Authorization", "Bearer proxy-bearer-token")
                        .header(name, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "header {name}");
        }
    }

    /// An ingress that APPENDS `X-Memory-Actor-User` instead of replacing it
    /// leaves the caller's own value first in the list, and `HeaderMap::get`
    /// returns the first — so Bob sending `X-Memory-Actor-User: alice` would be
    /// Alice everywhere. Neither value may be adopted: refuse the request.
    #[tokio::test]
    async fn duplicated_actor_user_header_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    // Bob's own header, then the proxy's appended one.
                    .header("X-Memory-Actor-User", "alice")
                    .header("X-Memory-Actor-User", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "an ambiguous asserted identity must fail closed, not resolve to one of the two"
        );
    }

    /// Some proxies fold repeated headers into a comma-separated value. Reject
    /// that representation too rather than choosing one asserted identity.
    #[tokio::test]
    async fn folded_actor_user_header_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-User", "alice,bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Without a proxy assertion the root bearer keeps root, unchanged.
    #[tokio::test]
    async fn plain_root_bearer_keeps_root_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer the-root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "root");
    }

    /// A proxy's OIDC identity replaces the root actor template completely.
    #[tokio::test]
    async fn proxy_oidc_identity_drops_root_template_and_carries_the_pair() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                email: Some("root@example.com".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .header("X-Memory-Actor-Sub", "oidc-subject-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user, None, "the root username must not survive");
        assert_eq!(actor.issuer.as_deref(), Some("https://idp.example"));
        assert_eq!(actor.sub.as_deref(), Some("oidc-subject-123"));
        assert_eq!(actor.email, None);
    }

    /// Security boundary: the `X-Memory-Actor-*` headers are pure client input.
    /// With no proxy secret configured they must be ignored completely, or
    /// anyone who can reach the port authenticates as root and then names
    /// themselves whoever they like.
    #[tokio::test]
    async fn actor_headers_are_ignored_without_a_configured_proxy_bearer() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into())).with_root_actor(root);
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(
            actor.user.as_deref(),
            Some("boss"),
            "unproven actor headers must not override the root identity"
        );
    }

    /// Root credentials never consume proxy assertion headers, even when the
    /// proxy rung is configured.
    #[tokio::test]
    async fn actor_headers_are_ignored_on_the_root_rung() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
    }

    /// An unknown proxy bearer cannot fall through into the root rung.
    #[tokio::test]
    async fn wrong_proxy_bearer_is_unauthorized() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-proxy-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// The point of the feature: two people behind one proxy credential become
    /// different actors.
    #[tokio::test]
    async fn trusted_proxy_identity_builds_an_independent_actor() {
        let root = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            name: Some("Boss".into()),
            ..ActorContext::default()
        };
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(root)
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let router = Router::new()
            .route("/probe", get(echo_actor))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));

        for user in ["alice", "bob"] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/probe")
                        .header("Authorization", "Bearer proxy-bearer-token")
                        .header("X-Memory-Actor-User", user)
                        .header("X-Memory-Actor-Session-Id", format!("sess-{user}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let actor = body_as_actor(resp).await;
            assert_eq!(actor.user.as_deref(), Some(user));
            assert_eq!(actor.session_id.as_deref(), Some(&*format!("sess-{user}")));
            // The root template's contact details describe the root operator,
            // not the human just named, so they must not be carried over.
            assert_eq!(actor.email, None);
            assert_eq!(actor.name, None);
        }
    }

    /// A blank proxy bearer must not enable the trusted rung.
    #[tokio::test]
    async fn blank_proxy_bearer_does_not_enable_the_proxy_rung() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("   ");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
    }

    #[tokio::test]
    async fn rung1_root_token_attributes_via_root_actor_template() {
        // Bearer matches config token → middleware stamps the
        // `[auth].root_*` template.
        let root = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            name: Some("Boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into())).with_root_actor(root);
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
        assert_eq!(actor.email.as_deref(), Some("boss@example.com"));
        assert_eq!(actor.name.as_deref(), Some("Boss"));
    }

    #[tokio::test]
    async fn rung1_without_root_template_still_authenticates_anonymously() {
        // Backward-compat with existing single-user setups that have
        // bearer_token but no root_username/email/name configured.
        // Bearer matches → 200, but the actor stays anonymous.
        let state = AuthState::new(Some("plain-token".into()));
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer plain-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert!(
            !actor.has_any(),
            "rung-1 sans template must still attribute anonymously"
        );
    }

    /// Fresh store + writer/reader + pre-loaded users row, ready to
    /// plug into [`AuthState::with_multiuser`]. Returns the raw
    /// plaintext token issued to the new user so tests can present it
    /// in the request and assert it routes correctly.
    async fn setup_multiuser(username: &str) -> (TempDir, AuthState, String) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let pepper = TokenPepper::new("test-pepper");
        let token = ai_memory_store::generate_token().unwrap();
        let token_hash = ai_memory_store::hash_token(&token, &pepper);
        let mut new_user = NewUser {
            username: username.into(),
            name: Some(format!("{username} display")),
            email: Some(format!("{username}@example.com")),
        };
        new_user.validate().unwrap();
        store
            .writer
            .create_user(new_user, token_hash)
            .await
            .unwrap();

        let state = AuthState::new(Some("root-token-distinct-from-user-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root".into()),
                ..ActorContext::default()
            })
            .with_multiuser(pepper, store.reader.clone(), store.writer.clone());
        (tmp, state, token)
    }

    #[tokio::test]
    async fn rung2_db_user_token_attributes_to_row() {
        // Bearer doesn't match root, multi-user is enabled, and the
        // token hashes to a `users` row → middleware injects that
        // user's identity (NOT the root template).
        let (_tmp, state, token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("alice"));
        assert_eq!(actor.email.as_deref(), Some("alice@example.com"));
        assert_eq!(actor.name.as_deref(), Some("alice display"));
        // NOT the root template — root_actor.user is "root".
        assert_ne!(actor.user.as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn rung3_unknown_bearer_with_multiuser_returns_401_not_anonymous() {
        // The bypass guard: bearer present but matches NEITHER root
        // NOR any users row → MUST 401. Critical so a fat-fingered
        // operator (or compromised client) can't squeak through.
        let (_tmp, state, _token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer this-token-is-not-in-the-DB")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rung2_expired_user_token_is_rejected() {
        // Expiring a user's token must immediately stop authenticating
        // (no 30s cache window or similar). Critical for `ai-memory
        // user expire` to be useful as an offboarding tool.
        let (_tmp, state, token) = setup_multiuser("alice").await;

        // Look up user id via the writer-side roundtrip (no public
        // find-by-username on WriterHandle, so use the reader pool).
        let user = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .find_user_by_username("alice".into())
            .await
            .unwrap()
            .unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer.expire_user_token(user.id).await.unwrap();

        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expired user token must not authenticate"
        );
    }

    #[tokio::test]
    async fn rung2_revived_user_token_authenticates_again() {
        let (_tmp, state, token) = setup_multiuser("alice").await;
        let user = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .find_user_by_username("alice".into())
            .await
            .unwrap()
            .unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer.expire_user_token(user.id).await.unwrap();
        writer.revive_user_token(user.id).await.unwrap();

        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn root_token_wins_even_when_multiuser_is_enabled() {
        // Mixed setup: root token AND added users. Bearer = root token
        // → root_actor template (NOT a users-table lookup that wouldn't
        // find anything anyway).
        let (_tmp, state, _user_token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(
                        "Authorization",
                        "Bearer root-token-distinct-from-user-token",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn resolver_authenticates_user_added_after_router_construction() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let pepper = TokenPepper::new("test-pepper");
        let state = AuthState::new(Some("root-token".into())).with_multiuser(
            pepper.clone(),
            store.reader.clone(),
            store.writer.clone(),
        );
        let router = Router::new().route("/level", get(echo_auth_level)).layer(
            axum::middleware::from_fn_with_state(Arc::new(state), require_bearer),
        );

        let token = ai_memory_store::generate_token().unwrap();
        let mut user = NewUser {
            username: "alice".into(),
            name: None,
            email: None,
        };
        user.validate().unwrap();
        store
            .writer
            .create_user(user, ai_memory_store::hash_token(&token, &pepper))
            .await
            .unwrap();

        for (token, expected_status, expected_level) in [
            (token.as_str(), StatusCode::OK, Some("user")),
            ("root-token", StatusCode::OK, Some("root")),
            ("unknown-token", StatusCode::UNAUTHORIZED, None),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/level")
                        .header("Authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status, "token {token}");
            if let Some(expected_level) = expected_level {
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(body.as_ref(), expected_level.as_bytes());
            }
        }
    }

    #[test]
    fn blank_root_token_is_not_enabled() {
        assert!(!AuthState::new(Some("  ".into())).enabled());
    }

    #[tokio::test]
    async fn rung1_setup_with_unknown_bearer_returns_401() {
        // Existing single-user setup (rung 1 only, no multi-user). An
        // unknown bearer must still 401 — same as pre-P1.3 behaviour.
        let state = AuthState::new(Some("right-token".into())).with_root_actor(ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        });
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

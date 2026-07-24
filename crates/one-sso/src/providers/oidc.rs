//! Standard OpenID Connect (OIDC) provider.
//!
//! One provider covers the three big Western enterprise IdPs — Okta, Azure AD
//! (Microsoft Entra), and Google Workspace — because they all speak OIDC. The
//! flow is the textbook authorization-code flow with OIDC Discovery:
//!
//! 1. `discover` → GET `{issuer}/.well-known/openid-configuration` to learn the
//!    authorization / token / userinfo endpoints (all three IdPs publish it).
//! 2. `build_authorize_url` → redirect the browser to the IdP.
//! 3. `exchange_code` → POST the code to the token endpoint, get an access
//!    token (+ id_token).
//! 4. `fetch_user_info` → GET the userinfo endpoint with the access token to
//!    read the identity claims.
//!
//! # Security note (v1 scope)
//! Identity is read from the **userinfo endpoint** using the access token,
//! which was itself obtained from the token endpoint over TLS authenticated
//! with the client secret — so the claims are trusted without separately
//! verifying the `id_token` JWT signature against the IdP's JWKS. Full
//! `id_token` signature verification (JWKS fetch + `jsonwebtoken`) is a
//! hardening follow-up, not required for a correct, secure v1.

use serde::Deserialize;

use crate::error::SsoError;
use crate::providers::ProviderUserInfo;

const OIDC_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
const DEFAULT_SCOPES: &str = "openid profile email";
const DEFAULT_EXTERNAL_ID_CLAIM: &str = "sub";
const DEFAULT_NAME_CLAIM: &str = "name";

/// Stored config for an OIDC provider. Keys are camelCase in the persisted
/// JSON (matching the other providers and the admin form); parsed manually in
/// `service::parse_oidc_config`, same as `parse_feishu_config`.
#[derive(Debug, Clone)]
pub struct OidcProviderConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    /// Space-separated scopes; must include `openid`.
    pub scopes: String,
    /// Claim holding the stable per-user id (default `sub`).
    pub external_id_claim: String,
    /// Claim holding the display name (default `name`).
    pub name_claim: String,
    /// Optional claim holding a company/tenant id (e.g. Google Workspace `hd`).
    /// When present it becomes `ProviderUserInfo.org_external_id`, so same-
    /// company logins auto-join the SSO enterprise tenant — mirrors the Feishu
    /// `tenant_key` semantics. `None` disables company binding for this IdP.
    pub company_claim: Option<String>,
    /// Test-only override for the discovery host (points at a wiremock server);
    /// never set in production, never surfaced in the admin form. Same pattern
    /// as `FeishuProviderConfig::base_url`.
    pub base_url: Option<String>,
}

impl OidcProviderConfig {
    /// Host the discovery document is fetched from. Production uses the
    /// `issuer`; tests override it. The token / userinfo / authorize endpoints
    /// themselves come from the discovery document (absolute URLs), so they
    /// follow the mock server automatically in tests.
    fn discovery_base(&self) -> &str {
        self.base_url.as_deref().unwrap_or(&self.issuer)
    }

    pub fn scopes_or_default(&self) -> &str {
        if self.scopes.trim().is_empty() {
            DEFAULT_SCOPES
        } else {
            self.scopes.as_str()
        }
    }

    pub fn external_id_claim_or_default(&self) -> &str {
        if self.external_id_claim.trim().is_empty() {
            DEFAULT_EXTERNAL_ID_CLAIM
        } else {
            self.external_id_claim.as_str()
        }
    }

    pub fn name_claim_or_default(&self) -> &str {
        if self.name_claim.trim().is_empty() {
            DEFAULT_NAME_CLAIM
        } else {
            self.name_claim.as_str()
        }
    }
}

/// The subset of the OIDC discovery document we use.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcDiscovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Optional per spec, but present on Okta/Azure/Google. Required for our
    /// userinfo-based identity read.
    pub userinfo_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct OidcTokenResponse {
    access_token: Option<String>,
}

pub struct OidcProvider;

impl OidcProvider {
    fn client() -> Result<reqwest::Client, SsoError> {
        reqwest::Client::builder()
            .timeout(OIDC_HTTP_TIMEOUT)
            .build()
            .map_err(|e| SsoError::Internal(format!("http client: {e}")))
    }

    /// Fetch + parse the OIDC discovery document.
    pub async fn discover(config: &OidcProviderConfig) -> Result<OidcDiscovery, SsoError> {
        let base = config.discovery_base().trim_end_matches('/');
        let url = format!("{base}{DISCOVERY_PATH}");
        let resp = Self::client()?.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("OIDC discovery failed: HTTP {status}")));
        }
        let discovery: OidcDiscovery = resp
            .json()
            .await
            .map_err(|e| SsoError::Internal(format!("OIDC discovery parse: {e}")))?;
        if discovery.authorization_endpoint.trim().is_empty() || discovery.token_endpoint.trim().is_empty() {
            return Err(SsoError::Internal(
                "OIDC discovery missing authorization_endpoint/token_endpoint".into(),
            ));
        }
        Ok(discovery)
    }

    pub fn build_authorize_url(discovery: &OidcDiscovery, config: &OidcProviderConfig, state: &str) -> String {
        format!(
            "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}",
            discovery.authorization_endpoint,
            urlencode(&config.client_id),
            urlencode(&config.redirect_uri),
            urlencode(config.scopes_or_default()),
            urlencode(state),
        )
    }

    /// Exchange the authorization code for an access token at the token
    /// endpoint (standard `application/x-www-form-urlencoded` body).
    pub async fn exchange_code(
        discovery: &OidcDiscovery,
        config: &OidcProviderConfig,
        code: &str,
    ) -> Result<String, SsoError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
        ];
        if !config.redirect_uri.is_empty() {
            form.push(("redirect_uri", &config.redirect_uri));
        }

        let resp = Self::client()?
            .post(&discovery.token_endpoint)
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            let err = json
                .get("error_description")
                .or_else(|| json.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("token exchange failed");
            return Err(SsoError::Internal(format!("OIDC token exchange: HTTP {status}: {err}")));
        }
        let token: OidcTokenResponse = serde_json::from_value(json).unwrap_or_default();
        token
            .access_token
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| SsoError::Internal("OIDC token exchange: missing access_token".into()))
    }

    /// Read identity claims from the userinfo endpoint using the access token.
    pub async fn fetch_user_info(discovery: &OidcDiscovery, access_token: &str) -> Result<serde_json::Value, SsoError> {
        let endpoint = discovery
            .userinfo_endpoint
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .ok_or_else(|| SsoError::Internal("OIDC provider has no userinfo_endpoint".into()))?;
        let resp = Self::client()?.get(endpoint).bearer_auth(access_token).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SsoError::Internal(format!("OIDC userinfo failed: HTTP {status}")));
        }
        let claims: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SsoError::Internal(format!("OIDC userinfo parse: {e}")))?;
        Ok(claims)
    }

    /// Resolve the external id from the configured claim (default `sub`).
    pub fn resolve_external_id(claims: &serde_json::Value, claim: &str) -> Option<String> {
        claims
            .get(claim)
            .and_then(claim_as_string)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Map OIDC claims onto the normalized `ProviderUserInfo`.
    pub fn to_provider_user_info(
        claims: &serde_json::Value,
        external_id: &str,
        config: &OidcProviderConfig,
    ) -> ProviderUserInfo {
        let preferred = claims
            .get(config.name_claim_or_default())
            .and_then(claim_as_string)
            .or_else(|| claims.get("preferred_username").and_then(claim_as_string))
            .or_else(|| claims.get("name").and_then(claim_as_string))
            .or_else(|| claims.get("email").and_then(claim_as_string))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("oidc_{}", &external_id[..external_id.len().min(16)]));

        // Company/tenant id from the optional configured claim only — OIDC has
        // no standard company claim, so this stays None unless the admin maps
        // one (e.g. Google `hd`). Empty/blank never binds an enterprise to "".
        let org_external_id = config
            .company_claim
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .and_then(|c| claims.get(c))
            .and_then(claim_as_string)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        ProviderUserInfo {
            external_id: external_id.to_owned(),
            preferred_username: preferred,
            // Standard OIDC claims carry neither department nor job title —
            // those arrive via SCIM / directory sync in a later step.
            org_unit_path: None,
            job_title: None,
            org_external_id,
        }
    }

    /// Validate the config by resolving the discovery document — used by the
    /// admin "Test connection" button.
    pub async fn test_credentials(config: &OidcProviderConfig) -> Result<(), SsoError> {
        if config.issuer.trim().is_empty() || config.client_id.trim().is_empty() {
            return Err(SsoError::BadRequest(
                "Issuer and Client ID are required for connection test".into(),
            ));
        }
        let discovery = Self::discover(config).await?;
        if discovery
            .userinfo_endpoint
            .as_deref()
            .map(|e| e.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(SsoError::Internal(
                "OIDC issuer's discovery document has no userinfo_endpoint".into(),
            ));
        }
        Ok(())
    }
}

/// Accept a claim that is a JSON string, or coerce a number/bool to its string
/// form (some IdPs emit numeric `sub`).
fn claim_as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_with_base(base: &str) -> OidcProviderConfig {
        OidcProviderConfig {
            issuer: "https://issuer.example.com".into(),
            client_id: "client-abc".into(),
            client_secret: "secret-xyz".into(),
            redirect_uri: "https://app.example.com/api/one/sso/oidc/callback".into(),
            scopes: String::new(),
            external_id_claim: String::new(),
            name_claim: String::new(),
            company_claim: Some("hd".into()),
            base_url: Some(base.to_owned()),
        }
    }

    async fn mount_discovery(server: &MockServer) {
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": "https://issuer.example.com",
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "userinfo_endpoint": format!("{base}/userinfo"),
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn discover_parses_endpoints() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        let cfg = config_with_base(&server.uri());
        let d = OidcProvider::discover(&cfg).await.unwrap();
        assert_eq!(d.authorization_endpoint, format!("{}/authorize", server.uri()));
        assert_eq!(d.token_endpoint, format!("{}/token", server.uri()));
        assert_eq!(
            d.userinfo_endpoint.as_deref(),
            Some(format!("{}/userinfo", server.uri()).as_str())
        );
    }

    #[tokio::test]
    async fn build_authorize_url_contains_required_params() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        let cfg = config_with_base(&server.uri());
        let d = OidcProvider::discover(&cfg).await.unwrap();
        let url = OidcProvider::build_authorize_url(&d, &cfg, "state123");
        assert!(url.starts_with(&format!("{}/authorize?", server.uri())));
        assert!(url.contains("client_id=client-abc"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=state123"));
        // default scopes url-encoded ("openid profile email")
        assert!(url.contains("scope=openid%20profile%20email"));
    }

    #[tokio::test]
    async fn exchange_code_returns_access_token() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-123", "id_token": "jwt.here", "token_type": "Bearer"
            })))
            .mount(&server)
            .await;
        let cfg = config_with_base(&server.uri());
        let d = OidcProvider::discover(&cfg).await.unwrap();
        let token = OidcProvider::exchange_code(&d, &cfg, "code-abc").await.unwrap();
        assert_eq!(token, "at-123");
    }

    #[tokio::test]
    async fn exchange_code_surfaces_provider_error() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant", "error_description": "code expired"
            })))
            .mount(&server)
            .await;
        let cfg = config_with_base(&server.uri());
        let d = OidcProvider::discover(&cfg).await.unwrap();
        let err = OidcProvider::exchange_code(&d, &cfg, "bad").await.unwrap_err();
        assert!(format!("{err}").contains("code expired"));
    }

    #[tokio::test]
    async fn fetch_user_info_maps_claims() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": "okta-user-1", "name": "Jane Doe", "email": "jane@acme.com", "hd": "acme.com"
            })))
            .mount(&server)
            .await;
        let cfg = config_with_base(&server.uri());
        let d = OidcProvider::discover(&cfg).await.unwrap();
        let claims = OidcProvider::fetch_user_info(&d, "at-123").await.unwrap();

        let external_id = OidcProvider::resolve_external_id(&claims, cfg.external_id_claim_or_default()).unwrap();
        assert_eq!(external_id, "okta-user-1");
        let profile = OidcProvider::to_provider_user_info(&claims, &external_id, &cfg);
        assert_eq!(profile.preferred_username, "Jane Doe");
        assert_eq!(profile.external_id, "okta-user-1");
        // company_claim "hd" → org_external_id (auto-join key).
        assert_eq!(profile.org_external_id.as_deref(), Some("acme.com"));
        assert_eq!(profile.org_unit_path, None);
        assert_eq!(profile.job_title, None);
    }

    #[test]
    fn to_provider_user_info_falls_back_to_email_then_prefix() {
        let cfg = OidcProviderConfig {
            issuer: "https://i".into(),
            client_id: "c".into(),
            client_secret: "s".into(),
            redirect_uri: String::new(),
            scopes: String::new(),
            external_id_claim: String::new(),
            name_claim: String::new(),
            company_claim: None,
            base_url: None,
        };
        // No name → email.
        let claims = serde_json::json!({ "sub": "u1", "email": "bob@x.com" });
        let p = OidcProvider::to_provider_user_info(&claims, "u1", &cfg);
        assert_eq!(p.preferred_username, "bob@x.com");
        // No name/email → oidc_ prefix.
        let claims = serde_json::json!({ "sub": "user_1234567890" });
        let p = OidcProvider::to_provider_user_info(&claims, "user_1234567890", &cfg);
        assert!(p.preferred_username.starts_with("oidc_"));
        // company_claim None → org_external_id None even if `hd` present.
        let claims = serde_json::json!({ "sub": "u1", "hd": "acme.com" });
        let p = OidcProvider::to_provider_user_info(&claims, "u1", &cfg);
        assert_eq!(p.org_external_id, None);
    }

    #[tokio::test]
    async fn test_credentials_ok_and_rejects_bad_issuer() {
        let server = MockServer::start().await;
        mount_discovery(&server).await;
        let cfg = config_with_base(&server.uri());
        OidcProvider::test_credentials(&cfg).await.unwrap();

        // Unreachable discovery host → error.
        let mut bad = config_with_base("http://127.0.0.1:1");
        bad.base_url = Some("http://127.0.0.1:1".into());
        assert!(OidcProvider::test_credentials(&bad).await.is_err());

        // Missing issuer/client_id → BadRequest before any network call.
        let mut blank = config_with_base(&server.uri());
        blank.issuer = String::new();
        assert!(OidcProvider::test_credentials(&blank).await.is_err());
    }
}

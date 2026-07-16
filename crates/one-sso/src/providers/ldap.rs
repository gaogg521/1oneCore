//! LDAP password authentication (Active Directory / OpenLDAP).
//!
//! Port of the 1ONE ClaudeCode `LdapAuthProvider.ts` reference:
//! 1. optional service bind (bindDN / bindAccount + password),
//! 2. subtree search for the login user (filter supports `{{username}}`),
//! 3. verify the password by binding as the found DN (falling back to the
//!    userPrincipalName on `invalidCredentials`, which some AD setups need),
//! 4. derive `external_id` (configured attribute, else the DN) and an
//!    org-unit path from department/company attributes or the DN's OU chain.

use std::collections::HashMap;
use std::time::Duration;

use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use serde::Deserialize;

use crate::error::SsoError;

const PASSWORD_MASK: &str = "******";
const LDAP_RC_INVALID_CREDENTIALS: u32 = 49;

/// Config JSON stored in `one_sso_providers.config` for `provider = 'ldap'`.
/// Field names mirror the 1one admin UI payload exactly.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LdapProviderConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "baseDN")]
    pub base_dn: String,
    /// Full bind DN / UPN used for the service bind (preferred when set).
    #[serde(default, rename = "bindDN")]
    pub bind_dn: String,
    /// sAMAccountName, DOMAIN\user, or user@domain when bindDN is empty.
    #[serde(default, rename = "bindAccount")]
    pub bind_account: String,
    #[serde(default, rename = "bindPassword")]
    pub bind_password: String,
    /// AD: sAMAccountName/userPrincipalName, OpenLDAP: uid.
    #[serde(default, rename = "loginAttribute")]
    pub login_attribute: String,
    /// Supports `{{username}}` placeholders.
    #[serde(default, rename = "searchFilter")]
    pub search_filter: String,
    /// When empty, the entry DN is used as the stable external id.
    #[serde(default, rename = "externalIdAttribute")]
    pub external_id_attribute: String,
    /// Accepted for config-compat with the 1one reference; group→role
    /// mapping is handled by one-org RBAC, not here.
    #[serde(default, rename = "adminGroupDN")]
    pub admin_group_dn: String,
    #[serde(default = "default_true", rename = "tlsRejectUnauthorized")]
    pub tls_reject_unauthorized: bool,
    #[serde(default, rename = "timeoutMs")]
    pub timeout_ms: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct LdapAuthSuccess {
    pub external_id: String,
    pub user_dn: String,
    pub org_unit_path: Option<String>,
}

pub struct LdapProvider;

impl LdapProvider {
    /// Authenticate `username`/`password` against the directory.
    ///
    /// Both "user not found" and "wrong password" collapse into
    /// `InvalidCredentials` so the login endpoint does not leak which
    /// accounts exist.
    pub async fn authenticate(
        config: &LdapProviderConfig,
        username: &str,
        password: &str,
    ) -> Result<LdapAuthSuccess, SsoError> {
        let url = config.url.trim();
        let base_dn = config.base_dn.trim();
        if url.is_empty() || base_dn.is_empty() {
            return Err(SsoError::ProviderNotConfigured("ldap".into()));
        }
        let username = username.trim();
        // An empty password would degrade the verification bind into an
        // anonymous bind, which most servers accept — reject upfront.
        if username.is_empty() || password.is_empty() {
            return Err(SsoError::InvalidCredentials);
        }

        let login_attr = resolve_login_attribute(config);
        let filter = build_search_filter(config, username, &login_attr);
        let mut attrs: Vec<String> = vec![
            login_attr.clone(),
            "memberOf".into(),
            "department".into(),
            "company".into(),
            "userPrincipalName".into(),
        ];
        let external_attr = config.external_id_attribute.trim();
        if !external_attr.is_empty() {
            attrs.push(external_attr.to_owned());
        }

        // 1) service bind (optional) + search for the user's DN.
        let mut service_conn = connect(config).await?;
        let bind_principal = resolve_bind_principal(config);
        let bind_password = config.bind_password.trim();
        if !bind_principal.is_empty() && !bind_password.is_empty() && bind_password != PASSWORD_MASK {
            let result = service_conn
                .simple_bind(&bind_principal, bind_password)
                .await
                .map_err(|e| SsoError::Internal(format!("ldap service bind: {e}")))?;
            if result.rc != 0 {
                let _ = service_conn.unbind().await;
                return Err(SsoError::Internal(format!(
                    "ldap service bind rejected (rc={}): check bindDN/bindAccount and bind password",
                    result.rc
                )));
            }
        }

        let attr_refs: Vec<&str> = attrs.iter().map(String::as_str).collect();
        let search = service_conn
            .search(base_dn, Scope::Subtree, &filter, attr_refs)
            .await
            .map_err(|e| SsoError::Internal(format!("ldap search: {e}")))?;
        let (entries, _res) = search
            .success()
            .map_err(|e| SsoError::Internal(format!("ldap search failed: {e}")))?;
        let _ = service_conn.unbind().await;

        let Some(first) = entries.into_iter().next() else {
            return Err(SsoError::InvalidCredentials);
        };
        let entry = SearchEntry::construct(first);
        let user_dn = entry.dn.clone();
        let record = lowercase_attr_map(&entry.attrs);

        let external_id = if external_attr.is_empty() {
            user_dn.clone()
        } else {
            pick_attr(&record, external_attr).unwrap_or_else(|| user_dn.clone())
        };

        // 2) verify the password by binding as the user.
        let mut user_conn = connect(config).await?;
        let bind_result = user_conn
            .simple_bind(&user_dn, password)
            .await
            .map_err(|e| SsoError::Internal(format!("ldap user bind: {e}")))?;
        let verified = if bind_result.rc == 0 {
            true
        } else if bind_result.rc == LDAP_RC_INVALID_CREDENTIALS {
            // Some AD setups reject DN binds but accept the UPN.
            let fallback = pick_attr(&record, "userPrincipalName")
                .or_else(|| pick_attr(&record, &login_attr))
                .filter(|principal| !principal.is_empty() && principal != &user_dn);
            match fallback {
                Some(principal) => {
                    let retry = user_conn
                        .simple_bind(&principal, password)
                        .await
                        .map_err(|e| SsoError::Internal(format!("ldap user bind: {e}")))?;
                    retry.rc == 0
                }
                None => false,
            }
        } else {
            let _ = user_conn.unbind().await;
            return Err(SsoError::Internal(format!(
                "ldap user bind failed (rc={})",
                bind_result.rc
            )));
        };
        let _ = user_conn.unbind().await;

        if !verified {
            return Err(SsoError::InvalidCredentials);
        }

        Ok(LdapAuthSuccess {
            org_unit_path: resolve_org_unit_path(&user_dn, &record),
            external_id,
            user_dn,
        })
    }
}

async fn connect(config: &LdapProviderConfig) -> Result<ldap3::Ldap, SsoError> {
    let timeout = Duration::from_millis(config.timeout_ms.filter(|ms| *ms > 0).unwrap_or(10_000));
    let mut settings = LdapConnSettings::new().set_conn_timeout(timeout);
    if config.url.trim().starts_with("ldaps://") && !config.tls_reject_unauthorized {
        settings = settings.set_no_tls_verify(true);
    }
    let (conn, ldap) = LdapConnAsync::with_settings(settings, config.url.trim())
        .await
        .map_err(|e| SsoError::Internal(format!("ldap connect: {e}")))?;
    ldap3::drive!(conn);
    Ok(ldap)
}

/// Basic LDAP filter escaping (RFC 4515 subset, same as the TS reference).
pub fn escape_filter_value(value: &str) -> String {
    value
        .replace('\\', "\\5c")
        .replace('*', "\\2a")
        .replace('(', "\\28")
        .replace(')', "\\29")
}

fn resolve_login_attribute(config: &LdapProviderConfig) -> String {
    let raw = config.login_attribute.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("dn") {
        "sAMAccountName".into()
    } else {
        raw.to_owned()
    }
}

fn build_search_filter(config: &LdapProviderConfig, username: &str, login_attr: &str) -> String {
    let default_filter = if username.contains('@') {
        format!("(|({login_attr}={{{{username}}}})(userPrincipalName={{{{username}}}})(mail={{{{username}}}}))")
    } else {
        format!("(|({login_attr}={{{{username}}}})(sAMAccountName={{{{username}}}})(uid={{{{username}}}}))")
    };
    let raw = {
        let configured = config.search_filter.trim();
        if configured.is_empty() {
            default_filter
        } else {
            configured.to_owned()
        }
    };
    let safe = escape_filter_value(username);
    replace_username_placeholder(&raw, &safe)
}

/// Replace `{{username}}` (whitespace-tolerant, case-insensitive) like the
/// TS reference's `/\{\{\s*username\s*\}\}/gi`.
fn replace_username_placeholder(filter: &str, value: &str) -> String {
    let mut out = String::with_capacity(filter.len() + value.len());
    let bytes = filter.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let rest = &filter[i + 2..];
            let inner_end = rest.find("}}");
            if let Some(end) = inner_end {
                let inner = rest[..end].trim();
                if inner.eq_ignore_ascii_case("username") {
                    out.push_str(value);
                    i += 2 + end + 2;
                    continue;
                }
            }
        }
        // Advance one UTF-8 code point.
        let ch_len = filter[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&filter[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// LDAP bind principal: prefer explicit bindDN; else bindAccount
/// (UPN / DOMAIN\user as-is, bare sAMAccountName + domain inferred from the
/// base DN's DC components).
pub fn resolve_bind_principal(config: &LdapProviderConfig) -> String {
    let bind_dn = config.bind_dn.trim();
    if !bind_dn.is_empty() {
        return bind_dn.to_owned();
    }
    let account = config.bind_account.trim();
    if account.is_empty() {
        return String::new();
    }
    if account.contains('@') || account.contains('\\') {
        return account.to_owned();
    }
    match base_dn_to_dns_domain(config.base_dn.trim()) {
        Some(domain) => format!("{account}@{domain}"),
        None => account.to_owned(),
    }
}

/// `DC=intranet,DC=example,DC=com` → `intranet.example.com`
fn base_dn_to_dns_domain(base_dn: &str) -> Option<String> {
    let labels: Vec<String> = base_dn
        .split(',')
        .map(str::trim)
        .filter(|part| part.len() >= 3 && part[..3].eq_ignore_ascii_case("dc="))
        .map(|part| part[3..].trim().to_owned())
        .filter(|label| !label.is_empty())
        .collect();
    if labels.is_empty() {
        None
    } else {
        Some(labels.join("."))
    }
}

fn lowercase_attr_map(attrs: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    attrs
        .iter()
        .map(|(key, values)| (key.to_lowercase(), values.clone()))
        .collect()
}

fn pick_attr(record: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    record
        .get(&key.to_lowercase())
        .and_then(|values| values.first())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Parse OU segments from a DN (leaf-to-root) and return root-to-leaf labels.
pub fn parse_ou_chain_from_dn(dn: &str) -> Vec<String> {
    let mut ous: Vec<String> = dn
        .split(',')
        .map(str::trim)
        .filter(|part| part.len() >= 3 && part[..3].eq_ignore_ascii_case("ou="))
        .map(|part| part[3..].trim().to_owned())
        .filter(|label| !label.is_empty())
        .collect();
    ous.reverse();
    ous
}

/// `department` (fallback: DN OU chain) prefixed with `company` when present —
/// same composition rule as the 1one reference.
pub fn resolve_org_unit_path(dn: &str, record: &HashMap<String, Vec<String>>) -> Option<String> {
    let department = pick_attr(record, "department");
    let company = pick_attr(record, "company");
    let ou_path = {
        let chain = parse_ou_chain_from_dn(dn);
        if chain.is_empty() {
            None
        } else {
            Some(chain.join(" / "))
        }
    };

    let path = department.or(ou_path);
    match (path, company) {
        (Some(path), Some(company)) => Some(format!("{company} / {path}")),
        (Some(path), None) => Some(path),
        (None, Some(company)) => Some(company),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(key, values)| (key.to_lowercase(), values.iter().map(|v| (*v).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn escape_filter_value_escapes_special_chars() {
        assert_eq!(escape_filter_value(r"a*b(c)d\e"), r"a\2ab\28c\29d\5ce");
    }

    #[test]
    fn default_filter_switches_on_email_style_logins() {
        let config = LdapProviderConfig::default();
        let plain = build_search_filter(&config, "zhang.san", "sAMAccountName");
        assert_eq!(
            plain,
            "(|(sAMAccountName=zhang.san)(sAMAccountName=zhang.san)(uid=zhang.san))"
        );
        let mail = build_search_filter(&config, "zhang@corp.com", "sAMAccountName");
        assert!(mail.contains("(userPrincipalName=zhang@corp.com)"));
        assert!(mail.contains("(mail=zhang@corp.com)"));
    }

    #[test]
    fn custom_filter_placeholder_is_replaced_and_escaped() {
        let config = LdapProviderConfig {
            search_filter: "(&(objectClass=user)(cn={{ Username }}))".into(),
            ..Default::default()
        };
        let filter = build_search_filter(&config, "a(b)*", "uid");
        assert_eq!(filter, r"(&(objectClass=user)(cn=a\28b\29\2a))");
    }

    #[test]
    fn bind_principal_prefers_dn_then_account_with_inferred_domain() {
        let config = LdapProviderConfig {
            bind_dn: "CN=svc,DC=corp,DC=com".into(),
            bind_account: "svc".into(),
            base_dn: "DC=corp,DC=com".into(),
            ..Default::default()
        };
        assert_eq!(resolve_bind_principal(&config), "CN=svc,DC=corp,DC=com");

        let config = LdapProviderConfig {
            bind_account: "svc".into(),
            base_dn: "DC=intranet,DC=example,DC=com".into(),
            ..Default::default()
        };
        assert_eq!(resolve_bind_principal(&config), "svc@intranet.example.com");

        let config = LdapProviderConfig {
            bind_account: r"CORP\svc".into(),
            base_dn: "DC=corp,DC=com".into(),
            ..Default::default()
        };
        assert_eq!(resolve_bind_principal(&config), r"CORP\svc");
    }

    #[test]
    fn org_unit_path_prefers_department_and_prefixes_company() {
        let dn = "CN=Zhang San,OU=Dev,OU=Engineering,DC=corp,DC=com";
        let with_dept = record(&[("department", &["Platform"]), ("company", &["Acme"])]);
        assert_eq!(
            resolve_org_unit_path(dn, &with_dept).as_deref(),
            Some("Acme / Platform")
        );

        let empty = record(&[]);
        assert_eq!(resolve_org_unit_path(dn, &empty).as_deref(), Some("Engineering / Dev"));

        let company_only = record(&[("company", &["Acme"])]);
        assert_eq!(
            resolve_org_unit_path("CN=x,DC=corp,DC=com", &company_only).as_deref(),
            Some("Acme")
        );
        assert_eq!(resolve_org_unit_path("CN=x,DC=corp,DC=com", &empty), None);
    }

    #[test]
    fn config_parses_1one_reference_field_names() {
        let json = r#"{
            "url": "ldaps://dc.corp.com:636",
            "baseDN": "DC=corp,DC=com",
            "bindDN": "CN=svc,DC=corp,DC=com",
            "bindPassword": "secret",
            "loginAttribute": "sAMAccountName",
            "externalIdAttribute": "objectGUID",
            "tlsRejectUnauthorized": false,
            "timeoutMs": 5000
        }"#;
        let config: LdapProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.base_dn, "DC=corp,DC=com");
        assert_eq!(config.bind_dn, "CN=svc,DC=corp,DC=com");
        assert_eq!(config.external_id_attribute, "objectGUID");
        assert!(!config.tls_reject_unauthorized);
        assert_eq!(config.timeout_ms, Some(5000));
    }
}

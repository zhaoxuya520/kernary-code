#![forbid(unsafe_code)]

//! Auth metadata 与 secret-safe CredentialStore Port。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Mutex;

use harness_types::{AccountId, ProviderId};
use serde::{Deserialize, Serialize};

pub const OPENAI_API_KEY_CREDENTIAL_ID: &str = "openai:default";

/// CredentialStore 内部使用的稳定引用；它本身不是 secret。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialId(String);

impl CredentialId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 不实现 Serialize/Clone；Debug/Display 永远不显示内容，Drop 尽力清零。
pub struct SecretString {
    bytes: Vec<u8>,
}

impl SecretString {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            bytes: value.into().into_bytes(),
        }
    }

    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn expose_secret(&self) -> Result<&str, AuthError> {
        std::str::from_utf8(&self.bytes)
            .map_err(|_| AuthError::new("credential-not-utf8", "Credential 不是 UTF-8"))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Debug for SecretString {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl Display for SecretString {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    ApiKey,
    CodexDelegated,
    WorkloadIdentity,
}

/// 可展示、可持久化的账户元数据；不含 credential value。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountMetadata {
    pub id: AccountId,
    pub provider_id: ProviderId,
    pub display_name: String,
    pub auth_method: AuthMethod,
    pub credential_id: Option<CredentialId>,
    pub created_at_millis: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthError {
    pub code: String,
    pub message: String,
}

impl AuthError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Display for AuthError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for AuthError {}

/// OS Credential Manager / Keychain / Secret Service 的 Port。
pub trait CredentialStore: Send + Sync {
    fn put(&self, id: &CredentialId, secret: SecretString) -> Result<(), AuthError>;
    fn get(&self, id: &CredentialId) -> Result<Option<SecretString>, AuthError>;
    fn delete(&self, id: &CredentialId) -> Result<bool, AuthError>;
}

/// Windows Credential Manager / macOS Keychain / Linux Secret Service Adapter。
///
/// 所有调用串行化，避免平台 Store 对并发顺序的差异。
pub struct OsCredentialStore {
    service: String,
    access: Mutex<()>,
}

impl OsCredentialStore {
    pub fn new(service: impl Into<String>) -> Result<Self, AuthError> {
        let service = service.into();
        if service.trim().is_empty() {
            return Err(AuthError::new(
                "credential-service-empty",
                "Credential service 名称不能为空",
            ));
        }
        Ok(Self {
            service,
            access: Mutex::new(()),
        })
    }

    pub fn available() -> Result<(), AuthError> {
        keyring::v1::Entry::store_status()
            .as_ref()
            .copied()
            .map_err(|error| keyring_error("credential-store-unavailable", error))
    }

    fn entry(&self, id: &CredentialId) -> Result<keyring::v1::Entry, AuthError> {
        keyring::v1::Entry::new(&self.service, id.as_str())
            .map_err(|error| keyring_error("credential-entry", &error))
    }
}

impl CredentialStore for OsCredentialStore {
    fn put(&self, id: &CredentialId, secret: SecretString) -> Result<(), AuthError> {
        if secret.is_empty() {
            return Err(AuthError::new("empty-credential", "Credential 不能为空"));
        }
        let _guard = self
            .access
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "OS Store lock"))?;
        self.entry(id)?
            .set_secret(&secret.bytes)
            .map_err(|error| keyring_error("credential-write", &error))
    }

    fn get(&self, id: &CredentialId) -> Result<Option<SecretString>, AuthError> {
        let _guard = self
            .access
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "OS Store lock"))?;
        match self.entry(id)?.get_secret() {
            Ok(bytes) => Ok(Some(SecretString::from_bytes(bytes))),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_error("credential-read", &error)),
        }
    }

    fn delete(&self, id: &CredentialId) -> Result<bool, AuthError> {
        let _guard = self
            .access
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "OS Store lock"))?;
        match self.entry(id)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::v1::Error::NoEntry) => Ok(false),
            Err(error) => Err(keyring_error("credential-delete", &error)),
        }
    }
}

fn keyring_error(code: &'static str, error: &keyring::v1::Error) -> AuthError {
    AuthError::new(code, error.to_string())
}

/// 只用于测试和当前进程的 Store；不会持久化到磁盘。
#[derive(Default)]
pub struct MemoryCredentialStore {
    values: Mutex<BTreeMap<CredentialId, Vec<u8>>>,
}

impl MemoryCredentialStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn put(&self, id: &CredentialId, secret: SecretString) -> Result<(), AuthError> {
        if secret.is_empty() {
            return Err(AuthError::new("empty-credential", "Credential 不能为空"));
        }
        let mut values = self
            .values
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "Credential Store lock"))?;
        values.insert(id.clone(), secret.bytes.clone());
        Ok(())
    }

    fn get(&self, id: &CredentialId) -> Result<Option<SecretString>, AuthError> {
        let values = self
            .values
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "Credential Store lock"))?;
        Ok(values.get(id).cloned().map(|bytes| SecretString { bytes }))
    }

    fn delete(&self, id: &CredentialId) -> Result<bool, AuthError> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| AuthError::new("credential-store-poisoned", "Credential Store lock"))?;
        if let Some(mut value) = values.remove(id) {
            value.fill(0);
            return Ok(true);
        }
        Ok(false)
    }
}

/// 账户元数据管理；Credential 仍只通过 CredentialStore 访问。
#[derive(Default)]
pub struct AccountManager {
    accounts: BTreeMap<AccountId, AccountMetadata>,
    active_by_provider: BTreeMap<ProviderId, AccountId>,
}

impl AccountManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, account: AccountMetadata) -> Result<(), AuthError> {
        if self.accounts.contains_key(&account.id) {
            return Err(AuthError::new("account-exists", account.id.to_string()));
        }
        self.accounts.insert(account.id.clone(), account);
        Ok(())
    }

    pub fn activate(&mut self, account_id: &AccountId) -> Result<(), AuthError> {
        let account = self
            .accounts
            .get(account_id)
            .ok_or_else(|| AuthError::new("account-not-found", account_id.to_string()))?;
        self.active_by_provider
            .insert(account.provider_id.clone(), account_id.clone());
        Ok(())
    }

    #[must_use]
    pub fn active(&self, provider_id: &ProviderId) -> Option<&AccountMetadata> {
        self.active_by_provider
            .get(provider_id)
            .and_then(|id| self.accounts.get(id))
    }

    #[must_use]
    pub fn list(&self) -> Vec<AccountMetadata> {
        self.accounts.values().cloned().collect()
    }
}

/// 官方未发布第三方 OAuth/Device Flow 契约时保持关闭。
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeBrowserAuthGate;

impl NativeBrowserAuthGate {
    pub fn begin(&self) -> Result<(), AuthError> {
        Err(AuthError::new(
            "native-browser-auth-disabled",
            "请使用 API Key，或通过官方 Codex delegated adapter 登录 ChatGPT",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted_and_store_round_trips_without_metadata_leak() {
        let store = MemoryCredentialStore::new();
        let id = CredentialId::new("openai:account:test");
        let secret = SecretString::new("sk-test-do-not-log");
        assert_eq!(format!("{secret:?}"), "SecretString([REDACTED])");
        assert_eq!(secret.to_string(), "[REDACTED]");
        store.put(&id, secret).expect("put");
        let loaded = store.get(&id).expect("get").expect("exists");
        assert_eq!(loaded.expose_secret().expect("utf8"), "sk-test-do-not-log");
        assert!(store.delete(&id).expect("delete"));
        assert!(store.get(&id).expect("missing").is_none());
    }

    #[test]
    fn account_metadata_serialization_never_contains_secret() {
        let account = AccountMetadata {
            id: AccountId::from("account:test"),
            provider_id: ProviderId::from("openai"),
            display_name: "OpenAI work".to_owned(),
            auth_method: AuthMethod::ApiKey,
            credential_id: Some(CredentialId::new("openai:account:test")),
            created_at_millis: 1,
        };
        let json = serde_json::to_string(&account).expect("metadata json");
        assert!(!json.contains("sk-"));
    }

    #[test]
    fn native_browser_auth_is_fail_closed() {
        assert_eq!(
            NativeBrowserAuthGate.begin().expect_err("disabled").code,
            "native-browser-auth-disabled"
        );
    }

    #[test]
    fn os_store_rejects_empty_service_without_platform_side_effect() {
        assert_eq!(
            OsCredentialStore::new(" ")
                .err()
                .expect("empty service")
                .code,
            "credential-service-empty"
        );
    }

    #[test]
    #[ignore = "设置 HARNESS_OS_KEYRING_TEST=1 后显式验证真实 OS Credential Store"]
    fn os_store_round_trip_is_opt_in() {
        if std::env::var("HARNESS_OS_KEYRING_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let store = OsCredentialStore::new("dev.openai.harness.test").expect("store");
        let id = CredentialId::new(format!("test:{}", std::process::id()));
        store
            .put(&id, SecretString::new("test-secret"))
            .expect("put");
        assert_eq!(
            store
                .get(&id)
                .expect("get")
                .expect("secret")
                .expose_secret()
                .expect("utf8"),
            "test-secret"
        );
        assert!(store.delete(&id).expect("delete"));
    }
}

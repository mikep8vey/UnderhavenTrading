//! Authentication, sessions, CSRF and encryption of optional read-only keys.
//!
//! * Passwords: Argon2id with a random 16-byte salt and the `argon2` crate's
//!   default (19 MiB, t=2, p=1) parameters.
//! * Session tokens: 32 random bytes, base64url without padding. Only the
//!   SHA-256 hash is stored in SQLite; the raw token exists only in the browser
//!   cookie.
//! * Cookies: `HttpOnly; SameSite=Strict; Path=/` and `Secure` when the public
//!   deployment terminates TLS. The server refuses a non-loopback bind without
//!   either Secure cookies or an explicit insecure override (see `config`).
//! * CSRF: a per-session random token, sent to the authenticated SPA via
//!   `/api/session` and required in `X-CSRF-Token` on every mutation.
//! * Optional Bybit API keys: AES-256-GCM with a 32-byte master key read from a
//!   file outside SQLite. Keys are never returned by any API endpoint.

use crate::config::parse_hex_or_base64;
use crate::store::{AuthUser, SessionRow, Store};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::Path;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

pub const COOKIE_NAME: &str = "uh_session";
const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;
const MIN_PASSWORD_LEN: usize = 12;

#[derive(Clone)]
pub struct Auth {
    store: Store,
    master_key: [u8; 32],
    secure_cookies: bool,
    session_ttl_ms: i64,
}

#[derive(Debug, Clone)]
pub struct LoginSession {
    pub token: String,
    pub csrf_token: String,
    pub expires_at_ms: i64,
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A valid Argon2id hash used to equalise work for unknown usernames. It is
/// generated once and never accepted as a credential.
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("underhaven-dummy-password-not-a-credential")
            .unwrap_or_else(|_| String::new())
    })
}

pub fn validate_password_policy(password: &str, username: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    if password
        .to_ascii_lowercase()
        .contains(&username.to_ascii_lowercase())
    {
        bail!("password must not contain the username");
    }
    let classes = [
        password.chars().any(|c| c.is_ascii_lowercase()),
        password.chars().any(|c| c.is_ascii_uppercase()),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| !c.is_ascii_alphanumeric()),
    ]
    .iter()
    .filter(|v| **v)
    .count();
    if classes < 3 {
        bail!("password must use at least three of: lowercase, uppercase, digits, symbols");
    }
    Ok(())
}

/// Load the AES master key. The key never enters the database. A missing file is
/// generated with restrictive permissions on Unix.
pub fn load_master_key(path: &Path) -> Result<[u8; 32]> {
    if let Ok(env_value) = std::env::var("UNDERHAVEN_MASTER_KEY") {
        let bytes = parse_hex_or_base64(&env_value)
            .context("UNDERHAVEN_MASTER_KEY must be 32 bytes hex or base64")?;
        if bytes.len() != 32 {
            bail!("UNDERHAVEN_MASTER_KEY must decode to exactly 32 bytes");
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    if path.exists() {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() < 32 {
            bail!("master key file {} is too short", path.display());
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[..32]);
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    std::fs::write(path, key).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(key)
}

impl Auth {
    pub fn new(
        store: Store,
        master_key: [u8; 32],
        secure_cookies: bool,
        session_ttl_ms: Option<i64>,
    ) -> Self {
        Self {
            store,
            master_key,
            secure_cookies,
            session_ttl_ms: session_ttl_ms.unwrap_or(SESSION_TTL_MS),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn login(
        &self,
        username: &str,
        password: &str,
        ip: &str,
        user_agent: &str,
    ) -> Result<LoginSession> {
        let user = self.store.user_by_name(username).await?;
        // Always perform one Argon2 verification to keep timing similar for
        // unknown users and wrong passwords.
        let valid = match &user {
            Some(u) if !u.disabled => verify_password(password, &u.password_hash),
            _ => {
                let _ = verify_password(password, dummy_hash());
                false
            }
        };
        if !valid {
            self.store.record_login(ip, username, false).await?;
            bail!("invalid username or password");
        }
        let user: AuthUser = user.expect("valid password implies existing user");
        let token = generate_token();
        let csrf = generate_token();
        let expires = chrono::Utc::now().timestamp_millis() + self.session_ttl_ms;
        self.store
            .create_session(
                &user.id,
                &hash_token(&token),
                &csrf,
                expires,
                ip,
                &user_agent.chars().take(200).collect::<String>(),
            )
            .await?;
        self.store.record_login(ip, username, true).await?;
        self.store
            .audit(
                "auth.login",
                &user.username,
                &format!("session created from {ip}"),
            )
            .await?;
        Ok(LoginSession {
            token,
            csrf_token: csrf,
            expires_at_ms: expires,
        })
    }

    pub async fn session(&self, token: &str) -> Option<SessionRow> {
        if token.is_empty() {
            return None;
        }
        let hash = hash_token(token);
        match self.store.session_by_token(&hash).await {
            Ok(Some(session)) if session.expires_at_ms > chrono::Utc::now().timestamp_millis() => {
                let _ = self.store.touch_session(&session.id).await;
                Some(session)
            }
            _ => None,
        }
    }

    pub async fn logout(&self, token: &str, actor: &str) -> Result<()> {
        if let Some(session) = self.session(token).await {
            self.store.revoke_session(&session.id).await?;
            self.store
                .audit("auth.logout", actor, "session revoked")
                .await?;
        }
        Ok(())
    }

    pub async fn change_password(&self, user: &AuthUser, current: &str, next: &str) -> Result<()> {
        if !verify_password(current, &user.password_hash) {
            bail!("current password is incorrect");
        }
        validate_password_policy(next, &user.username)?;
        let hash = hash_password(next)?;
        self.store.update_password(&user.id, &hash).await?;
        self.store.revoke_all_sessions(&user.id).await?;
        self.store
            .audit(
                "auth.password_change",
                &user.username,
                "all sessions revoked",
            )
            .await?;
        Ok(())
    }

    pub fn cookie_header(&self, token: &str, max_age_secs: i64) -> String {
        format!(
            "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{}",
            if self.secure_cookies { "; Secure" } else { "" }
        )
    }

    pub fn clear_cookie(&self) -> String {
        format!(
            "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
            if self.secure_cookies { "; Secure" } else { "" }
        )
    }

    pub fn secure_cookies(&self) -> bool {
        self.secure_cookies
    }

    pub fn encrypt_secret(&self, plaintext: &str) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(&self.master_key)
            .map_err(|_| anyhow::anyhow!("invalid master key length"))?;
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("AES-GCM encryption failed"))?;
        let mut blob = Vec::with_capacity(12 + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(format!("v1.{}", URL_SAFE_NO_PAD.encode(blob)))
    }

    pub fn decrypt_secret(&self, encoded: &str) -> Result<String> {
        let raw = encoded
            .strip_prefix("v1.")
            .context("unsupported secret encoding")?;
        let blob = URL_SAFE_NO_PAD
            .decode(raw)
            .context("secret is not valid base64url")?;
        if blob.len() < 13 {
            bail!("secret blob is truncated");
        }
        let (nonce_bytes, ciphertext) = blob.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&self.master_key)
            .map_err(|_| anyhow::anyhow!("invalid master key length"))?;
        let nonce = Nonce::from_slice(nonce_bytes);
        let mut plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| anyhow::anyhow!("AES-GCM authentication failed"))?;
        let value = String::from_utf8(plaintext.clone()).context("secret is not UTF-8")?;
        plaintext.zeroize();
        Ok(value)
    }

    pub fn store_read_only_key(
        &self,
        _name: &str,
        _uid: Option<&str>,
        _note: Option<&str>,
        _api_key: &str,
        api_secret: &str,
        key_info: &crate::bybit::ReadOnlyKeyInfo,
    ) -> Result<()> {
        if !key_info.read_only {
            bail!("refusing to store a key that is not read-only");
        }
        if api_secret.len() < 16 {
            bail!("API secret looks too short");
        }
        Ok(())
    }

    pub async fn persist_read_only_key(
        &self,
        name: &str,
        uid: Option<&str>,
        note: Option<&str>,
        api_key: &str,
        api_secret: &str,
        key_info: &crate::bybit::ReadOnlyKeyInfo,
    ) -> Result<()> {
        self.store_read_only_key(name, uid, note, api_key, api_secret, key_info)?;
        let enc_key = self.encrypt_secret(api_key)?;
        let enc_secret = self.encrypt_secret(api_secret)?;
        self.store
            .save_credential(name, &enc_key, &enc_secret, uid, note)
            .await?;
        self.store
            .audit(
                "credentials.store",
                "admin",
                &format!("read-only key '{name}' stored; uid={uid:?}"),
            )
            .await?;
        Ok(())
    }

    pub async fn load_credentials(&self) -> Result<Option<crate::bybit::ApiCredentials>> {
        let Some(row) = self.store.credential().await? else {
            return Ok(None);
        };
        if !row.read_only {
            bail!("stored credential is not marked read-only; refusing to load");
        }
        Ok(Some(crate::bybit::ApiCredentials {
            api_key: self.decrypt_secret(&row.api_key_enc)?,
            api_secret: self.decrypt_secret(&row.api_secret_enc)?,
        }))
    }
}

/// CSRF comparison in constant time.
pub fn csrf_matches(expected: &str, provided: &str) -> bool {
    if expected.is_empty() || provided.is_empty() {
        return false;
    }
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

/// Minimal cookie parser: returns the value of `name` from a `Cookie` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let mut kv = part.trim().splitn(2, '=');
        let k = kv.next()?.trim();
        let v = kv.next().unwrap_or("").trim();
        if k == name {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn auth() -> (Auth, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::connect(&dir.path().join("a.db")).await.unwrap();
        store.ensure_wallets(100.0).await.unwrap();
        let key = [7u8; 32];
        let auth = Auth::new(store, key, true, None);
        (auth, dir)
    }

    #[test]
    fn password_hash_roundtrip_and_policy() {
        let hash = hash_password("CorrectHorse9!xyz").unwrap();
        assert!(verify_password("CorrectHorse9!xyz", &hash));
        assert!(!verify_password("wrong", &hash));
        assert!(validate_password_policy("CorrectHorse9!xyz", "admin").is_ok());
        assert!(validate_password_policy("short", "admin").is_err());
        assert!(validate_password_policy("AdminAdmin123!", "admin").is_err());
    }

    #[test]
    fn aes_gcm_roundtrip_and_tamper_detection() {
        let key = [9u8; 32];
        let dir = TempDir::new().unwrap();
        // Construct without a store is not possible; use a real store.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let auth = rt.block_on(async {
            let store = Store::connect(&dir.path().join("b.db")).await.unwrap();
            Auth::new(store, key, true, None)
        });
        let enc = auth.encrypt_secret("my-secret-value").unwrap();
        assert_ne!(enc, "my-secret-value");
        assert_eq!(auth.decrypt_secret(&enc).unwrap(), "my-secret-value");
        let mut tampered = enc.clone().into_bytes();
        let last = tampered.len() - 2;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(auth.decrypt_secret(&tampered).is_err());
    }

    #[tokio::test]
    async fn session_lifecycle_and_csrf() {
        let (auth, _dir) = auth().await;
        auth.store
            .create_user("admin", &hash_password("CorrectHorse9!xyz").unwrap())
            .await
            .unwrap();
        let session = auth
            .login("admin", "CorrectHorse9!xyz", "127.0.0.1", "test")
            .await
            .unwrap();
        assert!(session.token.len() >= 40);
        let found = auth.session(&session.token).await.unwrap();
        assert_eq!(found.username, "admin");
        assert!(csrf_matches(&found.csrf_token, &session.csrf_token));
        auth.logout(&session.token, "admin").await.unwrap();
        assert!(auth.session(&session.token).await.is_none());
    }

    #[tokio::test]
    async fn failed_login_is_recorded() {
        let (auth, _dir) = auth().await;
        auth.store
            .create_user("admin", &hash_password("CorrectHorse9!xyz").unwrap())
            .await
            .unwrap();
        assert!(auth
            .login("admin", "wrong", "1.2.3.4", "test")
            .await
            .is_err());
        let count = auth.store.recent_failed_logins("1.2.3.4", 0).await.unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cookie_has_security_flags() {
        let dir = TempDir::new().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let auth = rt.block_on(async {
            let store = Store::connect(&dir.path().join("c.db")).await.unwrap();
            Auth::new(store, [1u8; 32], true, None)
        });
        let header = auth.cookie_header("token", 100);
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Secure"));
        assert!(auth.clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn cookie_parser_handles_multiple_values() {
        let header = "a=1; uh_session=abc; b=2";
        assert_eq!(cookie_value(header, "uh_session").as_deref(), Some("abc"));
        assert!(cookie_value(header, "missing").is_none());
    }
}

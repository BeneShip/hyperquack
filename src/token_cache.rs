use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use time::OffsetDateTime;

use crate::motherduck_client::TokenType;

/// Discount tokens whose remaining lifetime is under this margin, so a token we
/// hand out never expires while the spawned command is still using it.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// Persistent, best-effort cache of MotherDuck tokens keyed on the service
/// account and token type. Reusing unexpired tokens avoids minting a fresh one
/// on every invocation (and, for `map-db`, on every database).
#[derive(Debug)]
pub struct TokenCache {
    conn: Connection,
}

impl TokenCache {
    /// `$XDG_CONFIG_HOME/hyperquack/tokens.sqlite`, else
    /// `~/.config/hyperquack/tokens.sqlite`. We deliberately anchor on
    /// `~/.config` rather than `dirs::config_dir()` so the path is the same on
    /// every platform (on macOS `config_dir()` is `~/Library/Application Support`).
    fn cache_path() -> Option<PathBuf> {
        let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => dirs::home_dir()?.join(".config"),
        };
        Some(config_home.join("hyperquack").join("tokens.sqlite"))
    }

    /// Open (creating if needed) the cache at the default path, initialize the
    /// schema, and drop any already-expired rows.
    pub fn open_default() -> Result<Self> {
        let path = Self::cache_path().context("Could not determine cache path")?;
        Self::open(&path)
    }

    /// Open the cache at an explicit path: create the parent directory, open the
    /// connection, restrict permissions (it holds bearer tokens), initialize the
    /// schema, and prune expired rows.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Creating cache directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Opening token cache {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }

        let cache = Self { conn };
        cache.init()?;
        cache.prune_expired()?;
        Ok(cache)
    }

    fn init(&self) -> Result<()> {
        self.conn
            .execute(
                "CREATE TABLE IF NOT EXISTS tokens (
                    member_name TEXT NOT NULL,
                    token_type  TEXT NOT NULL,
                    token       TEXT NOT NULL,
                    expire_at   INTEGER NOT NULL,
                    PRIMARY KEY (member_name, token_type)
                )",
                [],
            )
            .context("Initializing token cache schema")?;
        Ok(())
    }

    /// Remove every token that has already expired.
    pub fn prune_expired(&self) -> Result<usize> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let removed = self
            .conn
            .execute("DELETE FROM tokens WHERE expire_at <= ?1", params![now])
            .context("Pruning expired tokens")?;
        Ok(removed)
    }

    /// Return a cached token for this account and type that still has at least
    /// `EXPIRY_MARGIN_SECS` of life left, if any.
    pub fn get(&self, member_name: &str, token_type: TokenType) -> Result<Option<String>> {
        let cutoff = OffsetDateTime::now_utc().unix_timestamp() + EXPIRY_MARGIN_SECS;
        let token = self
            .conn
            .query_row(
                "SELECT token FROM tokens
                 WHERE member_name = ?1 AND token_type = ?2 AND expire_at > ?3",
                params![member_name, token_type.as_str(), cutoff],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Reading cached token")?;
        Ok(token)
    }

    pub fn put(
        &self,
        member_name: &str,
        token_type: TokenType,
        token: &str,
        expire_at: OffsetDateTime,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO tokens (member_name, token_type, token, expire_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    member_name,
                    token_type.as_str(),
                    token,
                    expire_at.unix_timestamp()
                ],
            )
            .context("Writing cached token")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache() -> (TokenCache, PathBuf) {
        // A unique path per test process/thread without depending on a temp-dir
        // crate. std::env::temp_dir + the test thread name keeps them distinct.
        let name = std::thread::current().name().unwrap_or("test").to_owned();
        let dir = std::env::temp_dir().join(format!("hq-token-cache-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tokens.sqlite");
        (TokenCache::open(&path).expect("open cache"), path)
    }

    fn secs_from_now(secs: i64) -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::seconds(secs)
    }

    #[test]
    fn roundtrips_a_live_token() {
        let (cache, _path) = temp_cache();
        cache
            .put("acct", TokenType::ReadWrite, "tok-1", secs_from_now(3600))
            .unwrap();
        assert_eq!(
            cache.get("acct", TokenType::ReadWrite).unwrap().as_deref(),
            Some("tok-1")
        );
    }

    #[test]
    fn distinguishes_token_type() {
        let (cache, _path) = temp_cache();
        cache
            .put("acct", TokenType::ReadWrite, "rw", secs_from_now(3600))
            .unwrap();
        assert_eq!(cache.get("acct", TokenType::ReadScaling).unwrap(), None);
    }

    #[test]
    fn hides_tokens_inside_the_expiry_margin() {
        let (cache, _path) = temp_cache();
        // Still valid, but expiring within EXPIRY_MARGIN_SECS: treated as unusable.
        cache
            .put("acct", TokenType::ReadWrite, "soon", secs_from_now(30))
            .unwrap();
        assert_eq!(cache.get("acct", TokenType::ReadWrite).unwrap(), None);
    }

    #[test]
    fn prunes_expired_rows() {
        let (cache, _path) = temp_cache();
        cache
            .put("acct", TokenType::ReadWrite, "old", secs_from_now(-10))
            .unwrap();
        let removed = cache.prune_expired().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(cache.get("acct", TokenType::ReadWrite).unwrap(), None);
    }
}

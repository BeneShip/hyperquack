use anyhow::{Context, Result};
use bon::Builder;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, time::Duration};
use suggest::Suggest;
use time::OffsetDateTime;
use ureq::Agent;
use uuid::Uuid;

use crate::token_cache::TokenCache;

#[derive(Debug, Builder, Clone)]
#[builder(on(String, into))]
pub struct MotherduckClient {
    pub admin_token: String,
    #[builder(default = MotherduckClient::build_agent())]
    agent: Agent,
    /// Best-effort local token cache. `None` disables caching (e.g. when the
    /// cache file cannot be opened); the client then mints fresh tokens.
    cache: Option<Arc<Mutex<TokenCache>>>,
}

pub static USER_AGENT: &str = "Paccurate HyperQuack";

/// Lifetime requested for newly minted tokens. Long enough that the on-disk
/// cache yields reuse across separate CLI runs within a work session.
pub const DEFAULT_TTL: Duration = Duration::from_secs(86_400);

impl MotherduckClient {
    fn build_agent() -> Agent {
        Agent::config_builder()
            .user_agent(USER_AGENT)
            .build()
            .into()
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.admin_token)
    }

    /// List active (non-deprovisioned) org members via the official REST API,
    /// following `next_page_token` until exhausted. `is_service_account`
    /// filters server-side when set.
    ///
    /// https://motherduck.com/docs/key-tasks/service-accounts-guide/manage-service-accounts-and-tokens/#view-service-accounts
    pub fn list_users(&self, is_service_account: Option<bool>) -> Result<Vec<Member>> {
        let url = "https://api.motherduck.com/v1/users";
        let mut members = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut request = self
                .agent
                .get(url)
                .header("Authorization", &self.auth_header())
                .header("Accept", "application/json")
                .query("is_deprovisioned", "false")
                .query("limit", USERS_PAGE_LIMIT.to_string());
            if let Some(is_service_account) = is_service_account {
                request = request.query("is_service_account", is_service_account.to_string());
            }
            if let Some(token) = &page_token {
                request = request.query("page_token", token);
            }

            let page = request
                .call()
                .context(url)?
                .body_mut()
                .read_json::<UsersPage>()
                .context("Parsing /v1/users response")?;

            members.extend(page.users);
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }

        Ok(members)
    }

    pub fn get_service_accounts(&self) -> Result<Vec<Member>> {
        self.list_users(Some(true))
    }

    pub fn get_member_by_name(&self, name: &str) -> Result<Member> {
        let members: HashMap<String, Member> = self
            .list_users(None)?
            .into_iter()
            .map(|m| (m.name.clone(), m))
            .collect();

        match members.get(name) {
            Some(member) => Ok(member.clone()),
            None => match members.into_keys().collect::<Vec<_>>().suggest(name) {
                Some(suggestion) => Err(anyhow::anyhow!(
                    "No member found with name {}. Did you mean {}?",
                    name,
                    suggestion
                )),
                None => Err(anyhow::anyhow!("No member found with name {}", name)),
            },
        }
    }

    /// Return a token for `member`, reusing an unexpired cached one when
    /// available and otherwise minting (and caching) a fresh one. All cache
    /// interaction is best-effort: a poisoned lock or SQLite error is ignored
    /// and we fall through to minting a new token.
    pub fn token_for(
        &self,
        member: &Member,
        token_type: TokenType,
        ttl: Option<Duration>,
    ) -> Result<String> {
        if let Some(cache) = &self.cache
            && let Ok(cache) = cache.lock()
            && let Ok(Some(token)) = cache.get(&member.name, token_type)
        {
            return Ok(token);
        }

        let token = self.create_token(member, token_type, ttl)?;

        if let Some(cache) = &self.cache
            && let Ok(cache) = cache.lock()
        {
            let _ = cache.put(&member.name, token_type, &token.token, token.expire_at);
        }

        Ok(token.token)
    }

    pub fn create_token(
        &self,
        member: &Member,
        token_type: TokenType,
        ttl: Option<Duration>,
    ) -> Result<Token> {
        let url = format!("https://api.motherduck.com/v1/users/{}/tokens", member.name);
        let token = self
            .agent
            .post(&url)
            .header("Authorization", &self.auth_header())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send_json(serde_json::json!({
                "ttl": ttl.unwrap_or(DEFAULT_TTL).as_secs(),
                "name": format!("{}-{}", USER_AGENT, OffsetDateTime::now_utc().unix_timestamp()),
                "token_type": token_type
            }))
            .context(url)?
            .body_mut()
            .read_json::<Token>()?;
        Ok(token)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(unused)]
pub struct Member {
    pub id: Uuid,
    /// The API calls this `username`; it is the name tokens are minted
    /// against and what `motherduck_user` is set to.
    #[serde(rename = "username")]
    pub name: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub is_service_account: bool,
    pub is_deprovisioned: bool,
    /// Only roles assigned directly; roles inherited via a custom role are omitted.
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct UsersPage {
    users: Vec<Member>,
    next_page_token: Option<String>,
}

/// The API's documented maximum page size.
const USERS_PAGE_LIMIT: u32 = 1000;

#[derive(Serialize, Deserialize, Debug)]
#[allow(unused)]
pub struct Token {
    pub token: String,
    pub id: Uuid,
    pub name: String,
    #[serde(with = "time::serde::iso8601")]
    pub created_ts: OffsetDateTime,
    #[serde(with = "time::serde::iso8601")]
    pub expire_at: OffsetDateTime,
    pub read_only: bool,
    pub token_type: TokenType,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum TokenType {
    #[serde(rename = "read_write")]
    ReadWrite,
    #[serde(rename = "read_scaling")]
    ReadScaling,
}

impl TokenType {
    pub fn from_read_only(read_only: bool) -> Self {
        if read_only {
            TokenType::ReadScaling
        } else {
            TokenType::ReadWrite
        }
    }

    /// Stable string used as the cache key; matches the serde renames.
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenType::ReadWrite => "read_write",
            TokenType::ReadScaling => "read_scaling",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_users_page() {
        let page: UsersPage = serde_json::from_value(serde_json::json!({
            "users": [{
                "id": "6a1f1c1e-1b3e-4b7a-9a43-5d6c2a6c9e10",
                "username": "analytics_service_account",
                "email": "sa@example.invalid",
                "first_name": "Service",
                "last_name": "Account",
                "is_service_account": true,
                "is_deprovisioned": false,
                "roles": ["builder"]
            }],
            "total_count": 1,
            "next_page_token": null
        }))
        .unwrap();

        assert!(page.next_page_token.is_none());
        let [member] = page.users.as_slice() else {
            panic!("expected one user");
        };
        assert_eq!(member.name, "analytics_service_account");
        assert!(member.is_service_account);
        assert_eq!(member.roles, vec!["builder".to_string()]);
    }
}

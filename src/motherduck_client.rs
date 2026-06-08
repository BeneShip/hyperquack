use anyhow::Result;
use bon::Builder;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use suggest::Suggest;
use time::OffsetDateTime;
use ureq::Agent;
use uuid::Uuid;

#[derive(Debug, Builder, Clone)]
#[builder(on(String, into))]
pub struct MotherduckClient {
    pub admin_token: String,
    pub region: String,
    #[builder(default = MotherduckClient::build_agent())]
    agent: Agent,
}

pub static USER_AGENT: &str = "Paccurate HyperQuack";

impl MotherduckClient {
    fn build_agent() -> Agent {
        Agent::config_builder()
            .user_agent(USER_AGENT)
            .build()
            .into()
    }

    fn build_trpc_endpoint(&self, endpoint: &str) -> String {
        format!(
            "https://api.{}.motherduck.com/mom/trpc/{}",
            self.region, endpoint
        )
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.admin_token)
    }

    pub fn get_org_members(&self) -> Result<Vec<Member>> {
        let value: serde_json::Value = self
            .agent
            .get(self.build_trpc_endpoint("org.members"))
            .header("Authorization", &self.auth_header())
            .call()?
            .body_mut()
            .read_json::<serde_json::Value>()?;

        let pointer = "/result/data/orgMembers";
        let members: Vec<Member> = serde_json::from_value(
            value
                .pointer(pointer)
                .ok_or_else(|| anyhow::anyhow!("missing {} in response", pointer))?
                .clone(),
        )?;
        Ok(members)
    }

    pub fn get_service_accounts(&self) -> Result<Vec<Member>> {
        let members = self.get_org_members()?;
        Ok(members
            .into_iter()
            .filter(|m| m.is_service_account)
            .collect())
    }

    pub fn get_member_by_name(&self, name: &str) -> Result<Member> {
        let members: HashMap<String, Member> = self
            .get_org_members()?
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

    pub fn create_token(
        &self,
        member: &Member,
        token_type: TokenType,
        ttl: Option<Duration>,
    ) -> Result<Token> {
        let token = self
            .agent
            .post(&format!(
                "https://api.motherduck.com/v1/users/{}/tokens",
                member.name
            ))
            .header("Authorization", &self.auth_header())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send_json(serde_json::json!({
                "ttl": ttl.unwrap_or_else(|| Duration::from_secs(300)).as_secs(),
                "name": format!("{}-{}", USER_AGENT, OffsetDateTime::now_utc().unix_timestamp()),
                "token_type": token_type
            }))?
            .body_mut()
            .read_json::<Token>()?;
        Ok(token)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(unused)]
pub struct Member {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub org_role: String,
    pub is_service_account: bool,
}

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
}

//! Anthropic: `POST https://api.anthropic.com/v1/oauth/token` with the RFC 7523 `jwt-bearer`
//! grant. The access token goes in `Authorization: Bearer`, not `x-api-key` ([guide],
//! [reference]).
//!
//! [guide]: https://platform.claude.com/docs/en/manage-claude/workload-identity-federation
//! [reference]: https://platform.claude.com/docs/en/manage-claude/wif-reference

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig_core::DynModel;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;
use rig_core::providers::anthropic::wire::{ANTHROPIC, AnthropicConfig, Auth};
use rig_core::providers::registry::ProviderConfig;
use serde::Serialize;
use zeabur_builder_core::refusal::Refusal;

use super::{TokenSource, Vars, required};
use crate::http::Body;
use crate::plan::Reason;

const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";

/// Anthropic's `ANTHROPIC_*` variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Anthropic {
    /// `ANTHROPIC_FEDERATION_RULE_ID`, `fdrl_…`.
    rule: String,
    /// `ANTHROPIC_ORGANIZATION_ID`.
    organization: String,
    /// `ANTHROPIC_WORKSPACE_ID`, `wrkspc_…`, needed when the rule has several workspaces.
    workspace: Option<String>,
    /// `ANTHROPIC_SERVICE_ACCOUNT_ID`, `svac_…`; the rule names one already.
    service_account: Option<String>,
    /// `ANTHROPIC_IDENTITY_TOKEN_FILE`.
    token_file: PathBuf,
}

/// The federation, or `None` when `ANTHROPIC_API_KEY` is set or the rule is not.
pub(super) fn read(var: &Vars<'_>) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    Ok(from(var)?.map(|source| -> Arc<dyn TokenSource> { Arc::new(source) }))
}

fn from(var: &Vars<'_>) -> Result<Option<Anthropic>, Refusal<Reason>> {
    if var("ANTHROPIC_API_KEY").is_some() {
        return Ok(None);
    }
    let Some(rule) = var("ANTHROPIC_FEDERATION_RULE_ID") else {
        return Ok(None);
    };
    Ok(Some(Anthropic {
        rule,
        organization: required(var, "ANTHROPIC_ORGANIZATION_ID")?,
        workspace: var("ANTHROPIC_WORKSPACE_ID"),
        service_account: var("ANTHROPIC_SERVICE_ACCOUNT_ID"),
        token_file: required(var, "ANTHROPIC_IDENTITY_TOKEN_FILE")?.into(),
    }))
}

/// The RFC 7523 request.
#[derive(Debug, Serialize)]
struct JwtBearer<'a> {
    grant_type: &'static str,
    assertion: &'a str,
    federation_rule_id: &'a str,
    organization_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_account_id: Option<&'a str>,
}

impl TokenSource for Anthropic {
    fn name(&self) -> &'static str {
        "Anthropic"
    }

    fn url(&self) -> &str {
        TOKEN_URL
    }

    fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn request(&self, jwt: &str) -> Body {
        Body::json(&JwtBearer {
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
            assertion: jwt,
            federation_rule_id: &self.rule,
            organization_id: &self.organization,
            workspace_id: self.workspace.as_deref(),
            service_account_id: self.service_account.as_deref(),
        })
    }

    fn check(&self) -> &'static str {
        "check the federation rule, its audience and the issuer's keys; the Console's \
         authentication history has the reason"
    }

    fn model(
        &self,
        token: String,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: DynHttpClient,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, Refusal<Reason>> {
        // Rig's Anthropic dialect sends its key as `x-api-key`; an access token is a bearer token.
        let mut dialect = ANTHROPIC;
        dialect.quirks.auth = Auth::OptionalBearer;
        let mut config = ProviderConfig::Anthropic(AnthropicConfig::with_key(&dialect, token));
        if let Some(base_url) = base_url {
            config = config.with_base_url(base_url);
        }
        let facts = catalog.facts(spec.provider, &spec.id);
        Ok(config.completion_model(&spec.id, http, facts))
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::disallowed_types,
        reason = "test fixtures are JSON of any shape"
    )]

    use std::collections::HashMap;

    use pretty_assertions::assert_eq;
    use rig_core::completion::CompletionRequest;
    use rig_core::message::Message;
    use serde_json::{Value, json};

    use super::*;
    use crate::testing::{Replay, text};

    fn anthropic() -> Anthropic {
        let vars = HashMap::from([
            ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_1"),
            ("ANTHROPIC_ORGANIZATION_ID", "org"),
            ("ANTHROPIC_WORKSPACE_ID", "wrkspc_1"),
            (
                "ANTHROPIC_IDENTITY_TOKEN_FILE",
                "/var/run/secrets/anthropic.com/token",
            ),
        ]);
        from(&|name| vars.get(name).map(|v| (*v).to_owned()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn takes_a_jwt_bearer_grant() {
        let body = anthropic().request("jwt");
        assert_eq!(body.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&body.bytes).unwrap(),
            json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
                "assertion": "jwt",
                "federation_rule_id": "fdrl_1",
                "organization_id": "org",
                "workspace_id": "wrkspc_1",
            })
        );
    }

    #[test]
    fn an_api_key_comes_first() {
        let vars = HashMap::from([
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_1"),
        ]);
        assert_eq!(
            from(&|name| vars.get(name).map(|v| (*v).to_owned())),
            Ok(None)
        );
    }

    #[tokio::test]
    async fn sends_the_access_token_as_a_bearer_token() {
        let replay = Replay::new([text("1", "hi")]);
        let catalog = Catalog::builtin();
        let spec = catalog.resolve("anthropic/claude-opus-5-5").unwrap().spec;
        let model = anthropic()
            .model(
                "sk-ant-oat01-abc".to_owned(),
                catalog,
                spec,
                DynHttpClient::new(replay.clone()),
                None,
            )
            .unwrap();

        model
            .call(CompletionRequest::from(vec![Message::user("hi")]))
            .await
            .unwrap();

        let headers = replay.headers.lock().unwrap().clone();
        assert_eq!(headers[0]["authorization"], "Bearer sk-ant-oat01-abc");
        assert!(headers[0].get("x-api-key").is_none());
    }
}

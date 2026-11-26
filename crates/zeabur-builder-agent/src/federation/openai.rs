//! OpenAI: `POST https://auth.openai.com/oauth/token` with the RFC 8693 token exchange grant
//! ([guide], [reference]). The access token takes the API key's place.
//!
//! [guide]: https://developers.openai.com/api/docs/guides/workload-identity-federation
//! [reference]: https://developers.openai.com/api/reference/workload-identity-federation

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig_core::DynModel;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;
use rig_core::providers::registry::ConnectOptions;
use serde::Serialize;
use zeabur_builder_core::refusal::Refusal;

use super::{TokenSource, Vars, required};
use crate::http::Body;
use crate::plan::Reason;

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// OpenAI's `OPENAI_*` variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenAi {
    /// `OPENAI_IDENTITY_PROVIDER_ID`.
    provider: String,
    /// `OPENAI_SERVICE_ACCOUNT_ID`.
    service_account: String,
    /// `OPENAI_IDENTITY_TOKEN_FILE`.
    token_file: PathBuf,
}

/// The federation, or `None` when `OPENAI_API_KEY` is set or the identity provider is not.
pub(super) fn read(var: &Vars<'_>) -> Result<Option<Arc<dyn TokenSource>>, Refusal<Reason>> {
    Ok(from(var)?.map(|source| -> Arc<dyn TokenSource> { Arc::new(source) }))
}

fn from(var: &Vars<'_>) -> Result<Option<OpenAi>, Refusal<Reason>> {
    if var("OPENAI_API_KEY").is_some() {
        return Ok(None);
    }
    let Some(provider) = var("OPENAI_IDENTITY_PROVIDER_ID") else {
        return Ok(None);
    };
    Ok(Some(OpenAi {
        provider,
        service_account: required(var, "OPENAI_SERVICE_ACCOUNT_ID")?,
        token_file: required(var, "OPENAI_IDENTITY_TOKEN_FILE")?.into(),
    }))
}

/// The RFC 8693 request.
#[derive(Debug, Serialize)]
struct TokenExchange<'a> {
    grant_type: &'static str,
    subject_token_type: &'static str,
    subject_token: &'a str,
    identity_provider_id: &'a str,
    service_account_id: &'a str,
}

impl TokenSource for OpenAi {
    fn name(&self) -> &'static str {
        "OpenAI"
    }

    fn url(&self) -> &str {
        TOKEN_URL
    }

    fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn request(&self, jwt: &str) -> Body {
        Body::json(&TokenExchange {
            grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
            subject_token_type: "urn:ietf:params:oauth:token-type:jwt",
            subject_token: jwt,
            identity_provider_id: &self.provider,
            service_account_id: &self.service_account,
        })
    }

    fn check(&self) -> &'static str {
        "check the identity provider, its audience, its uploaded keys and the service account's \
         mapping"
    }

    fn model(
        &self,
        token: String,
        catalog: &Catalog,
        spec: &ModelSpec,
        http: DynHttpClient,
        base_url: Option<&str>,
    ) -> Result<DynModel<Completion>, Refusal<Reason>> {
        let mut options = ConnectOptions::new().http(http).api_key(token);
        if let Some(base_url) = base_url {
            options = options.base_url(base_url);
        }
        catalog
            .connect_with(spec, options)
            .map_err(|e| Refusal::new(Reason::ModelFailed, e.to_string()))
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
    use serde_json::{Value, json};

    use super::*;

    fn from_vars(vars: &[(&str, &str)]) -> Result<Option<OpenAi>, Refusal<Reason>> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        from(&|name| vars.get(name).map(|v| (*v).to_owned()))
    }

    #[test]
    fn takes_a_token_exchange() {
        let openai = from_vars(&[
            ("OPENAI_IDENTITY_PROVIDER_ID", "idp_1"),
            ("OPENAI_SERVICE_ACCOUNT_ID", "svc_1"),
            (
                "OPENAI_IDENTITY_TOKEN_FILE",
                "/var/run/secrets/tokens/token",
            ),
        ])
        .unwrap()
        .unwrap();
        let body = openai.request("jwt");
        assert_eq!(body.content_type, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&body.bytes).unwrap(),
            json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
                "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
                "subject_token": "jwt",
                "identity_provider_id": "idp_1",
                "service_account_id": "svc_1",
            })
        );
    }

    #[test]
    fn a_missing_variable_is_named() {
        assert_eq!(
            from_vars(&[
                ("OPENAI_IDENTITY_PROVIDER_ID", "idp_1"),
                ("OPENAI_SERVICE_ACCOUNT_ID", "svc_1"),
            ]),
            Err(Refusal::new(
                Reason::ModelKeyMissing,
                "set OPENAI_IDENTITY_TOKEN_FILE"
            ))
        );
        assert_eq!(from_vars(&[]), Ok(None));
    }
}

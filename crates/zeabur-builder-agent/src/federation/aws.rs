//! Amazon Bedrock: the AWS SDK assumes an IAM role with the service account token
//! (`AssumeRoleWithWebIdentity`, [OIDC providers]) and signs each Converse request with the
//! temporary credentials, renewing them before they expire. AWS gives no single access token, so
//! this is not a [`TokenSource`](super::TokenSource).
//!
//! [OIDC providers]: https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_providers_oidc.html

use aws_config::BehaviorVersion;
use aws_config::provider_config::ProviderConfig;
use aws_config::web_identity_token::{StaticConfiguration, WebIdentityTokenCredentialsProvider};
use aws_sdk_bedrockruntime::config::{
    ProvideCredentials as _, Region, SharedCredentialsProvider, SharedHttpClient,
};
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use aws_smithy_http_client::tls::{self};
use aws_smithy_types::error::display::DisplayErrorContext;
use rig_bedrock::client::BedrockRuntime;
use rig_core::DynModel;
use rig_core::operation::Completion;
use zeabur_builder_core::refusal::Refusal;

use super::{Vars, required};
use crate::plan::Reason;

/// Rig's vendor name for Bedrock.
pub(crate) const VENDOR: &str = "aws_bedrock";

/// The session name in the role's `CloudTrail` events.
const SESSION: &str = "zeabur-builder-agent";

/// The AWS SDK's variables for a role assumed with a web identity token.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Role {
    /// `AWS_ROLE_ARN`.
    arn: String,
    /// `AWS_WEB_IDENTITY_TOKEN_FILE`.
    token_file: String,
    /// `AWS_REGION`, of STS and Bedrock.
    region: String,
}

impl Role {
    fn from(var: &Vars<'_>) -> Result<Self, Refusal<Reason>> {
        Ok(Self {
            arn: required(var, "AWS_ROLE_ARN")?,
            token_file: required(var, "AWS_WEB_IDENTITY_TOKEN_FILE")?,
            region: required(var, "AWS_REGION")?,
        })
    }
}

/// `model` on Bedrock with the role in `AWS_ROLE_ARN`, from the process environment.
pub(crate) async fn connect(
    model: &str,
    base_url: Option<&str>,
) -> Result<DynModel<Completion>, Refusal<Reason>> {
    let role = Role::from(&|name| std::env::var(name).ok().filter(|value| !value.is_empty()))?;
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(CryptoMode::AwsLc))
        .build_https();
    connect_as(role, model, base_url, http).await
}

/// `model` on Bedrock as `role`, sending to STS and Bedrock through `http`. The role is assumed
/// once here, so a refused token stops the run before the agent starts; the SDK assumes it again
/// for its own credentials cache on the first call.
async fn connect_as(
    role: Role,
    model: &str,
    base_url: Option<&str>,
    http: SharedHttpClient,
) -> Result<DynModel<Completion>, Refusal<Reason>> {
    let region = Region::new(role.region);
    let credentials = WebIdentityTokenCredentialsProvider::builder()
        .configure(
            &ProviderConfig::without_region()
                .with_region(Some(region.clone()))
                .with_http_client(http.clone()),
        )
        .static_configuration(StaticConfiguration {
            web_identity_token_file: role.token_file.into(),
            role_arn: role.arn,
            session_name: SESSION.to_owned(),
        })
        .build();
    credentials.provide_credentials().await.map_err(|e| {
        Refusal::new(
            Reason::ModelKeyMissing,
            format!(
                "AWS STS refused the service account token: {}; check the IAM OIDC identity \
                 provider and the role's trust policy, and that the issuer's discovery document \
                 is reachable from the internet",
                DisplayErrorContext(&e)
            ),
        )
    })?;
    let mut config = aws_sdk_bedrockruntime::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(region)
        .credentials_provider(SharedCredentialsProvider::new(credentials))
        .http_client(http);
    if let Some(base_url) = base_url {
        config = config.endpoint_url(base_url);
    }
    let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
    Ok(BedrockRuntime::from(client).completion(model).erase())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use aws_smithy_runtime_api::client::http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    };
    use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
    use aws_smithy_types::body::SdkBody;
    use pretty_assertions::assert_eq;
    use rig_core::completion::CompletionRequest;
    use rig_core::message::Message;

    use super::*;

    /// A request the fake AWS got.
    #[derive(Debug, Clone)]
    struct Seen {
        uri: String,
        authorization: Option<String>,
        security_token: Option<String>,
        body: String,
    }

    /// STS and Bedrock with no network: STS answers `sts` with `sts_status`, Bedrock a Converse
    /// reply.
    #[derive(Debug, Clone)]
    struct FakeAws {
        sts_status: u16,
        sts: &'static str,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl HttpConnector for FakeAws {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            let header = |name: &str| request.headers().get(name).map(str::to_owned);
            let seen = Seen {
                uri: request.uri().to_owned(),
                authorization: header("authorization"),
                security_token: header("x-amz-security-token"),
                body: request
                    .body()
                    .bytes()
                    .map_or_else(String::new, |b| String::from_utf8_lossy(b).into_owned()),
            };
            let (status, reply) = if seen.uri.starts_with("https://sts.") {
                (self.sts_status, self.sts)
            } else {
                (200, CONVERSE_REPLY)
            };
            self.seen.lock().unwrap().push(seen);
            let response = HttpResponse::new(status.try_into().unwrap(), SdkBody::from(reply));
            HttpConnectorFuture::ready(Ok(response))
        }
    }

    impl HttpClient for FakeAws {
        fn http_connector(
            &self,
            _: &HttpConnectorSettings,
            _: &RuntimeComponents,
        ) -> SharedHttpConnector {
            SharedHttpConnector::new(self.clone())
        }
    }

    const STS_REPLY: &str = r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>ASIAFAKE</AccessKeyId>
      <SecretAccessKey>secret</SecretAccessKey>
      <SessionToken>session-token</SessionToken>
      <Expiration>2099-01-01T00:00:00Z</Expiration>
    </Credentials>
    <AssumedRoleUser>
      <Arn>arn:aws:sts::123456789012:assumed-role/builder/zeabur-builder-agent</Arn>
      <AssumedRoleId>AROAFAKE:zeabur-builder-agent</AssumedRoleId>
    </AssumedRoleUser>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#;

    const CONVERSE_REPLY: &str = r#"{"output":{"message":{"role":"assistant","content":[{"text":"hello"}]}},"stopReason":"end_turn","usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2},"metrics":{"latencyMs":1}}"#;

    async fn connect_to(aws: &FakeAws) -> Result<DynModel<Completion>, Refusal<Reason>> {
        let role = Role {
            arn: "arn:aws:iam::123456789012:role/builder".to_owned(),
            token_file: concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/token").to_owned(),
            region: "us-east-1".to_owned(),
        };
        connect_as(
            role,
            "anthropic.claude-opus-5-5",
            None,
            SharedHttpClient::new(aws.clone()),
        )
        .await
    }

    fn fake(sts_status: u16, sts: &'static str) -> FakeAws {
        FakeAws {
            sts_status,
            sts,
            seen: Arc::default(),
        }
    }

    #[tokio::test]
    async fn the_role_credentials_sign_the_converse_request() {
        let aws = fake(200, STS_REPLY);

        let model = connect_to(&aws).await.unwrap();
        model
            .call(CompletionRequest::from(vec![Message::user("hi")]))
            .await
            .unwrap();

        let seen = aws.seen.lock().unwrap().clone();
        let (Some(role), Some(converse)) = (seen.first(), seen.last()) else {
            panic!("{seen:?}");
        };
        assert_eq!(role.uri, "https://sts.us-east-1.amazonaws.com/");
        for part in [
            "Action=AssumeRoleWithWebIdentity",
            "RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fbuilder",
            "RoleSessionName=zeabur-builder-agent",
            "WebIdentityToken=jwt",
        ] {
            assert!(role.body.contains(part), "{part} in {}", role.body);
        }
        assert_eq!(role.authorization, None);
        assert_eq!(
            converse.uri,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-opus-5-5/converse"
        );
        let authorization = converse.authorization.as_deref().unwrap();
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256 Credential=ASIAFAKE/")
                && authorization.contains("/us-east-1/bedrock/aws4_request"),
            "{authorization}"
        );
        assert_eq!(converse.security_token.as_deref(), Some("session-token"));
    }

    #[tokio::test]
    async fn a_refused_token_says_what_to_check() {
        let aws = fake(
            400,
            r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><Error><Type>Sender</Type><Code>InvalidIdentityToken</Code><Message>No OpenIDConnect provider found in your account</Message></Error><RequestId>1</RequestId></ErrorResponse>"#,
        );

        let Err(Refusal { reason, message }) = connect_to(&aws).await else {
            panic!("connected");
        };

        assert_eq!(reason, Reason::ModelKeyMissing);
        assert!(message.contains("InvalidIdentityToken"), "{message}");
        assert!(message.contains("trust policy"), "{message}");
    }

    #[test]
    fn a_missing_variable_is_named() {
        let vars: HashMap<_, _> = [
            ("AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/builder"),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "/var/run/secrets/token"),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            Role::from(&|name| vars.get(name).map(|v| (*v).to_owned())),
            Err(Refusal::new(Reason::ModelKeyMissing, "set AWS_REGION"))
        );
    }
}

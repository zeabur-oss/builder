//! Git credentials from the Secret a `Build`'s `credentialsRef` names.
//!
//! The Secret must carry the label `builder.zeabur.com/git-credentials: "true"`, so a `Build`
//! cannot point the controller at any Secret in its namespace.

use std::fmt;

use k8s_openapi::api::core::v1::Secret;
use zeabur_builder_api::build::ConditionReason;
use zeabur_builder_api::labels;
use zeabur_builder_jobspec::GitAuth;

const BASIC_AUTH: &str = "kubernetes.io/basic-auth";
const SSH_AUTH: &str = "kubernetes.io/ssh-auth";

/// What the Secret holds.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GitCredentials {
    /// `kubernetes.io/basic-auth`: `username` and `password`, for `https://` URLs.
    Basic { username: String, password: String },
    /// `kubernetes.io/ssh-auth`: `ssh-privatekey` and `known_hosts`, for SSH URLs.
    Ssh {
        private_key: String,
        known_hosts: String,
    },
}

impl fmt::Debug for GitCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .finish_non_exhaustive(),
            Self::Ssh { .. } => f.debug_struct("Ssh").finish_non_exhaustive(),
        }
    }
}

/// Why a Secret gives no credentials: the `SourceReady` reason and message.
pub(crate) type Refusal = (ConditionReason, String);

impl GitCredentials {
    /// Reads `secret`, named `name`.
    pub(crate) fn from_secret(name: &str, secret: &Secret) -> Result<Self, Refusal> {
        let labelled = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(labels::GIT_CREDENTIALS))
            .is_some_and(|v| v == "true");
        if !labelled {
            return Err((
                ConditionReason::CredentialsMissing,
                format!(
                    "Secret `{name}` needs the label `{}: \"true\"`",
                    labels::GIT_CREDENTIALS
                ),
            ));
        }
        let invalid = |message: String| (ConditionReason::CredentialsInvalid, message);
        let data = secret.data.as_ref();
        let key = |key: &str| {
            data.and_then(|d| d.get(key))
                .and_then(|v| String::from_utf8(v.0.clone()).ok())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid(format!("Secret `{name}` has no `{key}`")))
        };
        match secret.type_.as_deref() {
            Some(BASIC_AUTH) => Ok(Self::Basic {
                username: key("username")?,
                password: key("password")?,
            }),
            Some(SSH_AUTH) => Ok(Self::Ssh {
                private_key: key("ssh-privatekey")?,
                known_hosts: key("known_hosts")?,
            }),
            other => Err(invalid(format!(
                "Secret `{name}` is of type `{}`; use {BASIC_AUTH} or {SSH_AUTH}",
                other.unwrap_or("Opaque")
            ))),
        }
    }

    /// How the Job's `fetch` uses the same Secret.
    pub(crate) const fn job_auth<'a>(&self, secret: &'a str) -> GitAuth<'a> {
        match self {
            Self::Basic { .. } => GitAuth::Basic { secret },
            Self::Ssh { .. } => GitAuth::Ssh { secret },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::ByteString;
    use kube::api::ObjectMeta;

    use super::*;

    fn secret(label: Option<&str>, type_: &str, data: &[(&str, &str)]) -> Secret {
        Secret {
            metadata: ObjectMeta {
                labels: label
                    .map(|v| BTreeMap::from([(labels::GIT_CREDENTIALS.to_owned(), v.to_owned())])),
                ..ObjectMeta::default()
            },
            type_: Some(type_.to_owned()),
            data: Some(
                data.iter()
                    .map(|(k, v)| ((*k).to_owned(), ByteString(v.as_bytes().to_vec())))
                    .collect(),
            ),
            ..Secret::default()
        }
    }

    #[test]
    fn reads_both_types() {
        let basic = secret(
            Some("true"),
            BASIC_AUTH,
            &[("username", "x-access-token"), ("password", "ghs_x")],
        );
        assert_eq!(
            GitCredentials::from_secret("git", &basic),
            Ok(GitCredentials::Basic {
                username: "x-access-token".to_owned(),
                password: "ghs_x".to_owned(),
            })
        );
        let ssh = secret(
            Some("true"),
            SSH_AUTH,
            &[
                ("ssh-privatekey", "KEY"),
                ("known_hosts", "github.com ssh-ed25519 AAAA"),
            ],
        );
        assert_eq!(
            GitCredentials::from_secret("git", &ssh).map(|c| c.job_auth("git")),
            Ok(GitAuth::Ssh { secret: "git" })
        );
    }

    #[test]
    fn refuses() {
        let reason = |s: &Secret| GitCredentials::from_secret("git", s).map_err(|(r, _)| r);
        let data = [("username", "u"), ("password", "p")];
        assert_eq!(
            reason(&secret(None, BASIC_AUTH, &data)),
            Err(ConditionReason::CredentialsMissing)
        );
        assert_eq!(
            reason(&secret(Some("false"), BASIC_AUTH, &data)),
            Err(ConditionReason::CredentialsMissing)
        );
        assert_eq!(
            reason(&secret(Some("true"), "Opaque", &data)),
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason(&secret(
                Some("true"),
                SSH_AUTH,
                &[("ssh-privatekey", "KEY")]
            )),
            Err(ConditionReason::CredentialsInvalid)
        );
        assert_eq!(
            reason(&secret(
                Some("true"),
                BASIC_AUTH,
                &[("username", "u"), ("password", "")]
            )),
            Err(ConditionReason::CredentialsInvalid)
        );
    }

    #[test]
    fn debug_hides_secrets() {
        let basic = GitCredentials::Basic {
            username: "u".to_owned(),
            password: "hunter2".to_owned(),
        };
        let ssh = GitCredentials::Ssh {
            private_key: "PRIVATE".to_owned(),
            known_hosts: String::new(),
        };
        assert!(!format!("{basic:?}").contains("hunter2"));
        assert!(!format!("{ssh:?}").contains("PRIVATE"));
    }
}

//! What a webhook delivery names: a repository and the refs a push moved.
//!
//! A delivery is only a wake-up call. Nothing else is read from it: no commit,
//! no URL to fetch from. The controller takes the [`RepoKey`]s to find the `Build`s that follow
//! the repository, checks the signature with each one's secret, and has those whose ref was
//! pushed ask the Git server again. A forged or replayed delivery can at most cause one more
//! poll, and the poll answers from the server.
//!
//! Payloads, per provider:
//!
//! - GitHub ([push]): `ref`, `deleted`, `after` and `repository.{clone_url,ssh_url,html_url}`;
//!   `ping` when a webhook is created.
//! - GitLab ([GitLab webhooks]): `X-Gitlab-Event: Push Hook` or `Tag Push Hook`, `ref`, `after` and
//!   `project.{git_http_url,git_ssh_url,web_url}`.
//! - Gitea and Forgejo ([Gitea webhooks], [Forgejo webhooks]): `X-Gitea-Event: push`, with the same
//!   `ref`, `after` and `repository` fields as GitHub's.
//! - Bitbucket Cloud ([Bitbucket payloads]): `X-Event-Key: repo:push`, `repository.full_name`,
//!   `repository.links.html.href` and `push.changes[].new.{type,name}`, where `new` is `null` for a
//!   deleted ref. Bitbucket Data Center sends another payload and is not supported; use the generic
//!   form.
//! - Generic ([Standard Webhooks]): `{"repository": "<url>", "ref": "refs/heads/main"}`.
//!
//! [push]: https://docs.github.com/en/webhooks/webhook-events-and-payloads#push
//! [GitLab webhooks]: https://docs.gitlab.com/user/project/integrations/webhooks/
//! [Gitea webhooks]: https://docs.gitea.com/usage/webhooks
//! [Forgejo webhooks]: https://forgejo.org/docs/latest/user/webhooks/
//! [Bitbucket payloads]: https://support.atlassian.com/bitbucket-cloud/docs/event-payloads/
//! [Standard Webhooks]: https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md

use serde_json::Value;
use thiserror::Error;

use crate::git::{DEFAULT_BRANCH, candidates};
use crate::repo::RepoKey;

/// Most refs read from one delivery; Bitbucket lists one change per ref.
pub const MAX_REFS: usize = 32;
/// Longest ref read from a delivery, `refs/heads/` included.
pub const MAX_REF_LEN: usize = 1024;

/// Who sent a delivery, from the path it arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// A repository webhook on GitHub or GitHub Enterprise Server.
    GitHub,
    /// The GitHub App's webhook, signed with the App's secret instead of a `Build`'s.
    GitHubApp,
    /// GitLab.
    GitLab,
    /// Gitea and Forgejo.
    Gitea,
    /// Bitbucket Cloud.
    Bitbucket,
    /// Any sender that signs as Standard Webhooks describes.
    Generic,
}

impl Provider {
    /// The provider served at `/hooks/<segment>`.
    #[must_use]
    pub fn from_segment(segment: &str) -> Option<Self> {
        Some(match segment {
            "github" => Self::GitHub,
            "github-app" => Self::GitHubApp,
            "gitlab" => Self::GitLab,
            "gitea" | "forgejo" => Self::Gitea,
            "bitbucket" => Self::Bitbucket,
            "generic" => Self::Generic,
            _ => return None,
        })
    }

    /// The header naming the event, if the provider sends one.
    #[must_use]
    pub const fn event_header(self) -> Option<&'static str> {
        match self {
            Self::GitHub | Self::GitHubApp => Some("x-github-event"),
            Self::GitLab => Some("x-gitlab-event"),
            Self::Gitea => Some("x-gitea-event"),
            Self::Bitbucket => Some("x-event-key"),
            Self::Generic => None,
        }
    }

    /// The header with the delivery's unique id, used to drop a delivery seen before.
    #[must_use]
    pub const fn delivery_header(self) -> &'static str {
        match self {
            Self::GitHub | Self::GitHubApp => "x-github-delivery",
            Self::GitLab => "x-gitlab-event-uuid",
            Self::Gitea => "x-gitea-delivery",
            Self::Bitbucket => "x-request-uuid",
            Self::Generic => "webhook-id",
        }
    }
}

/// The kind of event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Refs moved.
    Push,
    /// The sender checks that the webhook works.
    Ping,
    /// Anything else, accepted and ignored. Pull requests will get their own variant.
    Other,
}

/// What a delivery names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The event.
    pub event: Event,
    /// Every spelling of the repository the payload gives, as keys, without duplicates.
    pub repositories: Vec<RepoKey>,
    /// The full names of the refs a push moved, such as `refs/heads/main`. Deleted refs are left
    /// out.
    pub refs: Vec<String>,
}

/// Why a delivery is not understood.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HookError {
    /// The body is not JSON.
    #[error("the body is not JSON")]
    Json,
    /// A push without a ref the controller can read.
    #[error("the push names no valid ref")]
    Ref,
}

/// Reads `body`, sent by `provider` with `event` in its event header.
///
/// # Errors
///
/// When the body is not JSON, or a push names a ref that is not a ref.
pub fn parse(provider: Provider, event: Option<&str>, body: &[u8]) -> Result<Delivery, HookError> {
    let json: Value = serde_json::from_slice(body).map_err(|_error| HookError::Json)?;
    let kind = match (provider, event.unwrap_or_default()) {
        (Provider::GitHub | Provider::GitHubApp | Provider::Gitea, "push")
        | (Provider::GitLab, "Push Hook" | "Tag Push Hook")
        | (Provider::Bitbucket, "repo:push")
        | (Provider::Generic, _) => Event::Push,
        (Provider::GitHub | Provider::GitHubApp, "ping") => Event::Ping,
        _ => Event::Other,
    };
    let text = |pointer: &str| json.pointer(pointer).and_then(Value::as_str);
    let urls: Vec<String> = match provider {
        Provider::GitHub | Provider::GitHubApp | Provider::Gitea => [
            "/repository/clone_url",
            "/repository/ssh_url",
            "/repository/html_url",
        ]
        .iter()
        .filter_map(|p| text(p).map(str::to_owned))
        .collect(),
        Provider::GitLab => [
            "/project/git_http_url",
            "/project/git_ssh_url",
            "/project/web_url",
        ]
        .iter()
        .filter_map(|p| text(p).map(str::to_owned))
        .collect(),
        Provider::Bitbucket => text("/repository/links/html/href")
            .map(str::to_owned)
            .into_iter()
            .chain(text("/repository/full_name").map(|n| format!("https://bitbucket.org/{n}")))
            .collect(),
        Provider::Generic => text("/repository").map(str::to_owned).into_iter().collect(),
    };
    let repositories = urls.iter().filter_map(|u| RepoKey::parse(u).ok()).fold(
        Vec::<RepoKey>::new(),
        |mut keys, key| {
            if !keys.contains(&key) {
                keys.push(key);
            }
            keys
        },
    );
    let refs = if kind == Event::Push {
        pushed_refs(provider, &json)?
    } else {
        Vec::new()
    };
    Ok(Delivery {
        event: kind,
        repositories,
        refs,
    })
}

fn pushed_refs(provider: Provider, json: &Value) -> Result<Vec<String>, HookError> {
    let refs: Vec<String> = match provider {
        Provider::Bitbucket => json
            .pointer("/push/changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_REFS)
            .filter_map(|change| {
                let new = change.get("new").filter(|n| !n.is_null())?;
                let name = new.get("name")?.as_str()?;
                let prefix = match new.get("type")?.as_str()? {
                    "branch" => "refs/heads/",
                    "tag" => "refs/tags/",
                    _ => return None,
                };
                Some(format!("{prefix}{name}"))
            })
            .collect(),
        Provider::Generic => {
            let name = json
                .get("ref")
                .and_then(Value::as_str)
                .ok_or(HookError::Ref)?;
            vec![if name.starts_with("refs/") {
                name.to_owned()
            } else {
                format!("refs/heads/{name}")
            }]
        }
        Provider::GitHub | Provider::GitHubApp | Provider::GitLab | Provider::Gitea => {
            let name = json
                .get("ref")
                .and_then(Value::as_str)
                .ok_or(HookError::Ref)?;
            let deleted = json.get("deleted").and_then(Value::as_bool) == Some(true)
                || json
                    .get("after")
                    .and_then(Value::as_str)
                    .is_some_and(|a| !a.is_empty() && a.bytes().all(|b| b == b'0'));
            if deleted {
                Vec::new()
            } else {
                vec![name.to_owned()]
            }
        }
    };
    if refs.iter().all(|r| valid_ref(r)) {
        Ok(refs)
    } else {
        Err(HookError::Ref)
    }
}

fn valid_ref(name: &str) -> bool {
    name.len() <= MAX_REF_LEN
        && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
        && !name.bytes().any(|b| b.is_ascii_control() || b == b' ')
}

/// Whether a push of `pushed` concerns a `Build` that follows `followed`. `HEAD` follows the
/// default branch, which is `resolved` once the controller has asked; before that every push
/// counts.
#[must_use]
pub fn ref_matches(followed: &str, resolved: Option<&str>, pushed: &str) -> bool {
    if followed == DEFAULT_BRANCH {
        return resolved.is_none_or(|r| r == pushed);
    }
    candidates(followed).iter().any(|c| c == pushed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> RepoKey {
        RepoKey::parse(url).unwrap()
    }

    #[test]
    fn github_push() {
        let body = br#"{
            "ref": "refs/heads/main",
            "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
            "deleted": false,
            "repository": {
                "clone_url": "https://github.com/Acme/API.git",
                "ssh_url": "git@github.com:Acme/API.git",
                "html_url": "https://github.com/Acme/API"
            }
        }"#;
        let delivery = parse(Provider::GitHub, Some("push"), body).unwrap();
        assert_eq!(delivery.event, Event::Push);
        assert_eq!(delivery.repositories, [key("https://github.com/acme/api")]);
        assert_eq!(delivery.refs, ["refs/heads/main"]);
    }

    #[test]
    fn deleted_refs_wake_nothing() {
        let deleted = br#"{"ref": "refs/heads/x", "deleted": true, "after": "0000000000000000000000000000000000000000",
            "repository": {"clone_url": "https://github.com/acme/api.git"}}"#;
        assert_eq!(
            parse(Provider::GitHub, Some("push"), deleted).unwrap().refs,
            Vec::<String>::new()
        );
        let gitlab =
            br#"{"ref": "refs/heads/x", "after": "0000000000000000000000000000000000000000",
            "project": {"git_http_url": "https://gitlab.com/acme/api.git"}}"#;
        assert_eq!(
            parse(Provider::GitLab, Some("Push Hook"), gitlab)
                .unwrap()
                .refs,
            Vec::<String>::new()
        );
    }

    #[test]
    fn gitlab_gitea_and_generic() {
        let gitlab =
            br#"{"ref": "refs/tags/v1.0.0", "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
            "project": {"git_http_url": "https://gitlab.example.com/Group/Sub/App.git",
                        "git_ssh_url": "git@gitlab.example.com:Group/Sub/App.git",
                        "web_url": "https://gitlab.example.com/Group/Sub/App"}}"#;
        let d = parse(Provider::GitLab, Some("Tag Push Hook"), gitlab).unwrap();
        assert_eq!(d.event, Event::Push);
        assert_eq!(
            d.repositories,
            [key("https://gitlab.example.com/Group/Sub/App")]
        );
        assert_eq!(d.refs, ["refs/tags/v1.0.0"]);

        let gitea =
            br#"{"ref": "refs/heads/main", "after": "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c",
            "repository": {"clone_url": "http://localhost:3000/forgejo/webhooks.git",
                           "ssh_url": "ssh://forgejo@localhost:2222/forgejo/webhooks.git"}}"#;
        let d = parse(Provider::Gitea, Some("push"), gitea).unwrap();
        assert_eq!(
            d.repositories,
            [
                key("http://localhost:3000/forgejo/webhooks"),
                key("ssh://localhost:2222/forgejo/webhooks")
            ]
        );

        let generic = br#"{"repository": "https://git.example.com/team/app.git", "ref": "main"}"#;
        let d = parse(Provider::Generic, None, generic).unwrap();
        assert_eq!(d.refs, ["refs/heads/main"]);
        assert_eq!(d.repositories, [key("https://git.example.com/team/app")]);
    }

    #[test]
    fn bitbucket_changes() {
        let body = br#"{"repository": {"full_name": "team/app",
                "links": {"html": {"href": "https://bitbucket.org/team/app"}}},
            "push": {"changes": [
                {"new": {"type": "branch", "name": "main"}},
                {"new": null, "old": {"type": "branch", "name": "gone"}},
                {"new": {"type": "tag", "name": "v2"}}
            ]}}"#;
        let d = parse(Provider::Bitbucket, Some("repo:push"), body).unwrap();
        assert_eq!(d.repositories, [key("https://bitbucket.org/team/app")]);
        assert_eq!(d.refs, ["refs/heads/main", "refs/tags/v2"]);
    }

    #[test]
    fn other_events_name_no_refs() {
        let body = br#"{"zen": "Keep it logically awesome.", "hook_id": 1,
            "repository": {"clone_url": "https://github.com/acme/api.git"}}"#;
        let d = parse(Provider::GitHub, Some("ping"), body).unwrap();
        assert_eq!(d.event, Event::Ping);
        assert_eq!(d.refs, Vec::<String>::new());
        let d = parse(Provider::GitHub, Some("pull_request"), body).unwrap();
        assert_eq!(d.event, Event::Other);
        assert_eq!(
            parse(Provider::Gitea, None, body).unwrap().event,
            Event::Other
        );
    }

    #[test]
    fn hostile_payloads() {
        assert_eq!(
            parse(Provider::GitHub, Some("push"), b"not json"),
            Err(HookError::Json)
        );
        for body in [
            &br#"{"repository": {"clone_url": "https://github.com/a/b"}}"#[..],
            br#"{"ref": "main", "repository": {"clone_url": "https://github.com/a/b"}}"#,
            br#"{"ref": "refs/heads/a b", "repository": {"clone_url": "https://github.com/a/b"}}"#,
            br#"{"ref": "refs/remotes/x", "repository": {"clone_url": "https://github.com/a/b"}}"#,
            br#"{"ref": 7, "repository": {"clone_url": "https://github.com/a/b"}}"#,
        ] {
            assert_eq!(
                parse(Provider::GitHub, Some("push"), body),
                Err(HookError::Ref),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let local = br#"{"ref": "refs/heads/main",
            "repository": {"clone_url": "file:///etc", "ssh_url": "ext::sh -c id"}}"#;
        assert_eq!(
            parse(Provider::GitHub, Some("push"), local)
                .unwrap()
                .repositories,
            Vec::<RepoKey>::new()
        );
        let many = format!(
            r#"{{"repository": {{"full_name": "t/a"}}, "push": {{"changes": [{}]}}}}"#,
            vec![r#"{"new": {"type": "branch", "name": "x"}}"#; 100].join(",")
        );
        assert_eq!(
            parse(Provider::Bitbucket, Some("repo:push"), many.as_bytes())
                .unwrap()
                .refs
                .len(),
            MAX_REFS
        );
    }

    #[test]
    fn refs_match() {
        assert!(ref_matches("main", None, "refs/heads/main"));
        assert!(ref_matches("v1", None, "refs/tags/v1"));
        assert!(ref_matches("refs/heads/main", None, "refs/heads/main"));
        assert!(!ref_matches("main", None, "refs/heads/main2"));
        assert!(!ref_matches("main", None, "refs/heads/feature/main"));
        assert!(!ref_matches("refs/heads/main", None, "refs/tags/main"));
        assert!(ref_matches(
            "HEAD",
            Some("refs/heads/master"),
            "refs/heads/master"
        ));
        assert!(!ref_matches(
            "HEAD",
            Some("refs/heads/master"),
            "refs/heads/dev"
        ));
        assert!(ref_matches("HEAD", None, "refs/heads/dev"));
    }

    #[test]
    fn segments() {
        assert_eq!(Provider::from_segment("forgejo"), Some(Provider::Gitea));
        assert_eq!(Provider::from_segment("GitHub"), None);
        assert_eq!(Provider::from_segment(""), None);
    }
}

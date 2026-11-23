//! The controller's writes of whole objects. Runs that write one object at once must agree on the
//! result, so each write is one of three: server-side apply, a create that keeps the object made
//! first, or a replace at the `resourceVersion` the caller read. Clippy allows `Api::create` and
//! `Api::replace` only here (`clippy.toml`).

#![expect(
    clippy::disallowed_methods,
    reason = "the one module that calls Api::create and Api::replace"
)]

use std::fmt::Debug;

use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::{Resource, ResourceExt as _};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The field manager the controller writes as.
pub(crate) const MANAGER: &str = "zeabur-builder";

/// Writes `object` with server-side apply as [`MANAGER`], taking over fields another manager set.
/// Runs that write the same object at once end with the same object, whichever goes last.
pub(crate) async fn apply<K>(api: &Api<K>, object: &K) -> Result<K, kube::Error>
where
    K: Resource + Clone + Serialize + DeserializeOwned + Debug + Sync,
{
    api.patch(
        &object.name_any(),
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(object),
    )
    .await
}

/// The result of [`create_once`].
#[derive(Debug)]
pub(crate) enum Created<K> {
    /// This call made the object.
    New(K),
    /// An object of that name was there already, and is kept as it is.
    Exists,
}

/// Creates `object` unless one of its name exists. Of runs that create it at once, one makes it
/// and the others get [`Created::Exists`].
pub(crate) async fn create_once<K>(api: &Api<K>, object: &K) -> Result<Created<K>, kube::Error>
where
    K: Resource + Clone + Serialize + DeserializeOwned + Debug + Sync,
{
    match api.create(&PostParams::default(), object).await {
        Ok(created) => Ok(Created::New(created)),
        Err(kube::Error::Api(status)) if status.is_already_exists() => Ok(Created::Exists),
        Err(e) => Err(e),
    }
}

/// Replaces the object with `object`, which carries the `resourceVersion` the caller read. The API
/// server refuses it with a conflict when the object changed since that read.
pub(crate) async fn replace_at<K>(api: &Api<K>, object: &K) -> Result<K, kube::Error>
where
    K: Resource + Clone + Serialize + DeserializeOwned + Debug + Sync,
{
    if object.meta().resource_version.is_none() {
        return Err(kube::Error::Service(
            format!(
                "replacing `{}` needs the resourceVersion read",
                object.name_any()
            )
            .into(),
        ));
    }
    api.replace(&object.name_any(), &PostParams::default(), object)
        .await
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use http::{Method, Request, Response};
    use http_body_util::{BodyExt as _, Full};
    use k8s_openapi::api::core::v1::Secret;
    use kube::Client;
    use kube::api::ObjectMeta;

    use super::*;

    /// A request the fake API server got: its method and body.
    type Got = Arc<Mutex<Vec<(Method, Secret)>>>;

    /// An API server that answers every request with `code` and `body`, and keeps the requests.
    fn server(code: u16, body: &str) -> (Api<Secret>, Got) {
        let body = Bytes::from(body.to_owned());
        let got = Got::default();
        let kept = Arc::clone(&got);
        let service = tower::service_fn(move |request: Request<kube::client::Body>| {
            let (kept, body) = (Arc::clone(&kept), body.clone());
            async move {
                let method = request.method().clone();
                let sent = request.into_body().collect().await.unwrap().to_bytes();
                kept.lock()
                    .unwrap()
                    .push((method, serde_json::from_slice(&sent).unwrap()));
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Full::new(body))
                        .unwrap(),
                )
            }
        });
        (
            Api::namespaced(Client::new(service, "default"), "team-a"),
            got,
        )
    }

    const CREATED: &str = r#"{"apiVersion":"v1","kind":"Secret","metadata":{"name":"web-upload"}}"#;

    fn conflict(reason: &str) -> String {
        format!(
            r#"{{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"{reason}","code":409,"message":"{reason}"}}"#
        )
    }

    fn secret(version: Option<&str>) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some("web-upload".to_owned()),
                resource_version: version.map(str::to_owned),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        }
    }

    #[tokio::test]
    async fn a_replace_sends_the_version_read_and_fails_when_it_changed() {
        let (api, got) = server(409, &conflict("Conflict"));
        let error = replace_at(&api, &secret(Some("7"))).await.unwrap_err();
        assert!(matches!(error, kube::Error::Api(status) if status.is_conflict()));
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, Method::PUT);
        assert_eq!(got[0].1.metadata.resource_version.as_deref(), Some("7"));
    }

    #[tokio::test]
    async fn a_replace_without_the_version_read_is_not_sent() {
        let (api, got) = server(200, CREATED);
        assert!(replace_at(&api, &secret(None)).await.is_err());
        assert!(got.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_create_keeps_the_object_made_first() {
        let (api, _) = server(201, CREATED);
        assert!(matches!(
            create_once(&api, &secret(None)).await.unwrap(),
            Created::New(_)
        ));
        let (api, got) = server(409, &conflict("AlreadyExists"));
        assert!(matches!(
            create_once(&api, &secret(None)).await.unwrap(),
            Created::Exists
        ));
        assert_eq!(got.lock().unwrap()[0].0, Method::POST);
    }
}

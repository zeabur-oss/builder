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

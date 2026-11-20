//! Leader election with the `coordination.k8s.io` Lease [`LEASE`] in the controller's namespace.
//!
//! Every replica watches, so its caches are ready when it takes the Lease. Only the holder
//! reconciles and sweeps. The webhook receiver runs on every replica: it only writes
//! `status.lastWebhookAt`.
//!
//! A Lease does not fence. If a holder stops renewing, two replicas can act for a short time.
//! A `BuildRun` name is a function of its trigger ([`crate::decide::run_name`]), so the second
//! create of the same run gets `409 Conflict`.

use std::time::Duration;

use futures_util::Stream;
use kube::Client;
use kube_lease_manager::{LeaseManager, LeaseManagerBuilder, LeaseManagerError};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The Lease's name.
pub(crate) const LEASE: &str = "zeabur-builder";
/// How long a stopped replica may take to give the Lease back.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the Lease could not be set up.
#[derive(Debug, Error)]
pub(crate) enum LeaderError {
    /// The Lease manager refused its settings or the API server.
    #[error("Lease `{LEASE}` in `{namespace}`: {source}")]
    Lease {
        namespace: String,
        #[source]
        source: LeaseManagerError,
    },
}

/// Whether this replica holds the Lease.
#[derive(Debug, Clone)]
pub(crate) struct Leader(watch::Receiver<bool>);

impl Leader {
    /// Whether this replica holds the Lease now.
    pub(crate) fn is_leader(&self) -> bool {
        *self.0.borrow()
    }

    /// One item each time this replica takes the Lease.
    pub(crate) fn taken(&self) -> impl Stream<Item = ()> + Send + 'static {
        futures_util::stream::unfold(self.0.clone(), |mut state| async move {
            loop {
                state.changed().await.ok()?;
                if *state.borrow_and_update() {
                    return Some(((), state));
                }
            }
        })
    }
}

/// The task that takes and renews the Lease.
#[derive(Debug)]
pub(crate) struct Election(JoinHandle<kube_lease_manager::Result<LeaseManager>>);

impl Election {
    /// Waits until the task gives the Lease back. Drop every [`Leader`] before you call this.
    pub(crate) async fn release(self) {
        match tokio::time::timeout(RELEASE_TIMEOUT, self.0).await {
            Ok(Ok(Ok(_))) => {}
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "lease not released"),
            Ok(Err(e)) => tracing::warn!(error = %e, "lease task failed"),
            Err(_) => tracing::warn!("lease not released in time"),
        }
    }
}

/// Starts to take and renew the Lease in `namespace`, as `identity` when it is set.
pub(crate) async fn elect(
    client: Client,
    namespace: &str,
    identity: Option<String>,
) -> Result<(Leader, Election), LeaderError> {
    let mut builder = LeaseManagerBuilder::new(client, LEASE).with_namespace(namespace);
    if let Some(identity) = identity {
        builder = builder.with_identity(identity);
    }
    let manager = builder.build().await.map_err(|source| LeaderError::Lease {
        namespace: namespace.to_owned(),
        source,
    })?;
    let (state, task) = manager.watch().await;
    Ok((Leader(state), Election(task)))
}

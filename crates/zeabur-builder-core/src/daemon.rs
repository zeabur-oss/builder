//! Naming the buildkitd of a build namespace.
//!
//! Every namespace that runs builds gets its own buildkitd: a `StatefulSet` of at most one Pod,
//! with its own CA and cache volume, in the builder's namespace. Sharing a daemon across
//! namespaces shares its layer cache, its cache mounts (keyed only by `id`, which a Dockerfile
//! chooses), its build history and its network, and `BuildKit`'s control API has no per-caller
//! authorisation ([control.go]); so the boundary is the daemon itself.
//!
//! The name is `buildkitd-<namespace>`, so `kubectl get pods` reads plainly. A `StatefulSet`'s
//! name may be at most 52 characters, since its Pods carry the label
//! `controller-revision-hash: <name>-<10 characters>`, limited to 63. Longer namespaces keep their
//! first 33 characters and add a hash of the whole name.
//!
//! [control.go]: https://github.com/moby/buildkit/blob/v0.34.1/control/control.go

use sha2::{Digest as _, Sha256};

/// The longest `StatefulSet` name whose Pods' `controller-revision-hash` label still fits.
pub const MAX_NAME: usize = 52;
const PREFIX: &str = "buildkitd-";

/// The `StatefulSet`, headless Service and Secret prefix of a namespace's buildkitd.
#[must_use]
pub fn name(namespace: &str) -> String {
    if PREFIX.len() + namespace.len() <= MAX_NAME {
        return format!("{PREFIX}{namespace}");
    }
    let digest = Sha256::digest(namespace.as_bytes());
    let hash = crate::archive::hex(digest.first_chunk::<4>().map_or(&[], |first| first));
    let keep = MAX_NAME - PREFIX.len() - 1 - hash.len();
    let head = namespace.get(..keep).unwrap_or(namespace);
    format!("{PREFIX}{head}-{hash}")
}

/// The Pod of a namespace's buildkitd: the `StatefulSet`'s only ordinal.
#[must_use]
pub fn pod(namespace: &str) -> String {
    format!("{}-0", name(namespace))
}

/// The address build Jobs dial, through the headless Service in `daemon_namespace`.
#[must_use]
pub fn address(namespace: &str, daemon_namespace: &str) -> String {
    let name = name(namespace);
    format!("tcp://{name}-0.{name}.{daemon_namespace}.svc:1234")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_namespaces_keep_their_name() {
        assert_eq!(name("team-a"), "buildkitd-team-a");
        assert_eq!(pod("team-a"), "buildkitd-team-a-0");
        assert_eq!(
            address("team-a", "builder-system"),
            "tcp://buildkitd-team-a-0.buildkitd-team-a.builder-system.svc:1234"
        );
        let longest = "n".repeat(MAX_NAME - PREFIX.len());
        assert_eq!(name(&longest), format!("{PREFIX}{longest}"));
    }

    #[test]
    fn long_namespaces_fit_and_stay_distinct() {
        let a = format!("{}a", "n".repeat(62));
        let b = format!("{}b", "n".repeat(62));
        assert_eq!(name(&a).len(), MAX_NAME);
        assert_ne!(name(&a), name(&b));
        assert!(name(&a).starts_with("buildkitd-nnnn"));
        assert_eq!(name(&a), name(&a));
    }
}

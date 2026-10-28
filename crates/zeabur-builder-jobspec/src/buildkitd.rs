//! A build namespace's buildkitd, as a pure function of the namespace and the controller's
//! settings.
//!
//! A headless Service, a `StatefulSet` of zero or one Pod, and a `NetworkPolicy` that admits only
//! that namespace's Pods. All three live in the builder's namespace, where project members cannot
//! exec into the daemon or read its server key.
//!
//! - **One CA per namespace.** buildkitd checks only that a client certificate chains to its
//!   `--tlscacert` and ignores whom it names, so the Secret it mounts holds that namespace's CA
//!   alone. A client certificate read from a project's namespace opens only that project's daemon.
//! - **Ingress.** Port 1234 admits Pods whose namespace is the build namespace, by
//!   `kubernetes.io/metadata.name`, which the API server sets and nobody can change. A Pod label
//!   would not do: project members create Pods with any label they like.
//! - **Cache.** One volume per namespace, kept when the daemon scales to zero (`whenScaled:
//!   Retain`) and deleted with the `StatefulSet` (`whenDeleted: Delete`), which the controller
//!   removes when the namespace is gone.
//! - **Isolation.** `hostUsers: false` runs the privileged daemon in a user namespace (Kubernetes
//!   1.33 or later); `runtimeClassName` can move it into a sandbox such as Kata or gVisor where the
//!   cluster has one. Neither is a substitute for the other.
//! - **Network.** `RUN` steps get their own network namespace through the config's `networkMode =
//!   "bridge"`, so they leave through the Pod's address and the cluster-wide egress policy on
//!   `app.kubernetes.io/name: buildkitd` applies to them; that policy and the config are deployment
//!   files (`deploy/chart/templates/buildkitd.yaml`). Their TCP segments are sized to the Pod's
//!   interface (`START`).
//! - **Probes.** `buildctl debug workers` over the local socket, which needs no client certificate;
//!   every two seconds, since a build waits on readiness after a cold start.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    StatefulSet, StatefulSetPersistentVolumeClaimRetentionPolicy, StatefulSetSpec,
};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, ContainerPort, ExecAction, KeyToPath, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
    SecretVolumeSource, SecurityContext, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
    NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use zeabur_builder_api::labels;
use zeabur_builder_core::daemon;

/// The port build Jobs dial.
pub const PORT: i32 = 1234;
/// `app.kubernetes.io/name` of every buildkitd Pod, which the egress policy selects.
pub const APP: &str = "buildkitd";
const APP_LABEL: &str = "app.kubernetes.io/name";
const SOCKET: &str = "unix:///run/buildkit/buildkitd.sock";

/// How the container starts buildkitd (`sh -c START buildkitd <args>`): first, the TCP
/// connections of `RUN` steps get a maximum segment size that fits the Pod's interface.
///
/// The steps' bridge and veths have the kernel's default MTU of 1500, and buildkit's `bridge`
/// network mode has no setting for it (`util/network/cniprovider/bridge.go` in buildkit v0.34.1).
/// The Pod's interface is often smaller: 1450 under flannel's VXLAN, k3s's default. A step then
/// announces an MSS of 1460, the server's full-size replies are too large for the Pod's interface,
/// and unless the ICMP "fragmentation needed" reaches the server the connection hangs once a large
/// segment comes, such as the TLS certificate. On GitHub's arm64 runners with k3d it did: plain
/// HTTP and DNS from a step worked, HTTPS hung, `apk add` never ended and `mise install` failed.
/// Clamping the MSS of forwarded SYNs to the route's MTU (`TCPMSS --clamp-mss-to-pmtu`,
/// iptables-extensions(8)) keeps every segment within the Pod's MTU. Without the rule buildkitd
/// still starts, and steps work wherever the Pod's MTU is 1500.
const START: &str = "\
if iptables -t mangle -A FORWARD -p tcp --tcp-flags SYN,RST SYN -j TCPMSS --clamp-mss-to-pmtu; then
  echo \"RUN steps: TCP MSS clamped to the route MTU (eth0: $(cat /sys/class/net/eth0/mtu))\"
else
  echo 'warning: cannot clamp the TCP MSS of RUN steps; they may hang where the Pod MTU is under 1500' >&2
fi
exec buildkitd \"$@\"";

/// The Secret with a namespace's CA and certificates. buildkitd mounts its `ca.pem`, `cert.pem`
/// and `key.pem`; the controller copies the client pair into the namespace.
#[must_use]
pub fn tls_secret(namespace: &str) -> String {
    format!("{}-tls", daemon::name(namespace))
}

/// What the controller decided for one namespace's buildkitd.
#[derive(Debug, Clone, Copy)]
pub struct DaemonInputs<'a> {
    /// The build namespace it serves.
    pub namespace: &'a str,
    /// The builder's namespace, where it runs.
    pub daemon_namespace: &'a str,
    /// `moby/buildkit`, rootful.
    pub image: &'a str,
    /// The `ConfigMap` with `buildkitd.toml`, in `daemon_namespace`.
    pub config_map: &'a str,
    /// 1 to run, 0 when idle.
    pub replicas: i32,
    /// The cache volume's size, such as `20Gi`.
    pub cache_size: &'a str,
    /// The cache volume's storage class; the cluster's default when unset.
    pub storage_class: Option<&'a str>,
    /// A sandboxed runtime, such as `kata` or `gvisor`; runc when unset.
    pub runtime_class: Option<&'a str>,
    /// `false` runs it in a user namespace; `true` only for clusters older than 1.33.
    pub host_users: bool,
    /// Requests and limits, as quantities such as `100m` and `512Mi`.
    pub resources: &'a Resources,
}

/// The daemon's container resources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// `requests.cpu`.
    pub cpu_request: Option<String>,
    /// `requests.memory`.
    pub memory_request: Option<String>,
    /// `limits.memory`.
    pub memory_limit: Option<String>,
}

/// The objects of one namespace's buildkitd.
#[derive(Debug, Clone, PartialEq)]
pub struct Daemon {
    /// The headless Service behind `<name>-0.<name>.<daemon namespace>.svc`.
    pub service: Service,
    /// The daemon itself.
    pub stateful_set: StatefulSet,
    /// Admits the build namespace's Pods on [`PORT`], and nothing else.
    pub network_policy: NetworkPolicy,
}

/// Builds the objects.
#[must_use]
pub fn daemon(inputs: &DaemonInputs<'_>) -> Daemon {
    let name = daemon::name(inputs.namespace);
    let selector = BTreeMap::from([
        (APP_LABEL.to_owned(), APP.to_owned()),
        (labels::NAMESPACE.to_owned(), inputs.namespace.to_owned()),
    ]);
    let mut object_labels = selector.clone();
    object_labels.insert(labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned());
    let meta = ObjectMeta {
        name: Some(name.clone()),
        namespace: Some(inputs.daemon_namespace.to_owned()),
        labels: Some(object_labels.clone()),
        ..ObjectMeta::default()
    };

    let service = Service {
        metadata: meta.clone(),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_owned()),
            selector: Some(selector.clone()),
            ports: Some(vec![ServicePort {
                name: Some("grpc".to_owned()),
                port: PORT,
                ..ServicePort::default()
            }]),
            ..ServiceSpec::default()
        }),
        status: None,
    };

    let network_policy = network_policy(inputs.namespace, meta.clone(), selector.clone());

    let stateful_set = StatefulSet {
        metadata: meta,
        spec: Some(StatefulSetSpec {
            service_name: Some(name),
            replicas: Some(inputs.replicas),
            pod_management_policy: Some("Parallel".to_owned()),
            persistent_volume_claim_retention_policy: Some(
                StatefulSetPersistentVolumeClaimRetentionPolicy {
                    when_scaled: Some("Retain".to_owned()),
                    when_deleted: Some("Delete".to_owned()),
                },
            ),
            selector: LabelSelector {
                match_labels: Some(selector),
                ..LabelSelector::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(object_labels),
                    ..ObjectMeta::default()
                }),
                spec: Some(pod(inputs)),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("cache".to_owned()),
                    ..ObjectMeta::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_owned()]),
                    storage_class_name: inputs.storage_class.map(str::to_owned),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_owned(),
                            Quantity(inputs.cache_size.to_owned()),
                        )])),
                        ..VolumeResourceRequirements::default()
                    }),
                    ..PersistentVolumeClaimSpec::default()
                }),
                status: None,
            }]),
            ..StatefulSetSpec::default()
        }),
        status: None,
    };

    Daemon {
        service,
        stateful_set,
        network_policy,
    }
}

/// Admits Pods of `namespace` on [`PORT`]; every other ingress is dropped once a policy selects
/// the Pod.
fn network_policy(
    namespace: &str,
    meta: ObjectMeta,
    selector: BTreeMap<String, String>,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta,
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(selector),
                ..LabelSelector::default()
            }),
            policy_types: Some(vec!["Ingress".to_owned()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            "kubernetes.io/metadata.name".to_owned(),
                            namespace.to_owned(),
                        )])),
                        ..LabelSelector::default()
                    }),
                    ..NetworkPolicyPeer::default()
                }]),
                ports: Some(vec![NetworkPolicyPort {
                    port: Some(IntOrString::Int(PORT)),
                    protocol: Some("TCP".to_owned()),
                    ..NetworkPolicyPort::default()
                }]),
            }]),
            ..NetworkPolicySpec::default()
        }),
    }
}

fn pod(inputs: &DaemonInputs<'_>) -> PodSpec {
    let probe = |initial, period| Probe {
        exec: Some(ExecAction {
            command: Some(
                ["buildctl", "--addr", SOCKET, "debug", "workers"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
        }),
        initial_delay_seconds: Some(initial),
        period_seconds: Some(period),
        ..Probe::default()
    };
    let quantities = |pairs: &[(&str, &Option<String>)]| {
        let map: BTreeMap<String, Quantity> = pairs
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| ((*k).to_owned(), Quantity(v.clone()))))
            .collect();
        (!map.is_empty()).then_some(map)
    };
    let resources = inputs.resources;
    PodSpec {
        host_users: Some(inputs.host_users),
        runtime_class_name: inputs.runtime_class.map(str::to_owned),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        containers: vec![Container {
            name: "buildkitd".to_owned(),
            image: Some(inputs.image.to_owned()),
            command: Some(
                ["/bin/sh", "-c", START, "buildkitd"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
            args: Some(vec![
                "--config=/etc/buildkit/buildkitd.toml".to_owned(),
                format!("--addr={SOCKET}"),
                format!("--addr=tcp://0.0.0.0:{PORT}"),
                "--tlscacert=/certs/ca.pem".to_owned(),
                "--tlscert=/certs/cert.pem".to_owned(),
                "--tlskey=/certs/key.pem".to_owned(),
            ]),
            ports: Some(vec![ContainerPort {
                name: Some("grpc".to_owned()),
                container_port: PORT,
                ..ContainerPort::default()
            }]),
            readiness_probe: Some(probe(1, 2)),
            liveness_probe: Some(probe(10, 30)),
            security_context: Some(SecurityContext {
                privileged: Some(true),
                ..SecurityContext::default()
            }),
            resources: Some(ResourceRequirements {
                requests: quantities(&[
                    ("cpu", &resources.cpu_request),
                    ("memory", &resources.memory_request),
                ]),
                limits: quantities(&[("memory", &resources.memory_limit)]),
                ..ResourceRequirements::default()
            }),
            volume_mounts: Some(vec![
                mount("config", "/etc/buildkit", true),
                mount("certs", "/certs", true),
                mount("cache", "/var/lib/buildkit", false),
            ]),
            ..Container::default()
        }],
        volumes: Some(vec![
            Volume {
                name: "config".to_owned(),
                config_map: Some(ConfigMapVolumeSource {
                    name: inputs.config_map.to_owned(),
                    ..ConfigMapVolumeSource::default()
                }),
                ..Volume::default()
            },
            Volume {
                name: "certs".to_owned(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(tls_secret(inputs.namespace)),
                    items: Some(
                        ["ca.pem", "cert.pem", "key.pem"]
                            .map(|key| KeyToPath {
                                key: key.to_owned(),
                                path: key.to_owned(),
                                mode: None,
                            })
                            .to_vec(),
                    ),
                    default_mode: Some(0o400),
                    ..SecretVolumeSource::default()
                }),
                ..Volume::default()
            },
        ]),
        ..PodSpec::default()
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        read_only: read_only.then_some(true),
        ..VolumeMount::default()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn inputs(resources: &Resources) -> DaemonInputs<'_> {
        DaemonInputs {
            namespace: "team-a",
            daemon_namespace: "builder-system",
            image: "docker.io/moby/buildkit:v0.34.1",
            config_map: "buildkitd",
            replicas: 1,
            cache_size: "20Gi",
            storage_class: None,
            runtime_class: None,
            host_users: false,
            resources,
        }
    }

    #[test]
    fn golden() {
        let resources = Resources {
            cpu_request: Some("100m".to_owned()),
            memory_request: Some("256Mi".to_owned()),
            memory_limit: Some("4Gi".to_owned()),
        };
        let daemon = daemon(&inputs(&resources));
        let actual = serde_json::to_string_pretty(&serde_json::json!([
            daemon.service,
            daemon.stateful_set,
            daemon.network_policy,
        ]))
        .unwrap()
            + "\n";
        if std::env::var_os("ZEABUR_BUILDER_BLESS").is_some() {
            let path = format!("{}/golden/buildkitd.json", env!("CARGO_MANIFEST_DIR"));
            std::fs::write(path, &actual).unwrap();
            return;
        }
        assert_eq!(
            include_str!("../golden/buildkitd.json"),
            actual,
            "golden/buildkitd.json is stale"
        );
    }

    #[test]
    fn certificate_secret_matches_the_pod() {
        let resources = Resources::default();
        let daemon = daemon(&inputs(&resources));
        let pod = daemon.stateful_set.spec.unwrap().template.spec.unwrap();
        let certs = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "certs")
            .unwrap();
        assert_eq!(
            certs.secret.unwrap().secret_name.as_deref(),
            Some(tls_secret("team-a").as_str())
        );
        assert_eq!(pod.containers[0].resources.as_ref().unwrap().requests, None);
    }

    #[test]
    fn sandbox_is_optional() {
        let resources = Resources::default();
        let mut inputs = inputs(&resources);
        inputs.runtime_class = Some("kata");
        inputs.replicas = 0;
        let set = daemon(&inputs).stateful_set.spec.unwrap();
        assert_eq!(set.replicas, Some(0));
        let pod = set.template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("kata"));
        assert_eq!(pod.host_users, Some(false));
    }
}

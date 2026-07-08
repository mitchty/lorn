use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::time::Duration;

use clap::Parser;
use futures::future::try_join_all;
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet, StatefulSet},
    batch::v1::Job,
    core::v1::{
        Container, Namespace, PersistentVolume, PersistentVolumeClaim, Pod, PodSecurityContext,
        PodSpec, SecurityContext, Service, ServicePort,
    },
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    Client, Config,
    api::{Api, ApiResource, DynamicObject, ListParams},
    config::{KubeConfigOptions, Kubeconfig},
};
use thiserror::Error;
use tracing::{error, warn};

/// they shouldn't be able to bind.
#[derive(Debug, Parser)]
#[command(name = "lorn", version, about, long_about = None)]
struct Cli {
    /// Path to a kubeconfig file, fall back to the KUBECONFIG env var, then
    /// in-cluster/default config discovery if unset.
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<String>,

    /// Number of retries to attempt when talking to the k8s API server fails
    /// with a retryable 5xx error while creating the client for now.
    #[arg(long, default_value_t = 5)]
    retries: u32,

    /// Initial backoff delay, in milliseconds, before the first retry. Each
    /// subsequent retry doubles this delay with a circuit breaker.
    #[arg(long, default_value_t = 250)]
    retry_initial_backoff_ms: u64,

    /// Maximum backoff delay, in milliseconds, between retries.
    #[arg(long, default_value_t = 10_000)]
    retry_max_backoff_ms: u64,

    /// Increase log verbosity. Overridden by the RUST_LOG env var if set in the
    /// user env.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Error)]
enum LornError {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("kube config error: {0}")]
    KubeConfig(#[from] kube::config::KubeconfigError),
    #[error("infer config error: {0}")]
    InferConfig(#[from] kube::config::InferConfigError),
}

impl LornError {
    /// Whether this error looks transient enough to be worth retrying, mostly
    /// here to retry when we get 5xx from the apiserver when constructing the
    /// client. Repeated failures is fatal.
    fn is_retryable(&self) -> bool {
        matches!(self, LornError::Kube(kube::Error::Api(resp)) if (500..600).contains(&resp.code))
    }
}

async fn retry_with_backoff<T, F, Fut>(
    retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut f: F,
) -> Result<T, LornError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LornError>>,
{
    let mut attempt = 0;
    let mut backoff = initial_backoff;

    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < retries && err.is_retryable() => {
                warn!(
                    "attempt {}/{} failed, retrying in {backoff:?}: {err}",
                    attempt + 1,
                    retries + 1,
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
                backoff = (backoff * 2).min(max_backoff);
            }
            Err(err) => return Err(err),
        }
    }
}

async fn build_client(cli: &Cli) -> Result<Client, LornError> {
    let initial_backoff = Duration::from_millis(cli.retry_initial_backoff_ms);
    let max_backoff = Duration::from_millis(cli.retry_max_backoff_ms);

    retry_with_backoff(cli.retries, initial_backoff, max_backoff, || async {
        if let Some(path) = cli.kubeconfig.as_ref() {
            let kubeconfig = Kubeconfig::read_from(std::path::Path::new(path))?;
            let config =
                Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default()).await?;
            Ok(Client::try_from(config)?)
        } else {
            Ok(Client::try_default().await?)
        }
    })
    .await
}

fn pvc_names_from_volumes<'a>(
    volumes: impl Iterator<Item = Option<&'a Vec<k8s_openapi::api::core::v1::Volume>>>,
) -> HashSet<String> {
    volumes
        .flatten()
        .flatten()
        .filter_map(|v| v.persistent_volume_claim.as_ref())
        .map(|src| src.claim_name.clone())
        .collect()
}

async fn pvc_in_use(client: &Client, ns: &str, pvc_name: &str) -> Result<bool, LornError> {
    let lp = ListParams::default();

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pod_future = async {
        let pods = pods_api.list(&lp).await?;
        let referenced: HashSet<String> = pvc_names_from_volumes(
            pods.items
                .iter()
                .map(|p| p.spec.as_ref().and_then(|s| s.volumes.as_ref())),
        );
        Ok::<bool, LornError>(referenced.contains(pvc_name))
    };

    // StatefulSets volume bindings come from volumeClaimTemplates here
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let sts_future = async {
        let sets = sts_api.list(&lp).await?;
        let found = sets.items.iter().any(|s| {
            s.spec
                .as_ref()
                .and_then(|spec| spec.volume_claim_templates.as_ref())
                .is_some_and(|templates| {
                    templates
                        .iter()
                        .any(|t| t.metadata.name.as_deref().is_some_and(|n| n == pvc_name))
                })
        });
        Ok::<bool, LornError>(found)
    };

    let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let deploy_future = async {
        let deploys = deploy_api.list(&lp).await?;
        let referenced: HashSet<String> = pvc_names_from_volumes(deploys.items.iter().map(|d| {
            d.spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ps| ps.volumes.as_ref())
        }));
        Ok::<bool, LornError>(referenced.contains(pvc_name))
    };

    let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);
    let jobs_future = async {
        let jobs = jobs_api.list(&lp).await?;
        let referenced: HashSet<String> = pvc_names_from_volumes(jobs.items.iter().map(|j| {
            j.spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ps| ps.volumes.as_ref())
        }));
        Ok::<bool, LornError>(referenced.contains(pvc_name))
    };

    let (pod_ref, sts_ref, deploy_ref, job_ref) =
        tokio::try_join!(pod_future, sts_future, deploy_future, jobs_future)?;

    Ok(pod_ref || sts_ref || deploy_ref || job_ref)
}

async fn find_orphaned_pvcs(client: &Client) -> Result<Vec<(String, String, String)>, LornError> {
    let all_pvcs = Api::<PersistentVolumeClaim>::all(client.clone())
        .list(&ListParams::default())
        .await?;

    let checks = all_pvcs.items.iter().map(|pvc| {
        let ns = pvc
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let pvc_name = pvc
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unnamed>".to_string());
        let pv_name = pvc
            .spec
            .as_ref()
            .and_then(|s| s.volume_name.clone())
            .unwrap_or_else(|| "<unbound>".to_string());
        let client = client.clone();
        async move {
            let in_use = pvc_in_use(&client, &ns, &pvc_name).await?;
            Ok::<_, LornError>((ns, pvc_name, pv_name, in_use))
        }
    });

    let results = try_join_all(checks).await?;
    Ok(results
        .into_iter()
        .filter_map(|(ns, pvc, pv, in_use)| (!in_use).then_some((ns, pvc, pv)))
        .collect())
}

fn is_bitnami_image(image: &str) -> bool {
    image.starts_with("docker.io/bitnami/") || image.starts_with("bitnami/")
}

fn bitnami_images_in_pod_spec(spec: &PodSpec) -> Vec<(String, String)> {
    let containers = spec
        .containers
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("spec.containers[{i}]"), c));

    let init_containers = spec
        .init_containers
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| (format!("spec.initContainers[{i}]"), c));

    containers
        .chain(init_containers)
        .filter_map(|(path, c)| c.image.as_deref().map(|img| (path, img.to_owned())))
        .filter(|(_, img)| is_bitnami_image(img))
        .collect()
}

#[derive(Debug)]
struct BitnamiHit {
    /// What kind of resource we found an issue with
    kind: &'static str,
    /// Where the resouce was and its name `namespace/name`
    name: String,
    /// The path in the k8s yaml: aka `spec.containers[0]` or `spec.initContainers[1]`
    container_path: String,
    image: String,
}

async fn find_bitnami_images_in_ns(
    client: &Client,
    ns: &str,
) -> Result<Vec<BitnamiHit>, LornError> {
    let lp = ListParams::default();

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pod_future = async {
        let pods = pods_api.list(&lp).await?;
        let hits: Vec<BitnamiHit> = pods
            .items
            .iter()
            .flat_map(|p| {
                let resource_name = format!(
                    "{}/{}",
                    ns,
                    p.metadata.name.as_deref().unwrap_or("<unnamed>")
                );
                p.spec
                    .iter()
                    .flat_map(bitnami_images_in_pod_spec)
                    .map(move |(container_path, image)| BitnamiHit {
                        kind: "pod",
                        name: resource_name.clone(),
                        container_path,
                        image,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok::<Vec<BitnamiHit>, LornError>(hits)
    };

    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let sts_future = async {
        let sets = sts_api.list(&lp).await?;
        let hits: Vec<BitnamiHit> = sets
            .items
            .iter()
            .flat_map(|s| {
                let resource_name = format!(
                    "{}/{}",
                    ns,
                    s.metadata.name.as_deref().unwrap_or("<unnamed>")
                );
                s.spec
                    .iter()
                    .flat_map(|spec| {
                        spec.template
                            .spec
                            .iter()
                            .flat_map(bitnami_images_in_pod_spec)
                    })
                    .map(move |(container_path, image)| BitnamiHit {
                        kind: "statefulset",
                        name: resource_name.clone(),
                        container_path,
                        image,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok::<Vec<BitnamiHit>, LornError>(hits)
    };

    let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let deploy_future = async {
        let deploys = deploy_api.list(&lp).await?;
        let hits: Vec<BitnamiHit> = deploys
            .items
            .iter()
            .flat_map(|d| {
                let resource_name = format!(
                    "{}/{}",
                    ns,
                    d.metadata.name.as_deref().unwrap_or("<unnamed>")
                );
                d.spec
                    .iter()
                    .flat_map(|spec| {
                        spec.template
                            .spec
                            .iter()
                            .flat_map(bitnami_images_in_pod_spec)
                    })
                    .map(move |(container_path, image)| BitnamiHit {
                        kind: "deployment",
                        name: resource_name.clone(),
                        container_path,
                        image,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok::<Vec<BitnamiHit>, LornError>(hits)
    };

    let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);
    let jobs_future = async {
        let jobs = jobs_api.list(&lp).await?;
        let hits: Vec<BitnamiHit> = jobs
            .items
            .iter()
            .flat_map(|j| {
                let resource_name = format!(
                    "{}/{}",
                    ns,
                    j.metadata.name.as_deref().unwrap_or("<unnamed>")
                );
                j.spec
                    .iter()
                    .flat_map(|spec| {
                        spec.template
                            .spec
                            .iter()
                            .flat_map(bitnami_images_in_pod_spec)
                    })
                    .map(move |(container_path, image)| BitnamiHit {
                        kind: "job",
                        name: resource_name.clone(),
                        container_path,
                        image,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok::<Vec<BitnamiHit>, LornError>(hits)
    };

    let (mut pod_hits, sts_hits, deploy_hits, job_hits) =
        tokio::try_join!(pod_future, sts_future, deploy_future, jobs_future)?;

    pod_hits.extend(sts_hits);
    pod_hits.extend(deploy_hits);
    pod_hits.extend(job_hits);
    Ok(pod_hits)
}

async fn find_bitnami_images(
    client: &Client,
    namespaces: &[String],
) -> Result<Vec<BitnamiHit>, LornError> {
    let per_ns = namespaces.iter().map(|ns| {
        let ns = ns.clone();
        let client = client.clone();
        async move { find_bitnami_images_in_ns(&client, &ns).await }
    });

    let nested = try_join_all(per_ns).await?;
    Ok(nested.into_iter().flatten().collect())
}

#[derive(Debug)]
struct CsiNoSnapshot {
    /// csi driver name according to the k8s api, e.g. `ebs.csi.aws.com` or `csi.san.synology.com`
    driver: String,
    /// PV names that are actively using this driver.
    pvs: Vec<String>,
    /// True when the VolumeSnapshotClass CRD itself is not installed for this csi driver.
    // Note: this would be for a local path provisioner.
    crd_missing: bool,
}

async fn find_csi_no_snapshot(client: &Client) -> Result<Vec<CsiNoSnapshot>, LornError> {
    let pv_api: Api<PersistentVolume> = Api::all(client.clone());

    let vsc_ar = ApiResource {
        group: "snapshot.storage.k8s.io".into(),
        version: "v1".into(),
        api_version: "snapshot.storage.k8s.io/v1".into(),
        kind: "VolumeSnapshotClass".into(),
        plural: "volumesnapshotclasses".into(),
    };
    let vsc_api: Api<DynamicObject> = Api::all_with(client.clone(), &vsc_ar);

    let lp = ListParams::default();
    let (pvs_result, vscs_result) = tokio::join!(pv_api.list(&lp), vsc_api.list(&lp),);

    let pvs = pvs_result?;

    // CSI drivers that have at least one VolumeSnapshotClass registered. `None`
    // means the CRD is not installed at all, which is an odd edge case but can
    // happen if the clsuter only has local-path provisioner and throws the
    // overall logic off of assuming the CRD exists at all times whenever there
    // is a storage class.
    let (snapshot_capable, crd_missing): (BTreeSet<String>, bool) = match vscs_result {
        Ok(list) => {
            let drivers = list
                .items
                .iter()
                .filter_map(|vsc| {
                    vsc.data
                        .get("driver")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            (drivers, false)
        }
        // This technically indicates the CRD for snapshots doesn't exist at all.
        Err(kube::Error::Api(ref err)) if err.code == 404 => (BTreeSet::new(), true),
        Err(e) => return Err(e.into()),
    };

    let mut by_driver: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pv in &pvs.items {
        if let Some(csi) = pv.spec.as_ref().and_then(|s| s.csi.as_ref()) {
            let driver = &csi.driver;
            if crd_missing || !snapshot_capable.contains(driver) {
                by_driver.entry(driver.clone()).or_default().push(
                    pv.metadata
                        .name
                        .clone()
                        .unwrap_or_else(|| "<unnamed>".into()),
                );
            }
        }
    }

    Ok(by_driver
        .into_iter()
        .map(|(driver, mut pvs)| {
            pvs.sort();
            CsiNoSnapshot {
                driver,
                pvs,
                crd_missing,
            }
        })
        .collect())
}

/// Whether we were able to confirm a socket is actually bound to a
/// privileged port inside a pod's network namespace, via a best effort
/// port forward attempt. Mostly we want the Bound state for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortBindState {
    /// The probe found something bound to a socket at this port.
    Bound,
    /// The probe came back with a "connection refused" type of error: nothing
    /// is listening in this instance. Strong evidence the finding that led here
    /// is a false positive or for some reason a pod is configured to use a port
    /// but not actually bind()'ng to it.
    Unbound,
    /// Couldn't determine either way or its ambiguous. There is only so much I
    /// can do to try to find ports actually bound from within k8s with no
    /// access to the underlying systems to nsenter into the pod's net namespace
    /// and truly see what's bound.
    Unknown,
}

impl std::fmt::Display for PortBindState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PortBindState::Bound => "bound",
            PortBindState::Unbound => "unbound",
            PortBindState::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug)]
struct PrivilegedServicePod {
    namespace: String,
    pod_name: String,
    service_name: String,
    port: i32,
    protocol: String,
    bind_state: PortBindState,
    /// Lowercased k8s kind of the owning workload, e.g. `deployment`,
    /// `statefulset`, `replicaset`, or `pod` if it has no controller.
    // If this were a more grown up program this would be an enum.
    owner_kind: String,
    owner_name: String,
}

/// Try to find out if a pod has the ability/k8s setup to bind to ports below
/// 1024. I think this is all most of what would allow this to happen.
fn can_bind_privileged_port(
    pod_sc: Option<&PodSecurityContext>,
    container_sc: Option<&SecurityContext>,
    port: i32,
) -> bool {
    if container_sc.and_then(|sc| sc.privileged).unwrap_or(false) {
        return true;
    }

    let has_bind_capability = container_sc
        .and_then(|sc| sc.capabilities.as_ref())
        .and_then(|caps| caps.add.as_ref())
        .is_some_and(|added| added.iter().any(|c| c == "NET_BIND_SERVICE" || c == "ALL"));
    if has_bind_capability {
        return true;
    }

    let run_as_user = container_sc
        .and_then(|sc| sc.run_as_user)
        .or_else(|| pod_sc.and_then(|sc| sc.run_as_user));
    if run_as_user == Some(0) {
        return true;
    }

    pod_sc
        .and_then(|sc| sc.sysctls.as_ref())
        .into_iter()
        .flatten()
        .filter(|s| s.name == "net.ipv4.ip_unprivileged_port_start")
        .filter_map(|s| s.value.trim().parse::<i32>().ok())
        .any(|threshold| threshold <= port)
}

/// Finds the container, be it regular or an init container in a pod spec that
/// declares the given `containerPort`, if any.
fn container_for_port(pod_spec: &PodSpec, port: i32) -> Option<&Container> {
    pod_spec
        .containers
        .iter()
        .chain(pod_spec.init_containers.iter().flatten())
        .find(|c| c.ports.iter().flatten().any(|p| p.container_port == port))
}

fn resolve_target_port(sp: &ServicePort, pod_spec: &PodSpec) -> Option<i32> {
    let (port, container) = match sp.target_port.as_ref() {
        Some(IntOrString::Int(n)) => (*n, container_for_port(pod_spec, *n)),
        Some(IntOrString::String(name)) => pod_spec
            .containers
            .iter()
            .chain(pod_spec.init_containers.iter().flatten())
            .find_map(|c| {
                c.ports
                    .iter()
                    .flatten()
                    .find(|p| p.name.as_deref() == Some(name.as_str()))
                    .map(|p| (p.container_port, Some(c)))
            })?,
        None => (sp.port, container_for_port(pod_spec, sp.port)),
    };

    if port >= 1024 {
        return None;
    }

    let can_bind = can_bind_privileged_port(
        pod_spec.security_context.as_ref(),
        container.and_then(|c| c.security_context.as_ref()),
        port,
    );
    (!can_bind).then_some(port)
}

/// Best effort, dependency free (no exec and cat /proc/net/tcp... basically)
/// confirmation that a tcp port has an actual bound socket inside a pod's
/// network namespace: open a portforward session to it and watches for
/// an immediate "connection refused" style error, which kubelet's port-forward
/// helper reports when nothing is listening/bound on the other end.
async fn confirm_bound_port(
    client: &Client,
    ns: &str,
    pod: &str,
    port: i32,
    protocol: &str,
) -> PortBindState {
    if !protocol.eq_ignore_ascii_case("tcp") {
        return PortBindState::Unknown;
    }

    let Ok(port) = u16::try_from(port) else {
        return PortBindState::Unknown;
    };
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);

    let Ok(mut pf) = pods_api.portforward(pod, &[port]).await else {
        return PortBindState::Unknown;
    };
    let Some(error_fut) = pf.take_error(port) else {
        return PortBindState::Unknown;
    };

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), error_fut).await;
    pf.abort();

    match result {
        // The port errored out before the timeout. A refused connection
        // means nothing is bound for sure, anything else is too ambiguous to
        // treat as a disproof which could mean kubelet itself can't port forward.
        Ok(Some(message)) => {
            // TODO: This may not be complete enough but I can't make heads or
            // tails of kubectl's port forward source right now for the rest of
            // the issues that might need to be added, this one for sure though.
            if message.to_lowercase().contains("connection refused") {
                PortBindState::Unbound
            } else {
                PortBindState::Unknown
            }
        }
        // Error sender was dropped without ever sending which should mean the
        // port was usable the whole time we were watching.
        Ok(None) => PortBindState::Bound,
        // No error surfaced before the timeout. Since we're forwarding
        // to the pod's own loopback-equivalent, a refusal should show up
        // nearly instantly, so treat this as there is a port bound.
        Err(_) => PortBindState::Bound,
    }
}

/// Walk a pod's owner references to find the top level workload
/// that owns it. `ReplicaSet`s are resolved one level further to
/// their owning `Deployment` when possible so the more useful, user
/// facing resource is listed at the end.
fn owner_workload_for_pod(pod: &Pod, replicasets: &[ReplicaSet]) -> (String, String) {
    let controller_owner = pod
        .metadata
        .owner_references
        .as_ref()
        .and_then(|refs| refs.iter().find(|r| r.controller == Some(true)));

    let Some(owner) = controller_owner else {
        let name = pod
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unnamed>".to_string());
        return ("pod".to_string(), name);
    };

    if owner.kind == "ReplicaSet" {
        let rs_owner = replicasets
            .iter()
            .find(|rs| rs.metadata.name.as_deref() == Some(owner.name.as_str()))
            .and_then(|rs| rs.metadata.owner_references.as_ref())
            .and_then(|refs| refs.iter().find(|r| r.controller == Some(true)));

        if let Some(rs_owner) = rs_owner {
            return (rs_owner.kind.to_lowercase(), rs_owner.name.clone());
        }
        return ("replicaset".to_string(), owner.name.clone());
    }

    (owner.kind.to_lowercase(), owner.name.clone())
}

async fn find_privileged_service_pods_in_ns(
    client: &Client,
    ns: &str,
) -> Result<Vec<PrivilegedServicePod>, LornError> {
    let lp = ListParams::default();

    let svc_api: Api<Service> = Api::namespaced(client.clone(), ns);
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let rs_api: Api<ReplicaSet> = Api::namespaced(client.clone(), ns);

    let (services, pods, replicasets) =
        tokio::try_join!(svc_api.list(&lp), pods_api.list(&lp), rs_api.list(&lp))?;

    let mut hits = Vec::new();

    for svc in &services.items {
        let Some(spec) = svc.spec.as_ref() else {
            continue;
        };

        let Some(selector) = spec.selector.as_ref().filter(|s| !s.is_empty()) else {
            continue;
        };

        let service_ports: Vec<&ServicePort> = spec.ports.iter().flatten().collect();

        let service_name = svc
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "<unnamed>".to_string());

        for pod in &pods.items {
            let Some(pod_spec) = pod.spec.as_ref() else {
                continue;
            };

            let matches = pod
                .metadata
                .labels
                .as_ref()
                .is_some_and(|labels| selector.iter().all(|(k, v)| labels.get(k) == Some(v)));
            if !matches {
                continue;
            }

            // Dedup by (port, protocol) in case multiple service ports resolve
            // to the same target port. How? I dunno but I got dups without
            // this.
            let mut privileged: BTreeSet<(i32, String)> = BTreeSet::new();

            for sp in &service_ports {
                if let Some(resolved) = resolve_target_port(sp, pod_spec) {
                    let protocol = sp.protocol.clone().unwrap_or_else(|| "TCP".to_string());
                    privileged.insert((resolved, protocol));
                }
            }

            if privileged.is_empty() {
                continue;
            }

            let pod_name = pod
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| "<unnamed>".to_string());
            let (owner_kind, owner_name) = owner_workload_for_pod(pod, &replicasets.items);

            for (port, protocol) in privileged {
                // Ignore anything we know isn't actively bound. Bound and Unknown filter up
                let bind_state = confirm_bound_port(client, ns, &pod_name, port, &protocol).await;
                if bind_state == PortBindState::Unbound {
                    continue;
                }

                hits.push(PrivilegedServicePod {
                    namespace: ns.to_string(),
                    pod_name: pod_name.clone(),
                    service_name: service_name.clone(),
                    port,
                    protocol,
                    bind_state,
                    owner_kind: owner_kind.clone(),
                    owner_name: owner_name.clone(),
                });
            }
        }
    }

    Ok(hits)
}

async fn find_privileged_service_pods(
    client: &Client,
    namespaces: &[String],
) -> Result<Vec<PrivilegedServicePod>, LornError> {
    let per_ns = namespaces.iter().map(|ns| {
        let ns = ns.clone();
        let client = client.clone();
        async move { find_privileged_service_pods_in_ns(&client, &ns).await }
    });

    let nested = try_join_all(per_ns).await?;
    Ok(nested.into_iter().flatten().collect())
}

/// Lists all namespace names in the cluster. Some environments (certain
/// AKS RBAC setups in particular) grant access to namespaced resources
/// but deny `list` on the cluster-scoped `namespaces` resource itself,
/// so callers should treat a failure here as "namespace-scoped checks
/// can't run" rather than a fatal error for the whole program.
async fn list_namespace_names(client: &Client) -> Result<Vec<String>, LornError> {
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await?;

    Ok(namespaces
        .items
        .into_iter()
        .map(|ns| ns.metadata.name.unwrap_or_else(|| "default".to_string()))
        .collect())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let default_level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    tracing_subscriber::fmt()
        // Logs are on stderr now.
        .with_writer(std::io::stderr)
        .with_file(true)
        .with_line_number(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .init();

    if let Err(err) = run(cli).await {
        error!("{err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), LornError> {
    let client = build_client(&cli).await?;

    let mut found = false;

    // Namespace-scoped checks may fail with the user RBAC. So try to note that.
    let namespaces = match list_namespace_names(&client).await {
        Ok(namespaces) => namespaces,
        Err(err) => {
            warn!(
                "unable to list namespaces, skipping namespace-scoped checks e.g. bitnami images and privileged service ports err was: {err}"
            );
            found = true;
            Vec::new()
        }
    };

    let (orphaned_pvcs, bitnami_hits, csi_no_snapshot, privileged_service_pods) = tokio::try_join!(
        find_orphaned_pvcs(&client),
        find_bitnami_images(&client, &namespaces),
        find_csi_no_snapshot(&client),
        find_privileged_service_pods(&client, &namespaces),
    )?;

    for (ns, pvc_name, pv_name) in orphaned_pvcs {
        println!("orphaned pvc {ns}/{pvc_name} pv {pv_name}");
        found = true;
    }

    for hit in bitnami_hits {
        println!(
            "{} {} {}.image {}",
            hit.kind, hit.name, hit.container_path, hit.image
        );
        found = true;
    }

    for hit in &csi_no_snapshot {
        for pv in &hit.pvs {
            if hit.crd_missing {
                println!("{} missing snapshot capability for pv {pv}", hit.driver);
            } else {
                println!("csi {} has no snapshot crd for pv {pv}", hit.driver);
            }
            found = true;
        }
    }

    for hit in privileged_service_pods {
        println!(
            "pod {}/{} service {} targetPort {}/{} state {} owner {} {}",
            hit.namespace,
            hit.pod_name,
            hit.service_name,
            hit.port,
            hit.protocol,
            hit.bind_state,
            hit.owner_kind,
            hit.owner_name
        );
        found = true;
    }

    if found {
        std::process::exit(1);
    }

    Ok(())
}

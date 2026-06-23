use std::collections::HashSet;

use futures::future::try_join_all;
use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    batch::v1::Job,
    core::v1::{Namespace, PersistentVolumeClaim, Pod, PodSpec},
};
use kube::{
    Client, Config,
    api::{Api, ListParams},
    config::{KubeConfigOptions, Kubeconfig},
};
use thiserror::Error;

#[derive(Debug, Error)]
enum LornError {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("kube config error: {0}")]
    KubeConfig(#[from] kube::config::KubeconfigError),
    #[error("infer config error: {0}")]
    InferConfig(#[from] kube::config::InferConfigError),
}

async fn build_client() -> Result<Client, LornError> {
    if let Ok(path) = std::env::var("KUBECONFIG") {
        let kubeconfig = Kubeconfig::read_from(std::path::Path::new(&path))?;
        let config =
            Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default()).await?;
        Ok(Client::try_from(config)?)
    } else {
        Ok(Client::try_default().await?)
    }
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

async fn find_bitnami_images(client: &Client) -> Result<Vec<BitnamiHit>, LornError> {
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await?;

    let per_ns = namespaces.items.iter().map(|ns_obj| {
        let ns = ns_obj
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let client = client.clone();
        async move { find_bitnami_images_in_ns(&client, &ns).await }
    });

    let nested = try_join_all(per_ns).await?;
    Ok(nested.into_iter().flatten().collect())
}

#[tokio::main]
async fn main() -> Result<(), LornError> {
    let client = build_client().await?;

    let (orphaned_pvcs, bitnami_hits) =
        tokio::try_join!(find_orphaned_pvcs(&client), find_bitnami_images(&client),)?;

    let mut found = false;

    for (ns, pvc_name, pv_name) in orphaned_pvcs {
        println!("orphaned pvc: {ns}/{pvc_name} pv: {pv_name}");
        found = true;
    }

    for hit in bitnami_hits {
        println!(
            "{} {} {}.image: {}",
            hit.kind, hit.name, hit.container_path, hit.image
        );
        found = true;
    }

    if found {
        std::process::exit(1);
    }

    Ok(())
}

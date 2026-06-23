use std::collections::HashSet;

use futures::future::try_join_all;
use k8s_openapi::api::{
    apps::v1::Deployment,
    apps::v1::StatefulSet,
    batch::v1::Job,
    core::v1::{PersistentVolumeClaim, Pod},
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
        .map(|pvc_src| pvc_src.claim_name.clone())
        .collect()
}

/// Check whether a given PVC is referenced by any Pod, StatefulSet, Deployment,
/// or Job in its given namespace. Returns `true` when the PVC is is in use aka
/// not orphaned. Otherwise false with the error we got back from kube-rs.
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

    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let sts_future = async {
        let sets = sts_api.list(&lp).await?;
        let referenced: bool = sets.items.iter().any(|s| {
            s.spec
                .as_ref()
                .and_then(|spec| spec.volume_claim_templates.as_ref())
                .map(|templates| {
                    templates
                        .iter()
                        .any(|t| t.metadata.name.as_deref().is_some_and(|n| n == pvc_name))
                })
                .unwrap_or(false)
        });
        Ok::<bool, LornError>(referenced)
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

#[tokio::main]
async fn main() -> Result<(), LornError> {
    let client = build_client().await?;

    let pvcs_api: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let all_pvcs = pvcs_api.list(&ListParams::default()).await?;

    if all_pvcs.items.is_empty() {
        return Ok(());
    }

    // For each PVC build a future that checks whether it is in use, collect at
    // the end and dump out what we found.
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

    for (ns, pvc_name, pv_name, in_use) in results {
        if !in_use {
            println!("orphaned pvc: {ns}/{pvc_name}  pv: {pv_name}");
        }
    }

    Ok(())
}

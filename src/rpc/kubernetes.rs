use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use kube::{
	api::{ListParams, Patch, PatchParams, PostParams},
	Api, Client, CustomResource, CustomResourceExt,
};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use netapp::NodeID;

use garage_util::error::Error;

static K8S_GROUP: &str = "deuxfleurs.fr";

#[derive(CustomResource, Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[kube(
	group = "deuxfleurs.fr",
	version = "v1",
	kind = "GarageNode",
	namespaced
)]
pub struct Node {
	hostname: String,
	address: IpAddr,
	port: u16,
}

pub async fn create_kubernetes_crd() -> Result<(), Error> {
	let client = Client::try_default().await?;
	let crds: Api<CustomResourceDefinition> = Api::all(client.clone());

	let params = PatchParams::apply(&format!("garage.{}", K8S_GROUP));
	let crd = GarageNode::crd();
	let patch = Patch::Apply(crd);
	crds.patch(&format!("garagenodes.{}", K8S_GROUP), &params, &patch)
		.await?;

	Ok(())
}

pub async fn get_kubernetes_nodes(
	kubernetes_service_name: &str,
	kubernetes_namespace: &str,
) -> Result<Vec<(NodeID, SocketAddr)>, Error> {
	let client = Client::try_default().await?;
	let nodes: Api<GarageNode> = Api::namespaced(client.clone(), kubernetes_namespace);

	let lp = ListParams::default().labels(&format!(
		"garage.{}/service={}",
		K8S_GROUP, kubernetes_service_name
	));

	let nodes = nodes.list(&lp).await?;
	let mut ret = Vec::with_capacity(nodes.items.len());

	for node in nodes {
		println!("Found Pod: {:?}", node.metadata.name);

		let pubkey = &node
			.metadata
			.name
			.map(|k| hex::decode(&k).ok())
			.flatten()
			.map(|k| NodeID::from_slice(&k[..]))
			.flatten();

		if let Some(pubkey) = pubkey {
			ret.push((*pubkey, SocketAddr::new(node.spec.address, node.spec.port)))
		}
	}

	Ok(ret)
}

pub async fn publish_kubernetes_node(
	kubernetes_service_name: &str,
	kubernetes_namespace: &str,
	node_id: NodeID,
	hostname: &str,
	rpc_public_addr: SocketAddr,
) -> Result<(), Error> {
	let node_pubkey = hex::encode(node_id);

	let mut node = GarageNode::new(
		&node_pubkey,
		Node {
			hostname: hostname.to_string(),
			address: rpc_public_addr.ip(),
			port: rpc_public_addr.port(),
		},
	);

	let labels = node.metadata.labels.insert(BTreeMap::new());
	labels.insert(
		format!("garage.{}/service", K8S_GROUP),
		kubernetes_service_name.to_string(),
	);

	debug!("Node object to be applied: {:#?}", node);

	let client = Client::try_default().await?;
	let nodes: Api<GarageNode> = Api::namespaced(client.clone(), kubernetes_namespace);

	if let Ok(old_node) = nodes.get(&node_pubkey).await {
		node.metadata.resource_version = old_node.metadata.resource_version;
		nodes
			.replace(&node_pubkey, &PostParams::default(), &node)
			.await?;
	} else {
		nodes.create(&PostParams::default(), &node).await?;
	};

	Ok(())
}
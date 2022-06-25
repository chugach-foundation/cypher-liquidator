mod config;
mod fast_tx_builder;
mod liquidator;
mod logging;
mod utils;

use config::*;
use liquidator::*;
use logging::*;
use utils::*;

use clap::Parser;
use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signer::Signer};
use std::{str::FromStr, sync::Arc};

pub const CYPHER_CONFIG_PATH: &str = "./cfg/group.json";

#[derive(Parser)]
struct Cli {
    #[clap(short = 'c', long = "config", parse(from_os_str))]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), LiquidatorError> {
    let args = Cli::parse();

    _ = init_logger();

    // load config
    let config_path = args.config.as_path().to_str().unwrap();

    info!("Loading config from {}", config_path);

    let liquidator_config = Arc::new(load_liquidator_config(config_path).unwrap());
    let cypher_config = Arc::new(load_cypher_config(CYPHER_CONFIG_PATH).unwrap());

    let keypair = load_keypair(liquidator_config.wallet.as_str()).unwrap();
    let pubkey = keypair.pubkey();
    info!("Loaded keypair with pubkey: {}", pubkey.to_string());

    let cluster_config = cypher_config.get_config_for_cluster(liquidator_config.cluster.as_str());
    let cypher_group_config = Arc::new(
        cypher_config
            .get_group(liquidator_config.cluster.as_str())
            .unwrap(),
    );

    let cypher_group_key = Pubkey::from_str(cypher_group_config.address.as_str()).unwrap();

    // initialize rpc client with cluster and cluster url provided in config
    info!(
        "Initializing rpc client for cluster-{} with url: {}",
        liquidator_config.cluster, cluster_config.rpc_url
    );
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        cluster_config.rpc_url.to_string(),
        CommitmentConfig::processed(),
    ));

    let cypher_liqor_pubkey = derive_cypher_user_address(&cypher_group_key, &pubkey);

    let liquidator = Arc::new(Liquidator::new(
        Arc::clone(&rpc_client),
        Arc::clone(&liquidator_config),
        Arc::clone(&cypher_config),
        cypher_group_key,
        cypher_liqor_pubkey,
        keypair,
    ));

    let liq_t = tokio::spawn(async move {
        let res = liquidator.start().await;
        match res {
            Ok(_) => (),
            Err(e) => {
                warn!("An error occurred while running the liquidator: {:?}", e);
            }
        }
    });

    match tokio::join!(liq_t) {
        (Ok(_),) => (),
        (Err(e),) => {
            warn!(
                "An error occurred while joining with the liquidator task: {}",
                e.to_string()
            );
        }
    }

    Ok(())
}

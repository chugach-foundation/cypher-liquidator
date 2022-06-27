mod accounts_cache;
mod chain_meta_service;
mod config;
mod fast_tx_builder;
mod liquidator;
mod logging;
mod simulation;
mod utils;

use chain_meta_service::ChainMetaService;
use config::*;
use liquidator::*;
use logging::*;
use tokio::sync::broadcast::channel;
use utils::*;

use clap::Parser;
use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Keypair, signer::Signer,
};
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

    info!("Loading config from: {}.", config_path);

    let liquidator_config = Arc::new(load_liquidator_config(config_path).unwrap());
    let cypher_config = Arc::new(load_cypher_config(CYPHER_CONFIG_PATH).unwrap());

    let keypair = Arc::new(load_keypair(liquidator_config.wallet.as_str()).unwrap());
    let pubkey = keypair.pubkey();
    info!("Loaded keypair with pubkey: {}.", pubkey.to_string());

    let cluster_config = cypher_config.get_config_for_cluster(liquidator_config.cluster.as_str());
    let cypher_group_config = Arc::new(
        cypher_config
            .get_group(liquidator_config.cluster.as_str())
            .unwrap(),
    );

    let cypher_group_key = Pubkey::from_str(cypher_group_config.address.as_str()).unwrap();

    // initialize rpc client with cluster and cluster url provided in config
    info!(
        "Initializing rpc client for cluster-{} with url: {}.",
        liquidator_config.cluster, cluster_config.rpc_url
    );
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        cluster_config.rpc_url.to_string(),
        CommitmentConfig::processed(),
    ));

    let cypher_liqor_pubkey = derive_cypher_user_address(&cypher_group_key, &pubkey);

    let (shutdown_send, mut _shutdown_recv) = channel::<bool>(1);

    let cms = Arc::new(ChainMetaService::new(
        Arc::clone(&rpc_client),
        shutdown_send.subscribe(),
    ));

    let cms_clone = Arc::clone(&cms);
    let cms_t = tokio::spawn(async move {
        cms_clone.start_service().await;
    });

    tokio::select! {
        _ = run_liquidator(
            rpc_client,
            liquidator_config,
            cypher_config,
            cms,
            cypher_group_key,
            cypher_liqor_pubkey,
            keypair,
        ) => {},
        _ = tokio::signal::ctrl_c() => {
            match shutdown_send.send(true) {
                Ok(_) => {
                    info!("Sucessfully sent shutdown signal. Waiting for tasks to complete...")
                },
                Err(e) => {
                    warn!("Failed to send shutdown error: {}", e.to_string());
                    return Err(LiquidatorError::ShutdownError);
                }
            };
        },
    };

    let (cms_res,) = tokio::join!(cms_t);

    match cms_res {
        Ok(_) => (),
        Err(e) => {
            warn!(
                "There was an error while shutting down the chain meta service: {}",
                e.to_string()
            );
            return Err(LiquidatorError::ShutdownError);
        }
    }

    Ok(())
}

async fn run_liquidator(
    rpc_client: Arc<RpcClient>,
    liquidator_config: Arc<LiquidatorConfig>,
    cypher_config: Arc<CypherConfig>,
    chain_meta_service: Arc<ChainMetaService>,
    cypher_group_key: Pubkey,
    cypher_liqor_pubkey: Pubkey,
    keypair: Arc<Keypair>,
) -> Result<(), LiquidatorError> {
    loop {
        let liquidator = Arc::new(Liquidator::new(
            Arc::clone(&rpc_client),
            Arc::clone(&liquidator_config),
            Arc::clone(&cypher_config),
            Arc::clone(&chain_meta_service),
            cypher_group_key,
            cypher_liqor_pubkey,
            Arc::clone(&keypair),
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
    }
}

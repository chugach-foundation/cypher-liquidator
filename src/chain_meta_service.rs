use {
    log::{info, warn},
    solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient},
    solana_sdk::{commitment_config::CommitmentConfig, hash::Hash},
    std::sync::Arc,
    tokio::{
        sync::{
            broadcast::{channel, Receiver},
            Mutex, RwLock,
        },
        time::{sleep, Duration},
    },
};

pub struct ChainMetaService {
    client: Arc<RpcClient>,
    recent_blockhash: RwLock<Hash>,
    shutdown_receiver: Mutex<Receiver<bool>>,
}

impl ChainMetaService {
    pub fn default() -> Self {
        Self {
            client: Arc::new(RpcClient::new("http://localhost:8899".to_string())),
            recent_blockhash: RwLock::new(Hash::default()),
            shutdown_receiver: Mutex::new(channel::<bool>(1).1),
        }
    }

    pub fn new(client: Arc<RpcClient>, shutdown_receiver: Receiver<bool>) -> ChainMetaService {
        ChainMetaService {
            client,
            shutdown_receiver: Mutex::new(shutdown_receiver),
            ..ChainMetaService::default()
        }
    }

    #[inline(always)]
    async fn update_chain_meta(self: &Arc<Self>) -> Result<(), ClientError> {
        let hash_res = self
            .client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await;
        let hash = match hash_res {
            Ok(hash) => hash,
            Err(e) => {
                warn!("[CMS] Failed to fetch recent block hash: {}", e.to_string());
                return Err(e);
            }
        };
        info!("[CMS] Fetched recent block hash: {}", hash.0.to_string());
        *self.recent_blockhash.write().await = hash.0;

        Ok(())
    }

    #[inline(always)]
    async fn update_chain_meta_replay(self: Arc<Self>) {
        loop {
            let res = self.update_chain_meta().await;

            if res.is_err() {
                warn!(
                    "[CMS] Couldn't get new chain meta! Error: {}",
                    res.err().unwrap().to_string()
                );
            }

            sleep(Duration::from_millis(2000)).await;
        }
    }

    #[inline(always)]
    pub async fn start_service(self: &Arc<Self>) {
        let aself = self.clone();

        let cself = Arc::clone(&aself);
        let mut shutdown = aself.shutdown_receiver.lock().await;
        tokio::select! {
            _ = cself.update_chain_meta_replay() => {},
            _ = shutdown.recv() => {
                info!("[CMS] Received shutdown signal, stopping.");
            }
        }
    }

    #[inline(always)]
    pub async fn get_latest_blockhash(self: &Arc<Self>) -> Hash {
        *self.recent_blockhash.read().await
    }
}

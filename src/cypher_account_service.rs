use cypher::CypherUser;
use cypher::client::get_zero_copy_account;
use dashmap::DashMap;
use log::{info, warn};
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, MemcmpEncoding, RpcFilterType};
use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tokio::sync::broadcast::{channel, Sender};
use tokio::time::Duration;


#[derive(Clone)]
pub struct CypherUserWrapper {
    pub cypher_user: Box<CypherUser>,
    pub cypher_user_pubkey: Pubkey,
}

pub struct CypherAccountService {
    client: Arc<RpcClient>,
    map: DashMap<Pubkey, bool>,
    shutdown_sender: Sender<bool>,
    sender: Sender<CypherUserWrapper>,
    cypher_group_pubkey: Pubkey,
}

impl CypherAccountService {
    pub fn default() -> Self {
        Self {
            client: Arc::new(RpcClient::new("http://localhost:8899".to_string())),
            map: DashMap::new(),
            shutdown_sender: channel::<bool>(1).0,
            sender: channel::<CypherUserWrapper>(1).0,
            cypher_group_pubkey: Pubkey::default(),
        }
    }

    pub fn new(
        client: Arc<RpcClient>,
        shutdown_sender: Sender<bool>,
        sender: Sender<CypherUserWrapper>,
        cypher_group_pubkey: Pubkey,
    ) -> CypherAccountService {
        CypherAccountService {
            client,
            shutdown_sender,
            sender,
            cypher_group_pubkey,
            ..CypherAccountService::default()
        }
    }

    async fn get_all_accounts(self: &Arc<Self>) -> Result<(), ClientError> {
        let map_entries: Vec<Pubkey> = self.map.iter().map(|e| *e.key()).collect();

        for i in (0..self.map.len()).step_by(100) {
            let mut pubkeys: Vec<Pubkey> = Vec::new();
            pubkeys.extend(map_entries[i..map_entries.len().min(i + 100)].iter());

            let accounts_res = self.get_multiple_accounts(&pubkeys).await;
            let accounts = match accounts_res {
                Ok(a) => a,
                Err(e) => {
                    warn!("[CAS] Could not fetch cypher accounts: {}", e.to_string());
                    return Err(e);
                }
            };

            info!("[CAS] Fetched {} cypher user accounts.", accounts.len());

            for (idx, maybe_account) in accounts.iter().enumerate() {
                if maybe_account.is_none() {
                    info!("[CAS] Could not get account with key: {}", pubkeys[idx]);
                    continue;
                }

                let cypher_user =
                    get_zero_copy_account::<CypherUser>(maybe_account.as_ref().unwrap());

                match self.sender.send(CypherUserWrapper {
                    cypher_user,
                    cypher_user_pubkey: pubkeys[idx],
                }) {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(
                            "[CAS] An error occurred while sending cypher user with key: {} - {}",
                            pubkeys[idx], e
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn get_multiple_accounts(
        self: &Arc<Self>,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, ClientError> {
        let accounts_res = self
            .client
            .get_multiple_accounts_with_commitment(pubkeys, CommitmentConfig::confirmed())
            .await;

        let accounts = match accounts_res {
            Ok(a) => a.value,
            Err(e) => {
                warn!("[CAS] Could not fetch cypher accounts: {}", e.to_string());
                return Err(e);
            }
        };
        Ok(accounts)
    }

    async fn get_all_accounts_replay(self: &Arc<Self>) {
        let mut shutdown = self.shutdown_sender.subscribe();

        tokio::select! {
            _ = self.get_all_accounts_replay_inner() => {},
            _ = shutdown.recv() => {
                info!("[CAS] Received shutdown signal, stopping get multiple accounts RPC calls.");
            }
        }
    }

    async fn get_all_accounts_replay_inner(self: &Arc<Self>) {
        loop {
            let res = self.get_all_accounts().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!(
                        "[CAS] There was an error getting all cypher user accounts: {}",
                        e.to_string()
                    );
                }
            };

            // make this request more often that the request that fetches all accounts
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn get_all_program_accounts(self: &Arc<Self>) -> Result<(), ClientError> {
        let filters = Some(vec![
            RpcFilterType::Memcmp(Memcmp {
                offset: 8,
                bytes: MemcmpEncodedBytes::Base58(self.cypher_group_pubkey.to_string()),
                encoding: Some(MemcmpEncoding::Binary),
            }),
            RpcFilterType::DataSize(2208),
        ]);

        let accounts_res = self
            .client
            .get_program_accounts_with_config(
                &cypher::ID,
                RpcProgramAccountsConfig {
                    filters,
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        commitment: Some(CommitmentConfig::default()),
                        data_slice: Some(UiDataSliceConfig {
                            offset: 0,
                            length: 0,
                        }),
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                },
            )
            .await;

        let accounts = match accounts_res {
            Ok(a) => a,
            Err(e) => {
                warn!("[CAS] Could not fetch cypher accounts: {}", e.to_string());
                return Err(e);
            }
        };
        info!("[CAS] Fetched {} cypher user accounts.", accounts.len());

        let mut new_count: usize = 0;

        for (pubkey, _account) in accounts {
            let entry = self.map.contains_key(&pubkey);

            if !entry {
                self.map.insert(pubkey, true);
                new_count += 1;
            }
        }

        info!(
            "[CAS] Added {} new cypher user accounts to the cache.",
            new_count
        );

        Ok(())
    }

    async fn get_all_program_accounts_replay(self: &Arc<Self>) {
        let mut shutdown = self.shutdown_sender.subscribe();

        tokio::select! {
            _ = self.get_all_program_accounts_replay_inner() => {},
            _ = shutdown.recv() => {
                info!("[CAS] Received shutdown signal, stopping get program accounts RPC calls.");
            }
        }
    }

    async fn get_all_program_accounts_replay_inner(self: &Arc<Self>) {
        loop {
            let res = self.get_all_program_accounts().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!(
                        "[CAS] There was an error getting all cypher user accounts: {}",
                        e.to_string()
                    );
                }
            };

            // do not make this request as often as it will only be used to cache accounts
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    #[inline(always)]
    pub async fn start_service(self: &Arc<Self>) {
        let aself = self.clone();
        let mut shutdown = aself.shutdown_sender.subscribe();

        let gpa_self = Arc::clone(&aself);
        let gpa_t = tokio::spawn(async move {
            gpa_self.get_all_program_accounts_replay().await;
        });

        let gma_self = Arc::clone(&aself);
        let gma_t = tokio::spawn(async move {
            gma_self.get_all_accounts_replay().await;
        });

        tokio::select! {
            gma_res = gma_t => {
                match gma_res {
                    Ok(_) => {
                        warn!("[CAS] Task for get multiple accounts RPC call unexpectedly stopped without error.");
                    },
                    Err(e) => {
                        warn!("[CAS] Task for get multiple accounts RPC call unexpectedly stopped: {}", e);
                    }
                }
            },
            gpa_res = gpa_t => {
                match gpa_res {
                    Ok(_) => {
                        warn!("[CAS] Task for get program accounts accounts RPC call unexpectedly stopped without error.");
                    },
                    Err(e) => {
                        warn!("[CAS] Task for get program accounts RPC call unexpectedly stopped: {}", e);
                    }
                }
            },
            _ = shutdown.recv() => {
                info!("[CAS] Received shutdown signal, stopping.");
            }
        }
    }
}

use std::{sync::Arc, time::Duration};
use cypher::{
    states::{CypherGroup, CypherUser},
    constants::QUOTE_TOKEN_IDX
};
use cypher_math::Number;
use log::{info, warn};
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    client_error::ClientError,
    rpc_config::{
        RpcProgramAccountsConfig, RpcAccountInfoConfig, RpcTransactionConfig
    },
    rpc_filter::{
        RpcFilterType, Memcmp, MemcmpEncodedBytes, MemcmpEncoding
    }
};
use solana_sdk::{
    pubkey::Pubkey,
    commitment_config::CommitmentConfig,
    hash::Hash,
    signature::Keypair,
    transaction::Transaction,
    instruction::Instruction
};
use dashmap::DashMap;
use solana_transaction_status::UiTransactionEncoding;
use tokio::sync::RwLock;

use crate::{
    utils::{get_zero_copy_account, get_liquidate_market_collateral_ixs, get_liquidate_collateral_ixs},
    config::{
        cypher_config::CypherConfig,
        liquidator_config::LiquidatorConfig
    }, 
    fast_tx_builder::FastTxnBuilder, 
};

pub struct Liquidator {
    client: Arc<RpcClient>,
    liquidator_config: Arc<LiquidatorConfig>,
    cypher_config: Arc<CypherConfig>,
    cypher_group_pubkey: Pubkey,
    cypher_group: RwLock<Option<CypherGroup>>,
    cypher_users: RwLock<DashMap<Pubkey, CypherUser>>,
    cypher_liqor: RwLock<Option<CypherUser>>,
    latest_blockhash: RwLock<Hash>,
    latest_slot: RwLock<u64>,
    cypher_liqor_pubkey: Pubkey,
    keypair: Keypair
}

impl Liquidator {
    pub fn new(
        client: Arc<RpcClient>,
        liquidator_config: Arc<LiquidatorConfig>,
        cypher_config: Arc<CypherConfig>,
        cypher_group_pubkey: Pubkey,
        cypher_liqor_pubkey: Pubkey,
        keypair: Keypair,
    ) -> Self {
        Self {
            client,
            liquidator_config,
            cypher_config,
            cypher_group_pubkey,
            keypair,
            cypher_liqor_pubkey,
            cypher_liqor: RwLock::new(None),
            cypher_group: RwLock::new(None),
            cypher_users: RwLock::new(DashMap::new()),
            latest_blockhash: RwLock::new(Hash::default()),
            latest_slot: RwLock::new(u64::default()),
        }
    }

    pub async fn start(
        self: &Arc<Self>
    ) -> Result<(), LiquidatorError> {
        let aself = self.clone();

        let group_self = Arc::clone(&aself);
        let get_group_t = tokio::spawn(
            async move {
                group_self.get_cypher_group_and_liqor_replay().await;
            }
        );

        let users_self = Arc::clone(&aself);
        let get_users_t = tokio::spawn(
            async move {
                users_self.get_all_accounts_replay().await;
            }
        );

        let meta_self = Arc::clone(&aself);
        let get_meta_t = tokio::spawn(
            async move {
                meta_self.get_chain_meta_replay().await;
            }
        );

        self.run().await;

        let (
            group_res,
            users_res,
            meta_res
        ) = tokio::join!(get_group_t, get_users_t, get_meta_t);

        match group_res {
            Ok(_) => (),
            Err(e) => {
                warn!("There was an error joining with the task that fetches the cypher group: {}", e.to_string());
            },
        };

        match users_res {
            Ok(_) => (),
            Err(e) => {
                warn!("There was an error joining with the task that fetches the cypher users: {}", e.to_string());
            },
        };

        match meta_res {
            Ok(_) => (),
            Err(e) => {
                warn!("There was an error joining with the task that fetches the chain meta: {}", e.to_string());
            },
        };

        Ok(())
    }

    async fn run(
        self: &Arc<Self>
    ) {
        loop {
            let maybe_group = self.cypher_group.read().await;
            if maybe_group.is_none() {
                tokio::time::sleep(Duration::from_millis(5000)).await;
                continue;
            }
            let cypher_group = maybe_group.unwrap();

            let map = self.cypher_users.read().await;

            for entry in map.iter() {
                let cypher_user_pubkey = entry.key();
                let cypher_user = entry.value();

                // attempt to liquidate collateral
                
                let (can_liq, token_idx) = self.check_collateral(&cypher_group, cypher_user, cypher_user_pubkey);
                if can_liq {
                    info!("Cypher User: {} - Collateral is liquidatable.", cypher_user_pubkey);

                    if self.check_can_liquidate(&cypher_group).await {
                        info!("Cypher User: {} - Attempting to liquidate.", cypher_user_pubkey);
                        
                        let cypher_liqor = self.cypher_liqor.read().await.unwrap();

                        // the asset mint is the mint of the asset we are using from the liquidator cypher account
                        let asset_mint_idx = self.get_liquidator_asset_mint(&cypher_group, &cypher_liqor, &self.cypher_liqor_pubkey);
                        let asset_mint = cypher_group.get_cypher_token(asset_mint_idx).mint;

                        // the liability mint is the mint of the asset we are trying to liquidate from the cypher user account
                        let liab_mint = cypher_group.get_cypher_token(token_idx).mint;

                        let liq_ixs = get_liquidate_collateral_ixs(
                            &cypher_group,
                            &asset_mint,
                            &liab_mint,
                            &self.cypher_liqor_pubkey,
                            cypher_user_pubkey,
                            &self.keypair
                        );

                        let res = self.submit_transactions(liq_ixs, *self.latest_blockhash.read().await).await;
                        match res {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to submit transaction: {}", e.to_string());
                            },
                        }
                    }
                }

                // attempt to liquidate market collateral
                let (can_liq, token_idx) = self.check_market_collateral(&cypher_group, cypher_user, cypher_user_pubkey).await;
                if can_liq {
                    let cypher_token = cypher_group.get_cypher_token(token_idx);
                    let cypher_market = cypher_group.get_cypher_market(token_idx);

                    info!("Cypher User: {} - Market collateral is liquidatable.", cypher_user_pubkey);

                    if self.check_can_liquidate(&cypher_group).await {
                        info!("Cypher User: {} - Attempting to liquidate.", cypher_user_pubkey);

                        let liq_ixs = get_liquidate_market_collateral_ixs(
                            &cypher_group,
                            cypher_market,
                            cypher_token,
                            &self.cypher_liqor_pubkey,
                            cypher_user_pubkey,
                            &self.keypair
                        );

                        let res = self.submit_transactions(liq_ixs, *self.latest_blockhash.read().await).await;
                        match res {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to submit transaction: {}", e.to_string());
                            },
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(5000)).await;            
        }
    }

    async fn submit_transactions(
        self: &Arc<Self>,
        ixs: Vec<Instruction>,
        blockhash: Hash,
    ) -> Result<(), ClientError> {
        let mut txn_builder = FastTxnBuilder::new();
        let mut submitted: bool = false;
        let mut prev_tx: Transaction = Transaction::default();

        for ix in ixs {

            let tx = txn_builder.build(blockhash, &self.keypair, None);
            // we do this to attempt to pack as many ixs in a tx as possible
            // there's more efficient ways to do it but we'll do it in the future
            if tx.message_data().len() > 1000 {
                let res = self.send_and_confirm_transaction(&prev_tx).await;
                submitted = true;
                match res {
                    Ok(_) => (),
                    Err(e) => {
                        warn!("There was an error submitting transaction and waiting for confirmation: {}", e.to_string());
                        return Err(e);
                    }
                }
            } else {
                txn_builder.add(ix);
                prev_tx = tx;
            }
        }

        if !submitted {
            let tx = txn_builder.build(blockhash, &self.keypair, None);
            let res = self.send_and_confirm_transaction(&tx).await;
            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("There was an error submitting transaction and waiting for confirmation: {}", e.to_string());
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    async fn send_and_confirm_transaction(
        self: &Arc<Self>,
        tx: &Transaction
    ) -> Result<(), ClientError> {
        let submit_res = self.client.send_and_confirm_transaction_with_spinner_and_commitment(
            tx,
            CommitmentConfig::confirmed()
        ).await;

        let signature = match submit_res {
            Ok(s) => {
                info!("Successfully submitted transaction. Transaction signature: {}", s.to_string());
                s
            },
            Err(e) => {
                warn!("There was an error submitting transaction: {}", e.to_string());
                return Err(e);
            }
        };

        loop {
            let confirmed = self.client.get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                    encoding: Some(UiTransactionEncoding::Json),
                    max_supported_transaction_version: Some(0)
                }
            ).await;

            if confirmed.is_err() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            } else {
                break;
            }
        }

        Ok(())
    }

    async fn check_can_liquidate(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
    ) -> bool {
        let maybe_cypher_liqor = self.cypher_liqor.read().await;
        if maybe_cypher_liqor.is_none() {
            return false;
        }
        let cypher_liqor = maybe_cypher_liqor.unwrap();
        let margin_init_ratio = cypher_group.margin_init_ratio();
        let margin_c_ratio = cypher_liqor.get_margin_c_ratio(cypher_group).unwrap();

        if margin_c_ratio >= margin_init_ratio {
            return true;
        }

        false
    }

    fn get_liquidator_asset_mint(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_liqor: &CypherUser,
        cypher_liqor_pubkey: &Pubkey
    ) -> usize {
        let cypher_group_config = self.cypher_config
                .get_group(self.liquidator_config.cluster.as_str())
                .unwrap();
        let tokens = &cypher_group_config.tokens;
        
        let mut highest_deposit: Number = Number::ZERO;
        let mut highest_deposit_idx = 0;

        for token in tokens {
            let cypher_token = cypher_group.get_cypher_token(token.token_index);
            let maybe_cp = cypher_liqor.get_position(token.token_index);
            let cypher_position = match maybe_cp {
                Ok(cp) => cp,
                Err(_) => {
                    continue;
                },
            };

            let base_deposits = cypher_position.base_deposits();
            let native_deposits = cypher_position.native_deposits(cypher_token);
            let total_deposits = cypher_position.total_deposits(cypher_token);

            info!("Cypher Liquidator: {} - Token {} - Base Deposits {} - Native Deposits {} - Total Deposits {}", cypher_liqor_pubkey, token.symbol, base_deposits, native_deposits, total_deposits);

            if total_deposits > highest_deposit {
                highest_deposit = total_deposits;
                highest_deposit_idx = token.token_index;
            }
        }

        highest_deposit_idx
    }

    fn check_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey
    ) -> (bool, usize) {
        let cypher_group_config = self.cypher_config
                .get_group(self.liquidator_config.cluster.as_str())
                .unwrap();
        let tokens = &cypher_group_config.tokens;
        let margin_maint_ratio = cypher_group.margin_maint_ratio();
  
        let margin_c_ratio = cypher_user.get_margin_c_ratio(cypher_group).unwrap();

        info!("Cypher User: {} - Margin C Ratio: {} - Margin Init Ratio: {}", cypher_user_pubkey, margin_c_ratio, margin_maint_ratio);

        if margin_c_ratio < margin_maint_ratio {
            info!("Cypher User: {} - Margin C Ratio below Margin Init Ratio", cypher_user_pubkey);
            
            let mut highest_borrow: Number = Number::ZERO;
            let mut highest_borrow_idx = 0;

            for token in tokens {
                let cypher_token = cypher_group.get_cypher_token(token.token_index);
                let maybe_cp = cypher_user.get_position(token.token_index);
                let cypher_position = match maybe_cp {
                    Ok(cp) => cp,
                    Err(_) => {
                        continue;
                    },
                };

                let borrows = cypher_position.total_borrows(cypher_token);
                let base_borrows = cypher_position.base_borrows();
                let native_borrows = cypher_position.native_borrows(cypher_token);
                info!("Cypher User: {} - Token {} - CypherPosition - Base Borrows {} - Native Borrows {} - Total Borrows {}", cypher_user_pubkey, token.symbol, base_borrows, native_borrows, borrows);

                if borrows > highest_borrow {
                    highest_borrow = borrows;
                    highest_borrow_idx = token.token_index;
                }
            }

            return (true, highest_borrow_idx);
        }

        (false, usize::default())
    }

    async fn check_market_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey
    ) -> (bool, usize) {
        let cypher_group_config = self.cypher_config
                .get_group(self.liquidator_config.cluster.as_str())
                .unwrap();
        let tokens = &cypher_group_config.tokens;
        let slot = *self.latest_slot.read().await;

        for token in tokens {
            if token.token_index == QUOTE_TOKEN_IDX {
                break;
            }

            let market = cypher_group.get_cypher_market(token.token_index);
            let mint_maint_ratio = market.mint_maint_ratio();

            let maybe_uca = cypher_user.get_c_asset(token.token_index);
            let user_c_asset = match maybe_uca {
                Ok(u) => u,
                Err(_) => {
                    //warn!("Cypher User: {} - Could not get User C Asset.", cypher_user_pubkey);
                    continue;
                },
            };
            let maybe_amcr = user_c_asset.get_mint_c_ratio(market, slot, false);
            let asset_mint_c_ratio = match maybe_amcr {
                Ok(amcr) => amcr,
                Err(_) => {
                    //warn!("Cypher User: {} - Could not get Mint C Ratio for {}", cypher_user_pubkey, market.dex_market);
                    continue;
                },
            };

            info!("Cypher User: {} - Asset Mint C Ratio: {} - Mint Maintenance Ratio: {}", cypher_user_pubkey, asset_mint_c_ratio, mint_maint_ratio);

            if asset_mint_c_ratio < mint_maint_ratio {
                info!("Cypher User: {} - Margin C Ratio below Margin Init Ratio", cypher_user_pubkey);
                
                return (true, token.token_index);
            }

        }

        (false, usize::default())
    }

    async fn get_all_accounts(
        self: &Arc<Self>
    ) -> Result<(), ClientError> {

        let filters = Some(vec![
            RpcFilterType::Memcmp(Memcmp {
                offset: 8,
                bytes: MemcmpEncodedBytes::Base58(self.cypher_group_pubkey.to_string()),
                encoding: Some(MemcmpEncoding::Binary),
            }),
            RpcFilterType::DataSize(3960),
        ]);

        let accounts_res = self.client.get_program_accounts_with_config(
            &cypher::ID,
            RpcProgramAccountsConfig {
                filters,
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    commitment: Some(CommitmentConfig::default()),
                    ..RpcAccountInfoConfig::default()
                },
                ..RpcProgramAccountsConfig::default()
            },
        ).await;

        let accounts = match accounts_res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch cypher users: {}", e.to_string());
                return Err(e);
            },
        };
        info!("Fetched {} cypher accounts.", accounts.len());

        let map = self.cypher_users.write().await;
        for (pubkey, account) in accounts {
            let cypher_user = get_zero_copy_account::<CypherUser>(&account);
            map.insert(pubkey, *cypher_user);
        }

        Ok(())
    }

    async fn get_all_accounts_replay(
        self: &Arc<Self>
    ) {
        loop {
            let res = self.get_all_accounts().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("There was an error getting all cypher user accounts: {}", e.to_string());
                },
            };
            
            tokio::time::sleep(Duration::from_millis(15000)).await;            
        }
    }

    async fn get_cypher_group_and_liqor(
        self: &Arc<Self>
    ) -> Result<(), ClientError> {
        let res = self.client.get_account_with_commitment(
            &self.cypher_group_pubkey,
            CommitmentConfig::confirmed()
        ).await;

        let account_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch cypher group account: {}", e.to_string());
                return Err(e);
            },
        };

        let account = match account_res.value {
            Some(a) => a,
            None => {
                warn!("Could not get cypher group account info from request.");
                return Ok(());
            }
        };
        info!("Fetched cypher group.");
        
        let cypher_group = get_zero_copy_account::<CypherGroup>(&account);

        *self.cypher_group.write().await = Some(*cypher_group);

        let res = self.client.get_account_with_commitment(
            &self.cypher_liqor_pubkey,
            CommitmentConfig::confirmed()
        ).await;

        let account_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch cypher liquidator account: {}", e.to_string());
                return Err(e);
            },
        };

        let account = match account_res.value {
            Some(a) => a,
            None => {
                warn!("Could not get cypher liquidator account info from request.");
                return Ok(());
            }
        };
        info!("Fetched cypher liquidator account.");
        
        let cypher_liqor = get_zero_copy_account::<CypherUser>(&account);

        *self.cypher_liqor.write().await = Some(*cypher_liqor);

        Ok(())
    }

    async fn get_cypher_group_and_liqor_replay(
        self: &Arc<Self>
    ) {
        loop {
            let res = self.get_cypher_group_and_liqor().await;
            
            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("There was an error getting the cypher group and cypher liquidator user accounts: {}", e.to_string());
                },
            };
            
            tokio::time::sleep(Duration::from_millis(5000)).await;
        }
    }
    
    async fn get_chain_meta(
        self: &Arc<Self>
    ) -> Result<(), ClientError> {
        let res = self.client.get_latest_blockhash_with_commitment(
            CommitmentConfig::confirmed()
        ).await;

        let hash_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch latest blockhash: {}", e.to_string());
                return Err(e);
            },
        };

        let (hash, _slot) = hash_res;
        info!("Fetched blockhash: {}", &hash.to_string());
        *self.latest_blockhash.write().await = hash;

        let slot_res = self.client.get_slot_with_commitment(
            CommitmentConfig::confirmed()
        ).await;

        let slot = match slot_res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch latest slot: {}", e.to_string());
                return Err(e);
            },
        };
        info!("Fetched recent slot: {}", slot);
        
        *self.latest_slot.write().await = slot;        

        Ok(())
    }
    
    async fn get_chain_meta_replay(
        self: &Arc<Self>
    ) {
        loop {
            let res = self.get_chain_meta().await;
            
            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("There was an error getting the chain meta data: {}", e.to_string());
                },
            };
            
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum LiquidatorError {

}
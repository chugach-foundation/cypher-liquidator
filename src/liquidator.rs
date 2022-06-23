use cypher::{
    constants::QUOTE_TOKEN_IDX,
    quote_mint,
    states::{CypherGroup, CypherMarket, CypherUser},
};
use dashmap::DashMap;
use jet_proto_math::Number;
use log::{info, warn};
use serum_dex::instruction::CancelOrderInstructionV2;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    client_error::ClientError,
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcTransactionConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, MemcmpEncoding, RpcFilterType},
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::{self, ComputeBudgetInstruction},
    hash::Hash,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::Keypair,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::{cmp::min, sync::Arc, time::Duration};
use tokio::sync::RwLock;

use crate::{
    config::{cypher_config::CypherConfig, liquidator_config::LiquidatorConfig},
    fast_tx_builder::FastTxnBuilder,
    utils::{
        derive_open_orders_address, get_cancel_order_ix, get_liquidate_collateral_ixs,
        get_liquidate_market_collateral_ixs, get_open_orders, get_serum_market,
        get_serum_open_orders, get_settle_funds_ix, get_zero_copy_account, OpenOrder,
    },
};

#[derive(Default)]
struct LiquidationCheck {
    can_liquidate: bool,
    open_orders: bool,
    unsettled_funds: bool,
    open_orders_pubkey: Pubkey,
    market_index: usize,
    orders: Vec<OpenOrder>,
}

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
    keypair: Keypair,
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

    pub async fn start(self: &Arc<Self>) -> Result<(), LiquidatorError> {
        let aself = self.clone();

        let group_self = Arc::clone(&aself);
        let get_group_t = tokio::spawn(async move {
            group_self.get_cypher_group_and_liqor_replay().await;
        });

        let users_self = Arc::clone(&aself);
        let get_users_t = tokio::spawn(async move {
            users_self.get_all_accounts_replay().await;
        });

        let meta_self = Arc::clone(&aself);
        let get_meta_t = tokio::spawn(async move {
            meta_self.get_chain_meta_replay().await;
        });

        self.run().await;

        let (group_res, users_res, meta_res) = tokio::join!(get_group_t, get_users_t, get_meta_t);

        match group_res {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    "There was an error joining with the task that fetches the cypher group: {}",
                    e.to_string()
                );
            }
        };

        match users_res {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    "There was an error joining with the task that fetches the cypher users: {}",
                    e.to_string()
                );
            }
        };

        match meta_res {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    "There was an error joining with the task that fetches the chain meta: {}",
                    e.to_string()
                );
            }
        };

        Ok(())
    }

    async fn run(self: &Arc<Self>) {
        loop {
            let maybe_group = self.cypher_group.read().await;
            if maybe_group.is_none() {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                continue;
            }
            let cypher_group = maybe_group.unwrap();

            let map = self.cypher_users.read().await;

            for entry in map.iter() {
                let cypher_user_pubkey = entry.key();
                let cypher_user = *entry.value();

                // attempt to liquidate collateral

                let check = self
                    .check_collateral(&cypher_group, &cypher_user, cypher_user_pubkey)
                    .await;
                if check.can_liquidate {
                    info!(
                        "Cypher User: {} - Collateral is liquidatable.",
                        cypher_user_pubkey
                    );

                    if check.open_orders {
                        let cypher_market = cypher_group.get_cypher_market(check.market_index);
                        let res = self
                            .cancel_user_orders(
                                &cypher_group,
                                cypher_market,
                                &cypher_user,
                                cypher_user_pubkey,
                                &check,
                            )
                            .await;
                        match res {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to submit transaction: {}", e.to_string());
                            }
                        }
                        continue;
                    }

                    if check.unsettled_funds {
                        // TODO: optimize to settle multiple accounts at once
                        // settle user funds before attempting to liquidate
                        let cypher_market = cypher_group.get_cypher_market(check.market_index);
                        let res = self
                            .settle_user_funds(
                                &cypher_group,
                                cypher_market,
                                &cypher_user,
                                cypher_user_pubkey,
                                &check,
                            )
                            .await;
                        match res {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to submit transaction: {}", e.to_string());
                            }
                        }
                        continue;
                    }

                    if self.check_can_liquidate(&cypher_group).await {
                        let maybe_liqor = self.cypher_liqor.read().await;
                        if maybe_liqor.is_none() {
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                            continue;
                        }
                        let cypher_liqor = maybe_liqor.unwrap();

                        let (liqor_can_liq, asset_mint, liab_mint) = self.get_asset_and_liab_mint(
                            &cypher_group,
                            &cypher_liqor,
                            &cypher_user,
                        );

                        if liqor_can_liq {
                            info!(
                                "Cypher User: {} - Attempting to liquidate.",
                                cypher_user_pubkey
                            );

                            info!("Cypher User: {} - Asset Mint: {} - Liab Mint: {} - Attempting to liquidate.", cypher_user_pubkey, asset_mint, liab_mint);

                            let (
                                repay_amount,
                                liqee_asset_debit,
                                market_insurance_debit,
                                global_insurance_credit,
                                global_insurance_debit,
                            ) = self.simulate_liquidate_collateral(
                                &cypher_group,
                                &cypher_liqor,
                                &cypher_user,
                                asset_mint,
                                liab_mint,
                            );

                            if repay_amount == 0
                                && liqee_asset_debit == 0
                                && market_insurance_debit == 0
                                && global_insurance_credit == 0
                                && global_insurance_debit == 0
                            {
                                continue;
                            }

                            let liq_ixs = get_liquidate_collateral_ixs(
                                &cypher_group,
                                &asset_mint,
                                &liab_mint,
                                &self.cypher_liqor_pubkey,
                                cypher_user_pubkey,
                                &self.keypair,
                            );

                            let res = self
                                .submit_transactions(liq_ixs, *self.latest_blockhash.read().await)
                                .await;
                            match res {
                                Ok(_) => (),
                                Err(e) => {
                                    warn!("Failed to submit transaction: {}", e.to_string());
                                }
                            }
                        }
                    }
                }

                // attempt to liquidate market collateral
                let (can_liq, token_idx) = self
                    .check_market_collateral(&cypher_group, &cypher_user, cypher_user_pubkey)
                    .await;
                if can_liq {
                    info!(
                        "Cypher User: {} - Market collateral is liquidatable.",
                        cypher_user_pubkey
                    );

                    let cypher_token = cypher_group.get_cypher_token(token_idx);
                    let cypher_market = cypher_group.get_cypher_market(token_idx);

                    if self.check_can_liquidate(&cypher_group).await {
                        info!(
                            "Cypher User: {} - Attempting to liquidate.",
                            cypher_user_pubkey
                        );

                        let cypher_group_config = self
                            .cypher_config
                            .get_group(self.liquidator_config.cluster.as_str())
                            .unwrap();
                        let cypher_mkt =
                            cypher_group_config.get_market_by_index(token_idx).unwrap();
                        let cypher_tok = cypher_group_config.get_token_by_index(token_idx).unwrap();
                        info!("Cypher User: {} - Cypher Market: {} - Cypher Token: {} - C Asset Mint: {} - Dex Market: {} - Attempting to liquidate.", cypher_user_pubkey, cypher_mkt.name, cypher_tok.symbol, cypher_token.mint, cypher_market.dex_market);

                        let liq_ixs = get_liquidate_market_collateral_ixs(
                            &cypher_group,
                            cypher_market,
                            cypher_token,
                            &self.cypher_liqor_pubkey,
                            cypher_user_pubkey,
                            &self.keypair,
                        );

                        let res = self
                            .submit_transactions(liq_ixs, *self.latest_blockhash.read().await)
                            .await;
                        match res {
                            Ok(_) => (),
                            Err(e) => {
                                warn!("Failed to submit transaction: {}", e.to_string());
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(5000)).await;
        }
    }

    fn get_asset_and_liab_mint(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        liqor_user: &CypherUser,
        liqee_user: &CypherUser,
    ) -> (bool, Pubkey, Pubkey) {
        let cypher_group_config = self
            .cypher_config
            .get_group(self.liquidator_config.cluster.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let mut highest_borrow: Number = Number::ZERO;
        let mut highest_deposit: Number = Number::ZERO;
        let mut asset_mint: Pubkey = Pubkey::default();
        let mut liab_mint: Pubkey = Pubkey::default();

        for token in tokens {
            let cypher_token = cypher_group.get_cypher_token(token.token_index);
            let maybe_liqee_cp = liqee_user.get_position(token.token_index);
            let (liqee_borrows, liqee_deposits) = match maybe_liqee_cp {
                Ok(cp) => (
                    cp.total_borrows(cypher_token),
                    cp.total_deposits(cypher_token),
                ),
                Err(_) => (Number::ZERO, Number::ZERO),
            };

            let maybe_liqor_cp = liqor_user.get_position(token.token_index);
            let (_, liqor_deposits) = match maybe_liqor_cp {
                Ok(cp) => (
                    cp.total_borrows(cypher_token),
                    cp.total_deposits(cypher_token),
                ),
                Err(_) => (Number::ZERO, Number::ZERO),
            };

            if liqee_borrows >= Number::ZERO
                && liqee_borrows >= highest_borrow
                && liqor_deposits >= liqee_borrows
            {
                highest_borrow = liqee_borrows;
                liab_mint = cypher_token.mint;
            }

            if liqee_deposits >= Number::ZERO && liqee_deposits >= highest_deposit {
                highest_deposit = liqee_deposits;
                asset_mint = cypher_token.mint;
            }
        }

        if asset_mint != Pubkey::default() && liab_mint != Pubkey::default() {
            return (true, asset_mint, liab_mint);
        }

        (false, Pubkey::default(), Pubkey::default())
    }

    async fn submit_transactions(
        self: &Arc<Self>,
        ixs: Vec<Instruction>,
        blockhash: Hash,
    ) -> Result<(), ClientError> {
        let mut txn_builder = FastTxnBuilder::new();
        let mut submitted: bool = false;
        let mut prev_tx: Transaction = Transaction::default();

        txn_builder.add(Instruction::new_with_borsh(
            compute_budget::id(),
            &ComputeBudgetInstruction::RequestUnitsDeprecated {
                units: 500_000_u32,
                additional_fee: 5_000_u32,
            },
            vec![],
        ));

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
        tx: &Transaction,
    ) -> Result<(), ClientError> {
        let submit_res = self
            .client
            .send_and_confirm_transaction_with_spinner_and_commitment(
                tx,
                CommitmentConfig::confirmed(),
            )
            .await;

        let signature = match submit_res {
            Ok(s) => {
                info!(
                    "Successfully submitted transaction. Transaction signature: {}",
                    s.to_string()
                );
                s
            }
            Err(e) => {
                warn!(
                    "There was an error submitting transaction: {}",
                    e.to_string()
                );
                return Err(e);
            }
        };

        loop {
            let confirmed = self
                .client
                .get_transaction_with_config(
                    &signature,
                    RpcTransactionConfig {
                        commitment: Some(CommitmentConfig::confirmed()),
                        encoding: Some(UiTransactionEncoding::Json),
                        max_supported_transaction_version: Some(0),
                    },
                )
                .await;

            if confirmed.is_err() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            } else {
                break;
            }
        }

        Ok(())
    }

    async fn check_can_liquidate(self: &Arc<Self>, cypher_group: &CypherGroup) -> bool {
        let cypher_group_config = self
            .cypher_config
            .get_group(self.liquidator_config.cluster.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let maybe_cypher_liqor = self.cypher_liqor.read().await;
        if maybe_cypher_liqor.is_none() {
            return false;
        }
        let cypher_liqor = maybe_cypher_liqor.unwrap();
        let margin_init_ratio = cypher_group.margin_init_ratio();
        let margin_c_ratio = cypher_liqor.get_margin_c_ratio(cypher_group).unwrap();

        info!(
            "CYPHER LIQUIDATOR: {} - Margin C Ratio: {} - Margin Init Ratio: {}",
            &self.cypher_liqor_pubkey, margin_c_ratio, margin_init_ratio
        );

        for token in tokens {
            let cypher_token = cypher_group.get_cypher_token(token.token_index);
            let maybe_cp = cypher_liqor.get_position(token.token_index);
            let cypher_position = match maybe_cp {
                Ok(cp) => cp,
                Err(_) => {
                    continue;
                }
            };

            let borrows = cypher_position.total_borrows(cypher_token);
            let base_borrows = cypher_position.base_borrows();
            let native_borrows = cypher_position.native_borrows(cypher_token);
            info!("CYPHER LIQUIDATOR: {} - Token {} - CypherPosition - Base Borrows {} - Native Borrows {} - Total Borrows {}", self.cypher_liqor_pubkey, token.symbol, base_borrows, native_borrows, borrows);

            let base_deposits = cypher_position.base_deposits();
            let native_deposits = cypher_position.native_deposits(cypher_token);
            let total_deposits = cypher_position.total_deposits(cypher_token);
            info!("CYPHER LIQUIDATOR: {} - Token {} - CypherPosition - Base Deposits {} - Native Deposits {} - Total Deposits {}", self.cypher_liqor_pubkey, token.symbol, base_deposits, native_deposits, total_deposits);

            if token.token_index != QUOTE_TOKEN_IDX {
                let maybe_ca = cypher_liqor.get_c_asset(token.token_index);
                let cypher_asset = match maybe_ca {
                    Ok(ca) => ca,
                    Err(_) => {
                        continue;
                    }
                };

                info!("CYPHER LIQUIDATOR: {} - Token {} - CypherAsset -  Mint Collateral {} - Debt Shares {}", self.cypher_liqor_pubkey, token.symbol, cypher_asset.collateral, cypher_asset.debt_shares);
            };
        }

        if margin_c_ratio >= margin_init_ratio {
            return true;
        }

        false
    }

    async fn check_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
    ) -> LiquidationCheck {
        let cypher_group_config = self
            .cypher_config
            .get_group(self.liquidator_config.cluster.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let margin_maint_ratio = cypher_group.margin_maint_ratio();

        let margin_c_ratio = cypher_user.get_margin_c_ratio(cypher_group).unwrap();

        if margin_c_ratio < 2_u64.into() {
            info!(
                "Cypher User: {} - Margin C Ratio: {} - Margin Maintenance Ratio: {}",
                cypher_user_pubkey, margin_c_ratio, margin_maint_ratio
            );
        }

        if margin_c_ratio < margin_maint_ratio {
            info!(
                "Cypher User: {} - Margin C Ratio below Margin Maintenance Ratio",
                cypher_user_pubkey
            );

            if cypher_user.is_bankrupt(cypher_group).unwrap() {
                info!("Cypher User: {} - USER IS BANKRUPT", cypher_user_pubkey);
            }

            let quote_position = cypher_user.get_position(QUOTE_TOKEN_IDX).unwrap();
            let has_quote_borrows = quote_position.base_borrows() > Number::ZERO;

            for asset in cypher_user.iter_c_assets() {
                if asset.oo_info.is_account_open {
                    let market_idx = asset.market_idx as usize;
                    let cypher_market = cypher_group.get_cypher_market(market_idx);
                    let cypher_token = cypher_group.get_cypher_token(market_idx);
                    let maybe_cp = cypher_user.get_position(market_idx);
                    let cypher_position = match maybe_cp {
                        Ok(cp) => cp,
                        Err(_) => {
                            continue;
                        }
                    };
                    let open_orders_pubkey =
                        derive_open_orders_address(&cypher_market.dex_market, cypher_user_pubkey);
                    let maybe_ooa =
                        get_serum_open_orders(Arc::clone(&self.client), &open_orders_pubkey).await;
                    let open_orders_account = match maybe_ooa {
                        Ok(o) => o,
                        Err(e) => {
                            warn!(
                                "An error occurred while fetching open orders account: {}",
                                e.to_string()
                            );
                            return LiquidationCheck {
                                can_liquidate: false,
                                open_orders: false,
                                unsettled_funds: false,
                                ..Default::default()
                            };
                        }
                    };
                    let open_orders = get_open_orders(&open_orders_account);

                    if cypher_position.base_borrows() > Number::ZERO
                        && asset.oo_info.coin_total != 0
                    {
                        info!("Cypher User: {} - User has token borrows and coin unsettled funds. Asset: {} - Coin Total: {} - Must cancel orders and settle funds before liquidating.", cypher_user_pubkey, cypher_token.mint, asset.oo_info.coin_total);

                        return LiquidationCheck {
                            can_liquidate: true,
                            open_orders: !open_orders.is_empty(),
                            unsettled_funds: true,
                            open_orders_pubkey,
                            market_index: market_idx,
                            orders: open_orders,
                        };
                    }

                    if has_quote_borrows && asset.oo_info.pc_total != 0 {
                        info!("Cypher User: {} - User has quote borrows and price coin funds. Asset: {} - Price Coin Total: {} - Must cancel orders and settle funds before liquidating.", cypher_user_pubkey, quote_mint::id(), asset.oo_info.pc_total);

                        return LiquidationCheck {
                            can_liquidate: true,
                            open_orders: !open_orders.is_empty(),
                            unsettled_funds: true,
                            open_orders_pubkey,
                            market_index: market_idx,
                            orders: open_orders,
                        };
                    }
                }
            }

            for token in tokens {
                let cypher_token = cypher_group.get_cypher_token(token.token_index);
                let maybe_cp = cypher_user.get_position(token.token_index);
                let cypher_position = match maybe_cp {
                    Ok(cp) => cp,
                    Err(_) => {
                        continue;
                    }
                };

                let borrows = cypher_position.total_borrows(cypher_token);
                let base_borrows = cypher_position.base_borrows();
                let native_borrows = cypher_position.native_borrows(cypher_token);
                info!("Cypher User: {} - Token {} - CypherPosition - Base Borrows {} - Native Borrows {} - Total Borrows {}", cypher_user_pubkey, token.symbol, base_borrows, native_borrows, borrows);

                let base_deposits = cypher_position.base_deposits();
                let native_deposits = cypher_position.native_deposits(cypher_token);
                let total_deposits = cypher_position.total_deposits(cypher_token);
                info!("Cypher User: {} - Token {} - CypherPosition - Base Deposits {} - Native Deposits {} - Total Deposits {}", cypher_user_pubkey, token.symbol, base_deposits, native_deposits, total_deposits);

                if token.token_index != QUOTE_TOKEN_IDX {
                    let maybe_ca = cypher_user.get_c_asset(token.token_index);
                    let cypher_asset = match maybe_ca {
                        Ok(ca) => ca,
                        Err(_) => {
                            continue;
                        }
                    };

                    info!("Cypher User: {} - Token {} - CypherAsset -  Mint Collateral {} - Debt Shares {}", cypher_user_pubkey, token.symbol, cypher_asset.collateral, cypher_asset.debt_shares);
                };
            }

            return LiquidationCheck {
                can_liquidate: true,
                open_orders: false,
                unsettled_funds: false,
                ..Default::default()
            };
        }

        LiquidationCheck::default()
    }

    async fn settle_user_funds(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_market: &CypherMarket,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
        check: &LiquidationCheck,
    ) -> Result<(), ClientError> {
        let dex_market_state =
            get_serum_market(Arc::clone(&self.client), &cypher_market.dex_market)
                .await
                .unwrap();

        let settle_ix = get_settle_funds_ix(
            cypher_group,
            &dex_market_state,
            cypher_user_pubkey,
            &check.open_orders_pubkey,
            &cypher_user.user_signer,
            check.market_index,
        );
        let ixs = vec![settle_ix];

        self.submit_transactions(ixs, *self.latest_blockhash.read().await)
            .await
    }

    async fn cancel_user_orders(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_market: &CypherMarket,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
        check: &LiquidationCheck,
    ) -> Result<(), ClientError> {
        let dex_market_state =
            get_serum_market(Arc::clone(&self.client), &cypher_market.dex_market)
                .await
                .unwrap();

        let cypher_token = cypher_group.get_cypher_token(check.market_index);

        let mut ixs = Vec::new();

        for order in &check.orders {
            let cancel_ix = get_cancel_order_ix(
                cypher_group,
                cypher_market,
                cypher_token,
                &dex_market_state,
                &check.open_orders_pubkey,
                cypher_user_pubkey,
                &cypher_user.user_signer,
                CancelOrderInstructionV2 {
                    order_id: order.order_id,
                    side: order.side,
                },
            );
            ixs.push(cancel_ix);
        }

        self.submit_transactions(ixs, *self.latest_blockhash.read().await)
            .await
    }

    fn simulate_liquidate_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_liqor_user: &CypherUser,
        cypher_liqee_user: &CypherUser,
        asset_mint: Pubkey,
        liab_mint: Pubkey,
    ) -> (u64, u64, u64, u64, u64) {
        self.calc_collateral_liquidation(
            cypher_group,
            cypher_liqor_user,
            cypher_liqee_user,
            asset_mint,
            liab_mint,
        )
    }

    fn get_token_info(self: &Arc<Self>, group: &CypherGroup, token_mint: Pubkey) -> (usize, u64) {
        if token_mint == quote_mint::ID {
            return (QUOTE_TOKEN_IDX, 1_u64);
        }
        let token_idx = group.get_token_idx(token_mint).unwrap();
        let market = group.get_cypher_market(token_idx);
        (token_idx, market.market_price)
    }

    fn calc_collateral_liquidation(
        self: &Arc<Self>,
        group: &CypherGroup,
        liqor_user: &CypherUser,
        liqee_user: &CypherUser,
        asset_mint: Pubkey,
        liab_mint: Pubkey,
    ) -> (u64, u64, u64, u64, u64) {
        let (asset_token_idx, asset_price) = self.get_token_info(group, asset_mint);
        let (liab_token_idx, liab_price) = self.get_token_info(group, liab_mint);

        let target_ratio = group.margin_init_ratio();
        let liqor_fee = group.liq_liqor_fee();
        let insurance_fee = group.liq_insurance_fee();
        let excess_liabs_value = {
            let assets_value = liqee_user.get_assets_value(group).unwrap();
            let liabs_value = liqee_user.get_liabs_value(group).unwrap();
            (liabs_value * target_ratio - assets_value) / (target_ratio - liqor_fee - insurance_fee)
        };
        let loan_value_in_position = liqee_user
            .get_position(liab_token_idx)
            .unwrap()
            .total_borrows(group.get_cypher_token(liab_token_idx))
            * liab_price;
        let max_repay_value = min(excess_liabs_value, loan_value_in_position);

        let is_bankrupt = liqee_user.is_bankrupt(group).unwrap();
        if is_bankrupt {
            assert_eq!(asset_mint, quote_mint::ID);
            if liab_mint == quote_mint::ID {
                let repay_amount = min(max_repay_value.as_u64(0), group.insurance_fund);
                return (repay_amount, 0, 0, 0, repay_amount);
            }
        }

        let max_value_for_swap = if is_bankrupt {
            let liab_market = group.get_cypher_market(liab_token_idx);
            Number::from(liab_market.insurance_fund) / liqor_fee
        } else {
            let asset_position_value = liqee_user
                .get_position(asset_token_idx)
                .unwrap()
                .total_deposits(group.get_cypher_token(asset_token_idx))
                .as_u64(0)
                * asset_price;
            Number::from(asset_position_value) / (liqor_fee + insurance_fee)
        };
        let liqor_repay_position_value = liqor_user
            .get_position(liab_token_idx)
            .unwrap()
            .total_deposits(group.get_cypher_token(liab_token_idx))
            * liab_price;
        let max_liab_swap_value = min(liqor_repay_position_value, max_value_for_swap);

        let repay_value = min(max_repay_value, max_liab_swap_value);
        let repay_amount = (repay_value / liab_price).as_u64(0);
        let repay_value = Number::from(repay_amount * liab_price);
        let liqor_credit_value = repay_value * liqor_fee;
        let (liqee_asset_debit, market_insurance_debit, global_insurance_credit) = if is_bankrupt {
            if liab_mint == quote_mint::ID {
                unreachable!()
            } else {
                (0, liqor_credit_value.as_u64(0), 0)
            }
        } else {
            let global_insurance_credit_value = repay_value * insurance_fee;
            let liqee_debit_value = liqor_credit_value + global_insurance_credit_value;
            let liqee_asset_debit = (liqee_debit_value / asset_price).as_u64_ceil(0);
            (
                liqee_asset_debit,
                0,
                global_insurance_credit_value.as_u64(0),
            )
        };

        let global_insurance_debit = 0;
        (
            repay_amount,
            liqee_asset_debit,
            market_insurance_debit,
            global_insurance_credit,
            global_insurance_debit,
        )
    }

    async fn check_market_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
    ) -> (bool, usize) {
        let cypher_group_config = self
            .cypher_config
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
                }
            };
            let maybe_amcr = user_c_asset.get_mint_c_ratio(market, slot, true);
            let asset_mint_c_ratio = match maybe_amcr {
                Ok(amcr) => amcr,
                Err(_) => {
                    //warn!("Cypher User: {} - Could not get Mint C Ratio for {}", cypher_user_pubkey, market.dex_market);
                    continue;
                }
            };

            if asset_mint_c_ratio < 2_u64.into() {
                info!(
                    "Cypher User: {} - Asset Mint C Ratio: {} - Mint Maintenance Ratio: {}",
                    cypher_user_pubkey, asset_mint_c_ratio, mint_maint_ratio
                );
            }

            if asset_mint_c_ratio < mint_maint_ratio {
                info!(
                    "Cypher User: {} - Margin C Ratio below Mint Maintenance Ratio",
                    cypher_user_pubkey
                );

                if cypher_user.is_bankrupt(cypher_group).unwrap() {
                    info!("Cypher User: {} - USER IS BANKRUPT", cypher_user_pubkey);
                }

                for token in tokens {
                    let cypher_token = cypher_group.get_cypher_token(token.token_index);
                    let maybe_cp = cypher_user.get_position(token.token_index);
                    let cypher_position = match maybe_cp {
                        Ok(cp) => cp,
                        Err(_) => {
                            continue;
                        }
                    };

                    let borrows = cypher_position.total_borrows(cypher_token);
                    let base_borrows = cypher_position.base_borrows();
                    let native_borrows = cypher_position.native_borrows(cypher_token);
                    info!("Cypher User: {} - Token {} - CypherPosition - Base Borrows {} - Native Borrows {} - Total Borrows {}", cypher_user_pubkey, token.symbol, base_borrows, native_borrows, borrows);

                    let base_deposits = cypher_position.base_deposits();
                    let native_deposits = cypher_position.native_deposits(cypher_token);
                    let total_deposits = cypher_position.total_deposits(cypher_token);
                    info!("Cypher User: {} - Token {} - CypherPosition - Base Deposits {} - Native Deposits {} - Total Deposits {}", cypher_user_pubkey, token.symbol, base_deposits, native_deposits, total_deposits);

                    if token.token_index != QUOTE_TOKEN_IDX {
                        let maybe_ca = cypher_user.get_c_asset(token.token_index);
                        let cypher_asset = match maybe_ca {
                            Ok(ca) => ca,
                            Err(_) => {
                                continue;
                            }
                        };

                        info!("Cypher User: {} - Token {} - CypherAsset -  Mint Collateral {} - Debt Shares {}", cypher_user_pubkey, token.symbol, cypher_asset.collateral, cypher_asset.debt_shares);
                    };
                }

                return (true, token.token_index);
            }
        }

        (false, usize::default())
    }

    async fn get_all_accounts(self: &Arc<Self>) -> Result<(), ClientError> {
        let filters = Some(vec![
            RpcFilterType::Memcmp(Memcmp {
                offset: 8,
                bytes: MemcmpEncodedBytes::Base58(self.cypher_group_pubkey.to_string()),
                encoding: Some(MemcmpEncoding::Binary),
            }),
            RpcFilterType::DataSize(3960),
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
                        ..RpcAccountInfoConfig::default()
                    },
                    ..RpcProgramAccountsConfig::default()
                },
            )
            .await;

        let accounts = match accounts_res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch cypher users: {}", e.to_string());
                return Err(e);
            }
        };
        info!("Fetched {} cypher accounts.", accounts.len());

        let map = self.cypher_users.write().await;
        for (pubkey, account) in accounts {
            let cypher_user = get_zero_copy_account::<CypherUser>(&account);
            map.insert(pubkey, *cypher_user);
        }

        Ok(())
    }

    async fn get_all_accounts_replay(self: &Arc<Self>) {
        loop {
            let res = self.get_all_accounts().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!(
                        "There was an error getting all cypher user accounts: {}",
                        e.to_string()
                    );
                }
            };

            tokio::time::sleep(Duration::from_millis(15000)).await;
        }
    }

    async fn get_cypher_group_and_liqor(self: &Arc<Self>) -> Result<(), ClientError> {
        let res = self
            .client
            .get_account_with_commitment(&self.cypher_group_pubkey, CommitmentConfig::confirmed())
            .await;

        let account_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch cypher group account: {}", e.to_string());
                return Err(e);
            }
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

        let res = self
            .client
            .get_account_with_commitment(&self.cypher_liqor_pubkey, CommitmentConfig::confirmed())
            .await;

        let account_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    "Could not fetch cypher liquidator account: {}",
                    e.to_string()
                );
                return Err(e);
            }
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

    async fn get_cypher_group_and_liqor_replay(self: &Arc<Self>) {
        loop {
            let res = self.get_cypher_group_and_liqor().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("There was an error getting the cypher group and cypher liquidator user accounts: {}", e.to_string());
                }
            };

            tokio::time::sleep(Duration::from_millis(2000)).await;
        }
    }

    async fn get_chain_meta(self: &Arc<Self>) -> Result<(), ClientError> {
        let res = self
            .client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await;

        let hash_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch latest blockhash: {}", e.to_string());
                return Err(e);
            }
        };

        let (hash, _slot) = hash_res;
        info!("Fetched blockhash: {}", &hash.to_string());
        *self.latest_blockhash.write().await = hash;

        let slot_res = self
            .client
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await;

        let slot = match slot_res {
            Ok(a) => a,
            Err(e) => {
                warn!("Could not fetch latest slot: {}", e.to_string());
                return Err(e);
            }
        };
        info!("Fetched recent slot: {}", slot);

        *self.latest_slot.write().await = slot;

        Ok(())
    }

    async fn get_chain_meta_replay(self: &Arc<Self>) {
        loop {
            let res = self.get_chain_meta().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!(
                        "There was an error getting the chain meta data: {}",
                        e.to_string()
                    );
                }
            };

            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum LiquidatorError {}

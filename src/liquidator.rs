use {
    crate::{
        chain_meta_service::ChainMetaService,
        config::{cypher_config::CypherConfig, liquidator_config::LiquidatorConfig},
        cypher_account_service::CypherUserWrapper,
        fast_tx_builder::FastTxnBuilder,
        simulation::simulate_liquidate_collateral,
        utils::{
            get_cancel_order_ix, get_liquidate_collateral_ixs, get_open_orders, get_serum_market,
            get_serum_open_orders, get_settle_funds_ix, OpenOrder,
        },
    },
    cypher::{
        constants::QUOTE_TOKEN_IDX,
        utils::{derive_open_orders_address, get_zero_copy_account},
        CypherGroup, CypherMarket, CypherUser,
    },
    jet_proto_math::Number,
    log::{info, warn},
    serum_dex::instruction::CancelOrderInstructionV2,
    solana_client::{
        client_error::ClientError, nonblocking::rpc_client::RpcClient,
        rpc_config::RpcTransactionConfig,
    },
    solana_sdk::{
        commitment_config::CommitmentConfig, hash::Hash, instruction::Instruction, pubkey::Pubkey,
        signature::Keypair, transaction::Transaction,
    },
    solana_transaction_status::UiTransactionEncoding,
    std::{sync::Arc, time::Duration},
    tokio::sync::{
        broadcast::{Receiver, Sender},
        Mutex, RwLock,
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
    chain_meta_service: Arc<ChainMetaService>,
    receiver: Mutex<Receiver<CypherUserWrapper>>,
    shutdown: Sender<bool>,
    cypher_group_pubkey: Pubkey,
    cypher_group: RwLock<Option<CypherGroup>>,
    cypher_liqor: RwLock<Option<CypherUser>>,
    cypher_liqor_pubkey: Pubkey,
    keypair: Arc<Keypair>,
}

impl Liquidator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<RpcClient>,
        liquidator_config: Arc<LiquidatorConfig>,
        cypher_config: Arc<CypherConfig>,
        chain_meta_service: Arc<ChainMetaService>,
        receiver: Receiver<CypherUserWrapper>,
        shutdown: Sender<bool>,
        cypher_group_pubkey: Pubkey,
        cypher_liqor_pubkey: Pubkey,
        keypair: Arc<Keypair>,
    ) -> Self {
        Self {
            client,
            liquidator_config,
            cypher_config,
            chain_meta_service,
            receiver: Mutex::new(receiver),
            shutdown,
            cypher_group_pubkey,
            keypair,
            cypher_liqor_pubkey,
            cypher_liqor: RwLock::new(None),
            cypher_group: RwLock::new(None),
        }
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), LiquidatorError> {
        let aself = self.clone();

        let group_self = Arc::clone(&aself);
        let get_group_t = tokio::spawn(async move {
            group_self.get_cypher_group_and_liqor_replay().await;
        });

        let mut shutdown = self.shutdown.subscribe();

        tokio::select! {
            _ = self.run() => {},
            _ = shutdown.recv() => {
                info!("[LIQ] Received shutdown signal, stopping liquidator.");
            }
        }

        let (group_res,) = tokio::join!(get_group_t);

        match group_res {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    "[LIQ] There was an error joining with the task that fetches the cypher group: {}",
                    e.to_string()
                );
            }
        };

        Ok(())
    }

    async fn run(self: &Arc<Self>) {
        let mut receiver = self.receiver.lock().await;
        loop {
            if let Ok(user) = receiver.recv().await {
                match self
                    .process(&user.cypher_user, &user.cypher_user_pubkey)
                    .await
                {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(
                            "[LIQ] There was an error processing cypher user: {} - {:?}",
                            user.cypher_user_pubkey, e
                        );
                    }
                }
            }
        }
    }

    async fn process(
        self: &Arc<Self>,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
    ) -> Result<(), LiquidatorError> {
        let maybe_group = self.cypher_group.read().await;
        if maybe_group.is_none() {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            return Err(LiquidatorError::CypherGroupNotFound);
        }
        let cypher_group = maybe_group.unwrap();

        match self
            .process_collateral(&cypher_group, cypher_user, cypher_user_pubkey)
            .await
        {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    "[LIQ] An error occurred when processing cypher user {} collateral: {:?}",
                    cypher_user_pubkey, e
                );
            }
        }

        Ok(())
    }

    async fn process_collateral(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_user: &CypherUser,
        cypher_user_pubkey: &Pubkey,
    ) -> Result<(), LiquidatorError> {
        // attempt to liquidate collateral
        let check = self
            .check_collateral(cypher_group, cypher_user, cypher_user_pubkey)
            .await;
        if check.can_liquidate {
            info!(
                "[LIQ] Liqee: {} - Collateral is liquidatable.",
                cypher_user_pubkey
            );

            if check.open_orders {
                let cypher_market = cypher_group.get_cypher_market(check.market_index).unwrap();
                let res = self
                    .cancel_user_orders(cypher_group, cypher_market, cypher_user_pubkey, &check)
                    .await;
                match res {
                    Ok(_) => (),
                    Err(e) => {
                        warn!("[LIQ] Failed to submit transaction: {}", e.to_string());
                        return Err(LiquidatorError::ErrorSubmittingTransaction(e));
                    }
                }
                return Ok(());
            }

            if check.unsettled_funds {
                // TODO: optimize to settle multiple accounts at once
                // settle user funds before attempting to liquidate
                let cypher_market = cypher_group.get_cypher_market(check.market_index).unwrap();
                let res = self
                    .settle_user_funds(cypher_group, cypher_market, cypher_user_pubkey, &check)
                    .await;
                match res {
                    Ok(_) => (),
                    Err(e) => {
                        warn!("[LIQ] Failed to submit transaction: {}", e.to_string());
                        return Err(LiquidatorError::ErrorSubmittingTransaction(e));
                    }
                }
                return Ok(());
            }

            if self.check_can_liquidate(cypher_group).await {
                let maybe_liqor = self.cypher_liqor.read().await;
                if maybe_liqor.is_none() {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    return Err(LiquidatorError::CypherLiqorNotFound);
                }
                let cypher_liqor = maybe_liqor.unwrap();

                let (liqor_can_liq, asset_mint, liab_mint) =
                    self.get_asset_and_liab_mint(cypher_group, &cypher_liqor, cypher_user);

                if liqor_can_liq {
                    let (
                        repay_amount,
                        liqee_asset_debit,
                        global_insurance_credit,
                        global_insurance_debit,
                    ) = simulate_liquidate_collateral(
                        cypher_group,
                        &cypher_liqor,
                        cypher_user,
                        asset_mint,
                        liab_mint,
                    );

                    if repay_amount == 0
                        && liqee_asset_debit == 0
                        && global_insurance_credit == 0
                        && global_insurance_debit == 0
                    {
                        return Err(LiquidatorError::ZeroedSimulation);
                    }

                    info!("[LIQ] Liqee: {} - Asset Mint: {} - Liab Mint: {} - Attempting to liquidate.", cypher_user_pubkey, asset_mint, liab_mint);

                    let liq_ix = get_liquidate_collateral_ixs(
                        cypher_group,
                        &asset_mint,
                        &liab_mint,
                        &self.cypher_liqor_pubkey,
                        cypher_user_pubkey,
                        &self.keypair,
                    );

                    let blockhash = self.chain_meta_service.get_latest_blockhash().await;

                    let res = self.submit_transactions([liq_ix].to_vec(), blockhash).await;
                    match res {
                        Ok(_) => (),
                        Err(e) => {
                            warn!("[LIQ] Failed to submit transaction: {}", e.to_string());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn get_asset_and_liab_mint(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        liqor_user: &CypherUser,
        liqee_user: &CypherUser,
    ) -> (bool, Pubkey, Pubkey) {
        let cypher_group_config = self
            .cypher_config
            .get_group(self.liquidator_config.group.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let mut highest_borrow: Number = Number::ZERO;
        let mut highest_deposit: Number = Number::ZERO;
        let mut asset_mint: Pubkey = Pubkey::default();
        let mut liab_mint: Pubkey = Pubkey::default();

        for token in tokens {
            let cypher_token = cypher_group.get_cypher_token(token.token_index).unwrap();
            let maybe_liqee_cp = liqee_user.get_position(token.token_index);
            let (liqee_borrows, liqee_deposits) = match maybe_liqee_cp {
                Some(cp) => (
                    cp.total_borrows(cypher_token),
                    cp.total_deposits(cypher_token),
                ),
                None => (Number::ZERO, Number::ZERO),
            };

            let maybe_liqor_cp = liqor_user.get_position(token.token_index);
            let (_, liqor_deposits) = match maybe_liqor_cp {
                Some(cp) => (
                    cp.total_borrows(cypher_token),
                    cp.total_deposits(cypher_token),
                ),
                None => (Number::ZERO, Number::ZERO),
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
                        warn!("[LIQ] There was an error submitting transaction and waiting for confirmation: {}", e.to_string());
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
                    warn!("[LIQ] There was an error submitting transaction and waiting for confirmation: {}", e.to_string());
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
                    "[LIQ] Successfully submitted transaction. Transaction signature: {}",
                    s.to_string()
                );
                s
            }
            Err(e) => {
                warn!(
                    "[LIQ] There was an error submitting transaction: {}",
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
            .get_group(self.liquidator_config.group.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let maybe_cypher_liqor = self.cypher_liqor.read().await;
        if maybe_cypher_liqor.is_none() {
            return false;
        }
        let cypher_liqor = maybe_cypher_liqor.unwrap();
        let margin_init_ratio = cypher_group.margin_init_ratio();
        let margin_c_ratio = cypher_liqor.get_margin_c_ratio(cypher_group);

        info!(
            "[LIQ] LIQOR: {} - Margin C Ratio: {} - Margin Init Ratio: {}",
            &self.cypher_liqor_pubkey, margin_c_ratio, margin_init_ratio
        );

        if self.liquidator_config.log_liqor_health {
            for token in tokens {
                let cypher_token = cypher_group.get_cypher_token(token.token_index).unwrap();
                let maybe_cp = cypher_liqor.get_position(token.token_index);
                let cypher_position = match maybe_cp {
                    Some(cp) => cp,
                    None => {
                        continue;
                    }
                };

                let total_borrows = cypher_position.total_borrows(cypher_token);
                let total_deposits = cypher_position.total_deposits(cypher_token);
                info!(
                    "[LIQ] LIQOR: {} - {} Position - Total Borrows: {} - Total Deposits: {}",
                    self.cypher_liqor_pubkey, token.symbol, total_borrows, total_deposits
                );
            }
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
            .get_group(self.liquidator_config.group.as_str())
            .unwrap();
        let tokens = &cypher_group_config.tokens;
        let margin_maint_ratio = cypher_group.margin_maint_ratio();

        let margin_c_ratio = cypher_user.get_margin_c_ratio(cypher_group);

        if margin_c_ratio < margin_maint_ratio {
            info!(
                "[LIQ] Liqee: {} - Margin C Ratio below Margin Maintenance Ratio. C Ratio: {}",
                cypher_user_pubkey, margin_c_ratio,
            );

            if cypher_user.is_bankrupt(cypher_group) {
                info!("[LIQ] Liqee: {} - USER IS BANKRUPT!", cypher_user_pubkey);
            }

            let quote_position = cypher_user.get_position(QUOTE_TOKEN_IDX).unwrap();
            let has_quote_borrows = quote_position.base_borrows() > Number::ZERO;

            for position in cypher_user.iter_positions() {
                if position.oo_info.is_account_open {
                    let market_idx = position.market_idx as usize;
                    let cypher_market = cypher_group.get_cypher_market(market_idx).unwrap();
                    let cypher_token = cypher_group.get_cypher_token(market_idx).unwrap();
                    let maybe_cp = cypher_user.get_position(market_idx);
                    let cypher_position = match maybe_cp {
                        Some(cp) => cp,
                        None => {
                            continue;
                        }
                    };
                    let open_orders_pubkey =
                        derive_open_orders_address(&cypher_market.dex_market, cypher_user_pubkey).0;
                    let maybe_ooa =
                        get_serum_open_orders(Arc::clone(&self.client), &open_orders_pubkey).await;
                    let open_orders_account = match maybe_ooa {
                        Ok(o) => o,
                        Err(e) => {
                            warn!(
                                "[LIQ] An error occurred while fetching open orders account: {}",
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
                        && cypher_position.oo_info.coin_total != 0
                    {
                        info!("[LIQ] Liqee: {} - User has token borrows and coin unsettled funds. Asset: {} - Coin Total: {} - Must cancel orders and settle funds before liquidating.", cypher_user_pubkey, cypher_token.mint, cypher_position.oo_info.coin_total);

                        return LiquidationCheck {
                            can_liquidate: true,
                            open_orders: !open_orders.is_empty(),
                            unsettled_funds: cypher_position.oo_info.coin_free != 0,
                            open_orders_pubkey,
                            market_index: market_idx,
                            orders: open_orders,
                        };
                    }

                    if has_quote_borrows && cypher_position.oo_info.pc_total != 0 {
                        info!("[LIQ] Liqee: {} - User has quote borrows and price coin funds. Asset: {} - Price Coin Total: {} - Must cancel orders and settle funds before liquidating.", cypher_user_pubkey, cypher::quote_mint::id(), cypher_position.oo_info.pc_total);

                        return LiquidationCheck {
                            can_liquidate: true,
                            open_orders: !open_orders.is_empty(),
                            unsettled_funds: cypher_position.oo_info.pc_free != 0,
                            open_orders_pubkey,
                            market_index: market_idx,
                            orders: open_orders,
                        };
                    }
                }
            }

            if self.liquidator_config.log_liqee_healths {
                for token in tokens {
                    let cypher_token = cypher_group.get_cypher_token(token.token_index).unwrap();
                    let maybe_cp = cypher_user.get_position(token.token_index);
                    let cypher_position = match maybe_cp {
                        Some(cp) => cp,
                        None => {
                            continue;
                        }
                    };

                    let total_borrows = cypher_position.total_borrows(cypher_token);
                    let total_deposits = cypher_position.total_deposits(cypher_token);
                    info!(
                        "[LIQ] Liqee: {} - {} Position - Total Borrows: {} - Total Deposits: {}",
                        cypher_user_pubkey, token.symbol, total_borrows, total_deposits
                    );
                }
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
        cypher_user_pubkey: &Pubkey,
        check: &LiquidationCheck,
    ) -> Result<(), ClientError> {
        let dex_market_state =
            get_serum_market(Arc::clone(&self.client), &cypher_market.dex_market)
                .await
                .unwrap();
        let cypher_token = cypher_group.get_cypher_token(check.market_index).unwrap();
        let settle_ix = get_settle_funds_ix(
            cypher_group,
            cypher_market,
            cypher_token,
            &dex_market_state,
            &check.open_orders_pubkey,
            cypher_user_pubkey,
            &self.keypair,
        );
        let ixs = vec![settle_ix];

        let blockhash = self.chain_meta_service.get_latest_blockhash().await;

        self.submit_transactions(ixs, blockhash).await
    }

    async fn cancel_user_orders(
        self: &Arc<Self>,
        cypher_group: &CypherGroup,
        cypher_market: &CypherMarket,
        cypher_user_pubkey: &Pubkey,
        check: &LiquidationCheck,
    ) -> Result<(), ClientError> {
        let dex_market_state =
            get_serum_market(Arc::clone(&self.client), &cypher_market.dex_market)
                .await
                .unwrap();

        let cypher_token = cypher_group.get_cypher_token(check.market_index).unwrap();

        let mut ixs = Vec::new();

        for order in &check.orders {
            let cancel_ix = get_cancel_order_ix(
                cypher_group,
                cypher_market,
                cypher_token,
                &dex_market_state,
                &check.open_orders_pubkey,
                cypher_user_pubkey,
                &self.keypair,
                CancelOrderInstructionV2 {
                    order_id: order.order_id,
                    side: order.side,
                },
            );
            ixs.push(cancel_ix);
        }

        let blockhash = self.chain_meta_service.get_latest_blockhash().await;

        self.submit_transactions(ixs, blockhash).await
    }

    async fn get_cypher_group_and_liqor(self: &Arc<Self>) -> Result<(), ClientError> {
        let res = self
            .client
            .get_account_with_commitment(&self.cypher_group_pubkey, CommitmentConfig::confirmed())
            .await;

        let account_res = match res {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    "[LIQ] Could not fetch cypher group account: {}",
                    e.to_string()
                );
                return Err(e);
            }
        };

        let account = match account_res.value {
            Some(a) => {
                info!("[LIQ] Successfully fetched cypher group.");
                a
            }
            None => {
                warn!("[LIQ] Could not get cypher group account info from request.");
                return Ok(());
            }
        };

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
                    "[LIQ] Could not fetch cypher liquidator account: {}",
                    e.to_string()
                );
                return Err(e);
            }
        };

        let account = match account_res.value {
            Some(a) => {
                info!("[LIQ] Successfully fetched cypher liquidator account.");
                a
            }
            None => {
                warn!("[LIQ] Could not get cypher liquidator account info from request.");
                return Ok(());
            }
        };

        let cypher_liqor = get_zero_copy_account::<CypherUser>(&account);

        *self.cypher_liqor.write().await = Some(*cypher_liqor);

        Ok(())
    }

    async fn get_cypher_group_and_liqor_replay_inner(self: &Arc<Self>) {
        loop {
            let res = self.get_cypher_group_and_liqor().await;

            match res {
                Ok(_) => (),
                Err(e) => {
                    warn!("[LIQ] There was an error getting the cypher group and cypher liquidator accounts: {}", e.to_string());
                }
            };

            tokio::time::sleep(Duration::from_millis(2000)).await;
        }
    }

    async fn get_cypher_group_and_liqor_replay(self: &Arc<Self>) {
        let mut shutdown = self.shutdown.subscribe();

        tokio::select! {
            _ = self.get_cypher_group_and_liqor_replay_inner() => {},
            _ = shutdown.recv() => {
                info!("[LIQ] Received shutdown signal, stopping cypher group and liquidator account requests.");
            }
        }
    }
}

#[derive(Debug)]
pub enum LiquidatorError {
    ShutdownError,
    CypherGroupNotFound,
    CypherLiqorNotFound,
    ZeroedSimulation,
    ErrorSubmittingTransaction(ClientError),
}

use {
    cypher::{
        client::{cancel_order_ix, liquidate_collateral_ix, settle_funds_ix, ToPubkey},
        utils::{derive_dex_market_authority, gen_dex_vault_signer_key, parse_dex_account},
        CypherGroup, CypherMarket, CypherToken,
    },
    log::warn,
    serum_dex::{
        instruction::CancelOrderInstructionV2,
        matching::Side,
        state::{MarketStateV2, OpenOrders},
    },
    solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient},
    solana_sdk::{
        commitment_config::CommitmentConfig, instruction::Instruction, pubkey::Pubkey,
        signature::Keypair, signer::Signer,
    },
    std::{convert::identity, sync::Arc},
    std::{error::Error, fs::File, io::Read, str::FromStr},
};

pub async fn get_serum_market(
    client: Arc<RpcClient>,
    market_pubkey: &Pubkey,
) -> Result<MarketStateV2, ClientError> {
    let ai_res = client
        .get_account_with_commitment(market_pubkey, CommitmentConfig::confirmed())
        .await;

    let ai = match ai_res {
        Ok(ai) => ai.value.unwrap(),
        Err(e) => {
            warn!(
                "There was an error while fetching the serum market: {}",
                e.to_string()
            );
            return Err(e);
        }
    };

    let market: MarketStateV2 = parse_dex_account(ai.data);

    Ok(market)
}

pub async fn get_serum_open_orders(
    client: Arc<RpcClient>,
    open_orders_pubkey: &Pubkey,
) -> Result<OpenOrders, ClientError> {
    let ai_res = client
        .get_account_with_commitment(open_orders_pubkey, CommitmentConfig::confirmed())
        .await;

    let ai = match ai_res {
        Ok(ai) => ai.value.unwrap(),
        Err(e) => {
            warn!(
                "There was an error while fetching the open orders account: {}",
                e.to_string()
            );
            return Err(e);
        }
    };

    let dex_open_orders: OpenOrders = parse_dex_account(ai.data);

    Ok(dex_open_orders)
}

pub struct OpenOrder {
    pub order_id: u128,
    pub side: Side,
}

pub fn get_open_orders(open_orders: &OpenOrders) -> Vec<OpenOrder> {
    let mut oo: Vec<OpenOrder> = Vec::new();
    let orders = open_orders.orders;

    for i in 0..orders.len() {
        let order_id = open_orders.orders[i];

        if order_id != u128::default() {
            let side = open_orders.slot_side(i as u8).unwrap();
            oo.push(OpenOrder { order_id, side });
        }
    }

    oo
}

pub fn load_keypair(path: &str) -> Result<Keypair, Box<dyn Error>> {
    let fd = File::open(path);

    let mut file = match fd {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to load keypair file: {}", e.to_string());
            return Err(Box::new(e));
        }
    };

    let file_string = &mut String::new();
    let file_read_res = file.read_to_string(file_string);

    let _ = if let Err(e) = file_read_res {
        warn!(
            "Failed to read keypair bytes from keypair file: {}",
            e.to_string()
        );
        return Err(Box::new(e));
    };

    let keypair_bytes: Vec<u8> = file_string
        .replace('[', "")
        .replace(']', "")
        .replace(',', " ")
        .split(' ')
        .map(|x| u8::from_str(x).unwrap())
        .collect();

    let keypair = Keypair::from_bytes(keypair_bytes.as_ref());

    match keypair {
        Ok(kp) => Ok(kp),
        Err(e) => {
            warn!("Failed to load keypair from bytes: {}", e.to_string());
            Err(Box::new(e))
        }
    }
}

pub fn get_liquidate_collateral_ixs(
    cypher_group: &CypherGroup,
    asset_mint: &Pubkey,
    liab_mint: &Pubkey,
    liqor_pubkey: &Pubkey,
    liqee_pubkey: &Pubkey,
    signer: &Keypair,
) -> Instruction {
    liquidate_collateral_ix(
        &cypher_group.self_address,
        liqor_pubkey,
        &signer.pubkey(),
        liqee_pubkey,
        asset_mint,
        liab_mint,
    )
}

pub fn get_settle_funds_ix(
    cypher_group: &CypherGroup,
    cypher_market: &CypherMarket,
    cypher_token: &CypherToken,
    dex_market_state: &MarketStateV2,
    open_orders_pubkey: &Pubkey,
    cypher_user_pubkey: &Pubkey,
    signer: &Keypair,
) -> Instruction {
    let dex_vault_signer = gen_dex_vault_signer_key(
        dex_market_state.vault_signer_nonce,
        &cypher_market.dex_market,
    );
    settle_funds_ix(
        &cypher_group.self_address,
        &cypher_group.vault_signer,
        cypher_user_pubkey,
        &signer.pubkey(),
        &cypher_token.mint,
        &cypher_token.vault,
        &cypher_group.quote_vault(),
        &cypher_market.dex_market,
        open_orders_pubkey,
        &identity(dex_market_state.coin_vault).to_pubkey(),
        &identity(dex_market_state.pc_vault).to_pubkey(),
        &dex_vault_signer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn get_cancel_order_ix(
    cypher_group: &CypherGroup,
    cypher_market: &CypherMarket,
    cypher_token: &CypherToken,
    dex_market_state: &MarketStateV2,
    open_orders_pubkey: &Pubkey,
    cypher_user_pubkey: &Pubkey,
    signer: &Keypair,
    ix_data: CancelOrderInstructionV2,
) -> Instruction {
    let dex_vault_signer = gen_dex_vault_signer_key(
        dex_market_state.vault_signer_nonce,
        &cypher_market.dex_market,
    );
    let prune_authority = derive_dex_market_authority(&cypher_market.dex_market);
    cancel_order_ix(
        &cypher_group.self_address,
        &cypher_group.vault_signer,
        cypher_user_pubkey,
        &signer.pubkey(),
        &cypher_token.mint,
        &cypher_token.vault,
        &cypher_group.quote_vault(),
        &cypher_market.dex_market,
        &prune_authority,
        open_orders_pubkey,
        &identity(dex_market_state.event_q).to_pubkey(),
        &identity(dex_market_state.bids).to_pubkey(),
        &identity(dex_market_state.asks).to_pubkey(),
        &identity(dex_market_state.coin_vault).to_pubkey(),
        &identity(dex_market_state.pc_vault).to_pubkey(),
        &dex_vault_signer,
        ix_data,
    )
}

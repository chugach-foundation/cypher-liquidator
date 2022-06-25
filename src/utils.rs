use anchor_lang::{Owner, ZeroCopy};
use arrayref::array_ref;
use bytemuck::{bytes_of, checked::from_bytes};
use cypher::{
    constants::{B_CYPHER_USER, B_DEX_MARKET_AUTHORITY, B_OPEN_ORDERS},
    states::{CypherGroup, CypherMarket, CypherToken},
};
use cypher_tester::{dex, get_request_builder, parse_dex_account, ToPubkey};
use serum_dex::{
    instruction::{CancelOrderInstructionV2, MarketInstruction},
    matching::Side,
    state::{MarketStateV2, OpenOrders},
};
use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    program_error::ProgramError,
    pubkey::Pubkey,
    signer::Signer,
};
use std::{convert::identity, sync::Arc};

use {
    log::warn,
    solana_sdk::signature::Keypair,
    std::{error::Error, fs::File, io::Read, str::FromStr},
};

pub fn derive_open_orders_address(dex_market_pk: &Pubkey, cypher_user_pk: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            B_OPEN_ORDERS,
            dex_market_pk.as_ref(),
            cypher_user_pk.as_ref(),
        ],
        &cypher::ID,
    )
    .0
}

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

pub fn derive_dex_market_authority(dex_market_pk: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[B_DEX_MARKET_AUTHORITY, dex_market_pk.as_ref()],
        &cypher::ID,
    )
}

pub fn gen_dex_vault_signer_key(
    nonce: u64,
    dex_market_pk: &Pubkey,
) -> Result<Pubkey, ProgramError> {
    let seeds = [dex_market_pk.as_ref(), bytes_of(&nonce)];
    Ok(Pubkey::create_program_address(&seeds, &dex::id())?)
}

pub fn derive_cypher_user_address(group_address: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[B_CYPHER_USER, group_address.as_ref(), &owner.to_bytes()],
        &cypher::ID,
    )
    .0
}

pub fn get_zero_copy_account<T: ZeroCopy + Owner>(solana_account: &Account) -> Box<T> {
    let data = &solana_account.data.as_slice();
    let disc_bytes = array_ref![data, 0, 8];
    assert_eq!(disc_bytes, &T::discriminator());
    Box::new(*from_bytes::<T>(&data[8..std::mem::size_of::<T>() + 8]))
}

pub fn get_liquidate_market_collateral_ixs(
    cypher_group: &CypherGroup,
    cypher_market: &CypherMarket,
    cypher_token: &CypherToken,
    liqor_pubkey: &Pubkey,
    liqee_pubkey: &Pubkey,
    signer: &Keypair,
) -> Vec<Instruction> {
    let ixs = get_request_builder()
        .accounts(cypher::accounts::LiquidateMarketCollateral {
            cypher_group: cypher_group.self_address,
            vault_signer: cypher_group.vault_signer,
            minting_rounds: cypher_market.minting_rounds,
            cypher_user: *liqor_pubkey,
            user_signer: signer.pubkey(),
            liqee_cypher_user: *liqee_pubkey,
            c_asset_mint: cypher_token.mint,
            cypher_c_asset_vault: cypher_token.vault,
            token_program: spl_token::id(),
        })
        .args(cypher::instruction::LiquidateMarketCollateral {})
        .instructions()
        .unwrap();
    ixs
}

pub fn get_liquidate_collateral_ixs(
    cypher_group: &CypherGroup,
    asset_mint: &Pubkey,
    liab_mint: &Pubkey,
    liqor_pubkey: &Pubkey,
    liqee_pubkey: &Pubkey,
    signer: &Keypair,
) -> Vec<Instruction> {
    let ixs = get_request_builder()
        .accounts(cypher::accounts::LiquidateCollateral {
            cypher_group: cypher_group.self_address,
            cypher_user: *liqor_pubkey,
            user_signer: signer.pubkey(),
            liqee_cypher_user: *liqee_pubkey,
        })
        .args(cypher::instruction::LiquidateCollateral {
            asset_mint: *asset_mint,
            liab_mint: *liab_mint,
        })
        .instructions()
        .unwrap();
    ixs
}

pub fn get_settle_funds_ix(
    cypher_group: &CypherGroup,
    dex_market_state: &MarketStateV2,
    cypher_user_pubkey: &Pubkey,
    open_orders_pubkey: &Pubkey,
    user_signer: &Pubkey,
    market_index: usize,
) -> Instruction {
    let accounts = get_settle_funds_accounts(
        cypher_group,
        dex_market_state,
        cypher_user_pubkey,
        open_orders_pubkey,
        user_signer,
        market_index,
    );

    Instruction {
        program_id: cypher::ID,
        accounts,
        data: MarketInstruction::SettleFunds.pack(),
    }
}

fn get_settle_funds_accounts(
    cypher_group: &CypherGroup,
    dex_market_state: &MarketStateV2,
    cypher_user_pubkey: &Pubkey,
    open_orders_pubkey: &Pubkey,
    user_signer: &Pubkey,
    market_index: usize,
) -> Vec<AccountMeta> {
    let cypher_market = cypher_group.get_cypher_market(market_index);
    let cypher_token = cypher_group.get_cypher_token(market_index);
    let dex_vault_signer = gen_dex_vault_signer_key(
        dex_market_state.vault_signer_nonce,
        &cypher_market.dex_market,
    )
    .unwrap();
    vec![
        AccountMeta::new(cypher_group.self_address, false),
        AccountMeta::new_readonly(cypher_group.vault_signer, false),
        AccountMeta::new(*cypher_user_pubkey, false),
        AccountMeta::new_readonly(*user_signer, false),
        AccountMeta::new(cypher_token.mint, false),
        AccountMeta::new(cypher_token.vault, false),
        AccountMeta::new(cypher_group.quote_vault(), false),
        AccountMeta::new(cypher_market.dex_market, false),
        AccountMeta::new(*open_orders_pubkey, false),
        AccountMeta::new(identity(dex_market_state.coin_vault).to_pubkey(), false),
        AccountMeta::new(identity(dex_market_state.pc_vault).to_pubkey(), false),
        AccountMeta::new_readonly(dex_vault_signer, false),
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(dex::id(), false),
    ]
}

#[allow(clippy::too_many_arguments)]
pub fn get_cancel_order_ix(
    cypher_group: &CypherGroup,
    cypher_market: &CypherMarket,
    cypher_token: &CypherToken,
    dex_market_state: &MarketStateV2,
    open_orders_pubkey: &Pubkey,
    cypher_user_pubkey: &Pubkey,
    user_signer: &Pubkey,
    ix_data: CancelOrderInstructionV2,
) -> Instruction {
    let accounts = get_cancel_orders_accounts(
        cypher_group,
        cypher_market,
        cypher_token,
        dex_market_state,
        open_orders_pubkey,
        cypher_user_pubkey,
        user_signer,
    );
    Instruction {
        program_id: cypher::ID,
        accounts,
        data: MarketInstruction::CancelOrderV2(ix_data).pack(),
    }
}

fn get_cancel_orders_accounts(
    cypher_group: &CypherGroup,
    cypher_market: &CypherMarket,
    cypher_token: &CypherToken,
    dex_market_state: &MarketStateV2,
    open_orders_pubkey: &Pubkey,
    cypher_user_pubkey: &Pubkey,
    user_signer: &Pubkey,
) -> Vec<AccountMeta> {
    let dex_vault_signer = gen_dex_vault_signer_key(
        dex_market_state.vault_signer_nonce,
        &cypher_market.dex_market,
    )
    .unwrap();
    let prune_authority = derive_dex_market_authority(&cypher_market.dex_market).0;
    vec![
        AccountMeta::new(cypher_group.self_address, false),
        AccountMeta::new_readonly(cypher_group.vault_signer, false),
        AccountMeta::new(*cypher_user_pubkey, false),
        AccountMeta::new_readonly(*user_signer, false),
        AccountMeta::new(cypher_token.mint, false),
        AccountMeta::new(cypher_token.vault, false),
        AccountMeta::new(cypher_group.quote_vault(), false),
        AccountMeta::new(cypher_market.dex_market, false),
        AccountMeta::new_readonly(prune_authority, false),
        AccountMeta::new(identity(dex_market_state.bids).to_pubkey(), false),
        AccountMeta::new(identity(dex_market_state.asks).to_pubkey(), false),
        AccountMeta::new(*open_orders_pubkey, false),
        AccountMeta::new(identity(dex_market_state.event_q).to_pubkey(), false),
        AccountMeta::new(identity(dex_market_state.coin_vault).to_pubkey(), false),
        AccountMeta::new(identity(dex_market_state.pc_vault).to_pubkey(), false),
        AccountMeta::new_readonly(dex_vault_signer, false),
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(dex::id(), false),
    ]
}

use cypher::{
    constants::QUOTE_TOKEN_IDX,
    quote_mint,
    states::{CypherGroup, CypherUser},
};
use jet_proto_math::Number;
use solana_sdk::pubkey::Pubkey;
use std::cmp::min;

pub fn simulate_liquidate_collateral(
    cypher_group: &CypherGroup,
    cypher_liqor_user: &CypherUser,
    cypher_liqee_user: &CypherUser,
    asset_mint: Pubkey,
    liab_mint: Pubkey,
) -> (u64, u64, u64, u64) {
    calc_collateral_liquidation(
        cypher_group,
        cypher_liqor_user,
        cypher_liqee_user,
        asset_mint,
        liab_mint,
    )
}

fn get_token_info(group: &CypherGroup, token_mint: Pubkey) -> (usize, u64) {
    if token_mint == quote_mint::ID {
        return (QUOTE_TOKEN_IDX, 1_u64);
    }
    let token_idx = group.get_token_idx(token_mint).unwrap();
    let market = group.get_cypher_market(token_idx);
    (token_idx, market.market_price)
}

fn calc_collateral_liquidation(
    group: &CypherGroup,
    liqor_user: &CypherUser,
    liqee_user: &CypherUser,
    asset_mint: Pubkey,
    liab_mint: Pubkey,
) -> (u64, u64, u64, u64) {
    let (asset_token_idx, asset_price) = get_token_info(group, asset_mint);
    let (liab_token_idx, liab_price) = get_token_info(group, liab_mint);

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
            return (repay_amount, 0, 0, repay_amount);
        }
    }

    let max_value_for_swap = if is_bankrupt {
        Number::from(group.insurance_fund) / liqor_fee
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
    let (liqee_asset_debit, global_insurance_debit, global_insurance_credit) = if is_bankrupt {
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
    (
        repay_amount,
        liqee_asset_debit,
        global_insurance_credit,
        global_insurance_debit,
    )
}

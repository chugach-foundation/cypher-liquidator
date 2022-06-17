use anchor_lang::{ZeroCopy, Owner};
use cypher::{constants::B_CYPHER_USER, states::{CypherGroup, CypherMarket, CypherToken}};
use cypher_tester::get_request_builder;
use solana_sdk::{account::Account, pubkey::Pubkey, instruction::Instruction, signer::Signer};
use arrayref::array_ref;
use bytemuck::checked::from_bytes;

use {
    std::{
        str::FromStr, fs::File, error::Error, io::Read
    },
    log::warn,
    solana_sdk::signature::Keypair
};

pub fn load_keypair(path: &str) -> Result<Keypair, Box<dyn Error>> {
    let fd = File::open(path);
    
    let mut file = match fd {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to load keypair file: {}", e.to_string());
            return Err(Box::new(e));
        },
    };

    let file_string = &mut String::new();
    let file_read_res = file.read_to_string(file_string);

    let _ = if let Err(e) = file_read_res {
        warn!("Failed to read keypair bytes from keypair file: {}", e.to_string());
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

pub fn derive_cypher_user_address(group_address: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            B_CYPHER_USER,
            group_address.as_ref(),
            &owner.to_bytes(),
        ],
        &cypher::ID,
    ).0
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
) -> Vec<Instruction>{
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
) -> Vec<Instruction>{ 
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
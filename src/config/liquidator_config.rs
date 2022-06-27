use std::{error::Error, fs::File, io::BufReader};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquidatorConfig {
    pub wallet: String,
    pub cluster: String,
    pub log_simulations: bool,
    pub log_account_healths: bool,
}

pub fn load_liquidator_config(path: &str) -> Result<LiquidatorConfig, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let liq_conf: LiquidatorConfig = serde_json::from_reader(reader).unwrap();
    Ok(liq_conf)
}

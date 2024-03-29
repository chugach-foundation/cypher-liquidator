use {
    serde::{Deserialize, Serialize},
    std::{error::Error, fs::File, io::BufReader},
};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquidatorConfig {
    pub wallet: String,
    pub group: String,
    pub log_simulations: bool,
    pub log_liqee_healths: bool,
    pub log_liqor_health: bool,
}

pub fn load_liquidator_config(path: &str) -> Result<LiquidatorConfig, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let liq_conf: LiquidatorConfig = serde_json::from_reader(reader).unwrap();
    Ok(liq_conf)
}

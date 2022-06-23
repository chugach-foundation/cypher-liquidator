#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use serde_json;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CypherConfig {
    pub clusters: Clusters,
    pub groups: Vec<CypherGroupConfig>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Clusters {
    pub devnet: ClusterConfig,
    pub mainnet: ClusterConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterConfig {
    pub rpc_url: String,
    pub pubsub_url: String,
}

impl CypherConfig {
    pub fn get_config_for_cluster(&self, cluster: &str) -> &ClusterConfig {
        match cluster {
            "devnet" => &self.clusters.devnet,
            "mainnet" => &self.clusters.mainnet,
            "" => &self.clusters.devnet,
            &_ => &self.clusters.devnet,
        }
    }

    pub fn get_group(&self, cluster: &str) -> Option<&CypherGroupConfig> {
        self.groups.iter().find(|&g| g.cluster.as_str() == cluster)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CypherGroupConfig {
    pub cluster: String,
    pub name: String,
    pub quote_symbol: String,
    pub address: String,
    pub program_id: String,
    pub serum_program_id: String,
    pub tokens: Vec<CypherTokenConfig>,
    pub oracles: Vec<CypherOracleConfig>,
    pub markets: Vec<CypherMarketConfig>,
}

impl CypherGroupConfig {
    pub fn get_market(&self, market: &str) -> Option<&CypherMarketConfig> {
        self.markets.iter().find(|&m| m.name.as_str() == market)
    }

    pub fn get_market_by_index(&self, idx: usize) -> Option<&CypherMarketConfig> {
        self.markets.iter().find(|&m| m.market_index == idx)
    }

    pub fn get_token_by_index(&self, idx: usize) -> Option<&CypherTokenConfig> {
        self.tokens.iter().find(|&t| t.token_index == idx)
    }

    pub fn get_oracle(&self, symbol: &str) -> Option<&CypherOracleConfig> {
        self.oracles.iter().find(|&o| o.symbol == symbol)
    }

    pub fn get_mint_for_symbol(&self, symbol: &str) -> Option<&CypherTokenConfig> {
        self.tokens.iter().find(|&t| t.symbol == symbol)
    }

    pub fn get_market_index(&self, market: &str) -> Option<usize> {
        let market = self.markets.iter().find(|&m| m.name.as_str() == market);

        market.map(|m| m.market_index)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CypherTokenConfig {
    pub symbol: String,
    pub mint: String,
    pub token_index: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CypherOracleConfig {
    pub symbol: String,
    pub address: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CypherMarketConfig {
    pub name: String,
    pub base_symbol: String,
    pub quote_symbol: String,
    pub market_type: String,
    pub pair_base_symbol: String,
    pub pair_quote_symbol: String,
    pub address: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub market_index: usize,
    pub bids: String,
    pub asks: String,
    pub event_queue: String,
}

pub fn load_cypher_config(path: &str) -> Result<CypherConfig, Box<dyn Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let cypher_config: CypherConfig = serde_json::from_reader(reader).unwrap();
    Ok(cypher_config)
}

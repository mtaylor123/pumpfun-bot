use serde::Deserialize;
use std::fs;
use anyhow::{Context, Result};

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub rpc_url: String,
    pub payer_keypair_path: String,
    pub num_temp_wallets: usize,
    pub price_buy_amount_lamports: u64,
    pub volume_pair_amount_lamports: u64,
    pub price_buy_frequency: f64,
    pub volume_frequency: f64,
    pub total_iterations: usize,
    pub main_delay_min_secs: u64,
    pub main_delay_max_secs: u64,
    pub short_delay_min_secs: u64,
    pub short_delay_max_secs: u64,
    pub initial_buy_amount_lamports: u64,
    pub sell_threshold_price: f64,
    pub sell_threshold_mc: u64,
    pub timeout_secs: u64,
    pub fund_temp_wallets_amount_lamports: u64,
    pub github: GithubConfig,
    }

#[derive(Debug, Deserialize, Clone)]
pub struct GithubConfig {
    pub owner: String,
    pub repo: String,
    pub token: String,
}

pub fn load_config() -> Result<Config> {
    let content = fs::read_to_string("configtest.toml")
        .context("Failed to read config.toml. Make sure it exists in the project root.")?;

    let config: Config = toml::from_str(&content)
        .context("Failed to parse config.toml. Check for syntax errors or missing fields.")?;

    // Basic validation
    if config.num_temp_wallets < 50 || config.num_temp_wallets > 300 {
        //anyhow::bail!("num_temp_wallets should be between 50 and 300 for good diversity.");
    }

    if config.price_buy_amount_lamports == 0 || config.volume_pair_amount_lamports == 0 {
        anyhow::bail!("Trade amounts must be greater than 0 lamports.");
    }

    Ok(config)
}
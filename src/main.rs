mod config;
mod utils;
mod pumpfun;
mod trend_fetcher;
mod ai_generator;

mod monitoring;
mod sell;

mod initial_buy;

use monitoring::run_monitoring_and_sell;
use sell::perform_sell_all;
use crate::sell::refund_temp_wallets_to_payer;
use crate::pumpfun::sell_on_curve;
use solana_sdk::pubkey::Pubkey;

use config::load_config;
use utils::wallet_manager::WalletManager;
use solana_sdk::signature::{Keypair, Signer};
use std::fs::File;
use std::io::Read;
use anyhow::{Context, Result};
use serde_json::Value;
use std::str::FromStr;

use std::time::{SystemTime, UNIX_EPOCH};
use rand::seq::SliceRandom;
use rand::thread_rng;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting Pump.fun bot test...");

    let cfg = load_config()?;
    println!("Config loaded successfully!");
    println!("{:#?}", cfg);

    println!("\nTesting payer keypair loading...");

    // Load payer keypair with robust parsing
    let payer_bytes = std::fs::read(&cfg.payer_keypair_path)
        .context("Failed to read payer keypair file — check path in config.toml")?;

    let payer = load_keypair_from_bytes(&payer_bytes)
        .context("Failed to parse payer keypair — file may be corrupted or wrong format")?;

    println!("Payer keypair loaded successfully!");
    println!("Payer pubkey: {}", payer.pubkey());

    println!("\nTesting wallet manager...");

    let manager = WalletManager::new(
        payer,
        &cfg.rpc_url,
        cfg.num_temp_wallets,
        cfg.fund_temp_wallets_amount_lamports,  
    )?;

    println!("Generated and funded {} temp wallets!", cfg.num_temp_wallets);
    println!("Example random wallet pubkey: {}", manager.select_random_wallet().pubkey());

    
    
    // 1) Fetch trends
    let trends = trend_fetcher::fetch_recent_trends("solana", 6 * 60 * 60).await?;
    if trends.is_empty() {
        return Err(anyhow::anyhow!("No trends returned"));
    }


    // 2) Pick one at random
    let mut rng = rand::thread_rng();
    let t = trends.choose(&mut rng).unwrap();


    // 3) Make a unique metadata file key like SYMBOL-<timestamp>
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis();

    let sym = trend_fetcher::sanitize_symbol_for_filename(&t.symbol);
    let file_key = format!("{}-{}", sym, ms);

    // 4) Upload metadata JSON to GitHub Pages and get URI


    let uri = trend_fetcher::upload_metadata_and_get_uri(
        &cfg.github,
        &file_key,
        &t.name,
        &t.symbol,
        t.image_url.as_deref(),
        t.description.as_deref(),
    ).await?;

    println!("Picked trend: {} ({})", t.name, t.symbol);
    println!("Metadata URI: {}", uri);


    // 5) Create ONE Pump.fun token using your existing create method

    println!("\nCreating Pump.fun token...");
    let mint = pumpfun::create_pumpfun_token(
        &manager.rpc_client,
        &manager.payer,
        &t.name,
        &t.symbol,
        &uri,
    )?;

    println!("Mint created: {}", mint);




        // refund_temp_wallets_to_payer(
        // &manager.rpc_client,
        // &manager.payer,
        // &manager.temp_wallets,
        // ).await?;

   
    // After creation (mint is now available)
    println!("\nPerforming initial buy...");

    initial_buy::perform_initial_buy(
        &manager.rpc_client,
        &manager.payer,
        &mint,
        cfg.initial_buy_amount_lamports,
    ).await?;

    println!("Initial buy complete!");

     let monitoring_res = monitoring::run_monitoring_and_sell(
        &manager.rpc_client,
        &manager.payer,
        &manager.temp_wallets,
        &mint,
        cfg.sell_threshold_price,
        cfg.sell_threshold_mc,
        cfg.timeout_secs,
    ).await;

    if let Err(e) = &monitoring_res {
        eprintln!("[WARN] run_monitoring_and_sell failed: {:#}", e);
        // continue no matter what
    }



// let mint = Pubkey::from_str("JBkrFe4YtLQSdwGFQ2eqbEBoBqR9614acyRM7Y9NVjBd")?;

//  let loop_res = utils::wash_price::Invest_In_New_Coin_Strategy_loop(
//         &manager.rpc_client,
//         &manager.payer,
//         &manager.temp_wallets,
//         &mint,
//         cfg.price_buy_amount_lamports,
//         cfg.volume_pair_amount_lamports,
//         cfg.price_buy_frequency,
//         cfg.volume_frequency,
//         cfg.total_iterations,
//         cfg.main_delay_min_secs,
//         cfg.main_delay_max_secs,
//         cfg.short_delay_min_secs,
//         cfg.short_delay_max_secs,
//     ).await;

//     if let Err(e) = &loop_res {
//         eprintln!("[WARN] Invest_In_New_Coin_Strategy_loop  error: {:#}", e);
//     }

//     let monitoring_res = monitoring::run_monitoring_and_sell(
//         &manager.rpc_client,
//         &manager.payer,
//         &manager.temp_wallets,
//         &mint,
//         cfg.sell_threshold_price,
//         cfg.sell_threshold_mc,
//         cfg.timeout_secs,
//     ).await;

//     if let Err(e) = &monitoring_res {
//         eprintln!("[WARN] run_monitoring_and_sell failed: {:#}", e);
//         // continue no matter what
//     }


//     refund_temp_wallets_to_payer(
//         &manager.rpc_client,
//         &manager.payer,
//         &manager.temp_wallets,
//     ).await?;








//  refund_temp_wallets_to_payer(
//         &manager.rpc_client,
//         &manager.payer,
//         &manager.temp_wallets,
//         ).await?;


// sell_on_curve(rpc_client, payer, mint_pubkey, 1_000_000_000).await?; 



// // Check results
// loop_result?;
// monitoring_result?;



    Ok(())
}

// Helper function to load Keypair from bytes (handles raw array or structured JSON)
fn load_keypair_from_bytes(bytes: &[u8]) -> Result<Keypair> {
    // Attempt 1: Raw JSON array [12,34,56,...]
    if bytes.get(0) == Some(&b'[') {
        let json: Vec<u8> = serde_json::from_slice(bytes)
            .context("Failed to parse raw keypair array")?;
        if json.len() != 64 {
            anyhow::bail!("Raw keypair must be exactly 64 bytes (found {})", json.len());
        }
        return Ok(Keypair::from_bytes(&json)?);
    }

    // Attempt 2: Structured JSON ({"pubkey": "...", "keypair": [..]})
    let json: Value = serde_json::from_slice(bytes)
        .context("Failed to parse structured keypair JSON")?;

    // Extract the keypair array
    let key_array = json.get("keypair")
        .or_else(|| if json.is_array() { Some(&json) } else { None })
        .ok_or_else(|| anyhow::anyhow!("No keypair array found in JSON file"))?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Keypair value is not an array"))?;

    let mut bytes_vec = Vec::with_capacity(key_array.len());
    for v in key_array {
        let u = v.as_u64()
            .ok_or_else(|| anyhow::anyhow!("Invalid u8 value in keypair array"))? as u8;
        bytes_vec.push(u);
    }

    if bytes_vec.len() != 64 {
        anyhow::bail!("Keypair must be exactly 64 bytes (found {})", bytes_vec.len());
    }

    Ok(Keypair::from_bytes(&bytes_vec)?)
}
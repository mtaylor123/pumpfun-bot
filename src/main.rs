mod config;
mod utils;
mod pumpfun;
mod trend_fetcher;
mod ai_generator;
mod monitoring;
mod sell;
mod initial_buy;
mod app_flags;

use crate::app_flags::AppFlags;
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
use std::sync::Arc;
use std::sync::atomic::Ordering;


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

    let flags = AppFlags::new();

    // Wrap shared things in Arc so both tasks can use them without moving `manager`
    let rpc = Arc::new(manager.rpc_client);
    let payer = Arc::new(manager.payer);
    let temp_wallets = Arc::new(manager.temp_wallets);

    // Pubkey is Copy, so this is fine:
    let mint_copy = mint;

    let flags_for_monitor = flags.clone();
    let rpc_m = rpc.clone();
    let payer_m = payer.clone();
    let temps_m = temp_wallets.clone();

    let monitor_task = tokio::spawn(async move {
        monitoring::run_monitoring_and_sell(
            rpc_m.as_ref(),
            payer_m.as_ref(),
            temps_m.as_ref(),
            &mint_copy,
            cfg.sell_threshold_price,
            cfg.sell_threshold_mc,
            cfg.timeout_secs,
            flags_for_monitor,
        )
        .await
    });

    let flags_for_invest = flags.clone();
    let rpc_i = rpc.clone();
    let payer_i = payer.clone();
    let temps_i = temp_wallets.clone();

    let invest_task = tokio::spawn(async move {
        utils::wash_price::Invest_In_New_Coin_Strategy_loop(
            rpc_i.as_ref(),
            payer_i.as_ref(),
            temps_i.as_ref(),
            &mint_copy,
            cfg.price_buy_amount_lamports,
            cfg.volume_pair_amount_lamports,
            cfg.price_buy_frequency,
            cfg.volume_frequency,
            cfg.total_iterations,
            cfg.main_delay_min_secs,
            cfg.main_delay_max_secs,
            cfg.short_delay_min_secs,
            cfg.short_delay_max_secs,
            flags_for_invest,
        )
        .await
    });

    let (i_res, m_res) = tokio::join!(invest_task, monitor_task);

    // Handle invest task result without crashing the whole program
    match i_res {
        Ok(Ok(())) => println!("[main] invest task finished ok"),
        Ok(Err(e)) => {
            eprintln!("[main][WARN] invest task returned error: {:#}", e);
            // Important: allow monitor to proceed with sell if it was waiting
            flags.invest_done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Err(join_err) => {
            eprintln!("[main][WARN] invest task panicked/cancelled: {}", join_err);
            flags.invest_done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // Handle monitor task result without crashing the whole program
    match m_res {
        Ok(Ok(())) => println!("[main] monitor task finished ok"),
        Ok(Err(e)) => {
            eprintln!("[main][WARN] monitor task returned error: {:#}", e);
            // Safety: tell invest to stop if monitor died
            flags.stop_buys.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Err(join_err) => {
            eprintln!("[main][WARN] monitor task panicked/cancelled: {}", join_err);
            flags.stop_buys.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }



    // -------------------------
    // FINALIZATION (always runs)
    // -------------------------

    flags.stop_buys.store(true, Ordering::SeqCst);
    flags.invest_done.store(true, Ordering::SeqCst);

    println!("[main] Finalization: sanity sell_all (best-effort)...");
    if let Err(e) = perform_sell_all(rpc.as_ref(), payer.as_ref(), temp_wallets.as_ref(), &mint_copy).await {
        eprintln!("[main][WARN] final perform_sell_all failed: {:#}", e);
    }

    println!("[main] Finalization: refund temp wallets to payer (must run)...");
    if let Err(e) = refund_temp_wallets_to_payer(rpc.as_ref(), payer.as_ref(), temp_wallets.as_ref()).await {
        eprintln!("[main][WARN] refund_temp_wallets_to_payer failed: {:#}", e);
    } else {
        println!("[main] refund_temp_wallets_to_payer completed OK");
    }


// let mint = Pubkey::from_str("JBkrFe4YtLQSdwGFQ2eqbEBoBqR9614acyRM7Y9NVjBd")?;


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
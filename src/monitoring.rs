use crate::sell::perform_sell_all;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_client::rpc_client::RpcClient;
use anyhow::{anyhow, Result};
use tokio::time::{sleep, Duration};
use std::time::Instant;

use borsh::BorshDeserialize;
use serde::Deserialize;
use spl_token::state::Mint;
use solana_sdk::program_pack::Pack;
use std::str::FromStr;
use serde_json::Value;
use reqwest::{Client, header};

const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Your on-chain Borsh struct (after stripping 8-byte Anchor discriminator)
/// --------------------
#[derive(BorshDeserialize, Debug)]
pub struct BondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
}

/// Anchor accounts have an 8-byte discriminator prefix.
/// --------------------
fn strip_anchor_discriminator(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 8 {
        return Err(anyhow!("account data too short for Anchor discriminator"));
    }
    Ok(&data[8..])
}


/// --------------------
/// Borsh decode with debug print.
/// allow_leftover=true is recommended because you observed leftover=1 sometimes.
/// --------------------
fn borsh_decode_prefix_dbg<T: BorshDeserialize>(
    data: &[u8],
    label: &str,
    allow_leftover: bool,
) -> Result<T> {
    let mut cursor = data;
    let value = T::deserialize(&mut cursor)?;

    let consumed = data.len() - cursor.len();
    let leftover = cursor.len();

    println!(
        "{} decoded, bytes consumed = {}, leftover = {}",
        label, consumed, leftover
    );

    if !allow_leftover && leftover != 0 {
        return Err(anyhow!("{} decode left {} trailing bytes", label, leftover));
    }

    Ok(value)
}


/// --------------------
/// PDA helper
/// --------------------
fn pda(program_id: &Pubkey, seeds: &[&[u8]]) -> (Pubkey, u8) {
    Pubkey::find_program_address(seeds, program_id)
}

/// --------------------
/// Derive bonding curve PDA for a mint
/// --------------------
fn bonding_curve_pda(pump_program_id: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(pump_program_id, &[b"bonding-curve", mint.as_ref()]).0
}

/// --------------------
/// Fetch + decode bonding curve
/// --------------------
fn fetch_bonding_curve(rpc_client: &RpcClient, mint_pubkey: &Pubkey) -> Result<BondingCurve> {
    let pump_program_id = Pubkey::from_str(PUMP_PROGRAM_ID_STR)?;
    let curve_pda = bonding_curve_pda(&pump_program_id, mint_pubkey);

    let curve_acc = rpc_client.get_account(&curve_pda)?;
    let curve_data = strip_anchor_discriminator(&curve_acc.data)?;

    // allow leftovers so monitoring doesn't break over a trailing byte / padding
    let curve: BondingCurve = borsh_decode_prefix_dbg(curve_data, "BondingCurve", true)?;

    Ok(curve)
}



/// --------------------
/// Read mint decimals + supply (raw)
/// --------------------
fn fetch_mint_decimals_and_supply(rpc_client: &RpcClient, mint_pubkey: &Pubkey) -> Result<(u8, u64)> {
    let acc = rpc_client.get_account(mint_pubkey)?;
    if acc.owner != spl_token::ID {
        return Err(anyhow!("mint account owner is not Tokenkeg (not an SPL mint)"));
    }
    let mint = Mint::unpack(&acc.data)?;
    Ok((mint.decimals, mint.supply))
}

/// --------------------
/// pow10 in u128
/// --------------------
fn pow10_u128(exp: u8) -> u128 {
    let mut v: u128 = 1;
    for _ in 0..exp {
        v *= 10;
    }
    v
}


/// --------------------
/// Compute spot price as lamports per 1.0 token (whole token), integer math.
/// lamports_per_token = (vSOL_lamports * 10^decimals) / vTOK_raw
/// --------------------
fn compute_price_lamports_per_token(curve: &BondingCurve, decimals: u8) -> Result<u64> {
    if curve.virtual_token_reserves == 0 {
        return Err(anyhow!("virtual_token_reserves is 0"));
    }
    let scale = pow10_u128(decimals);
    let num = (curve.virtual_sol_reserves as u128)
        .checked_mul(scale)
        .ok_or_else(|| anyhow!("price numerator overflow"))?;
    Ok((num / curve.virtual_token_reserves as u128) as u64)
}


/// --------------------
/// CoinGecko SOL/USD
/// We store USD in micro-USD: $1.00 == 1_000_000
/// --------------------
#[derive(Deserialize)]
struct CoinGeckoPriceResp {
    solana: CoinGeckoSolana,
}
#[derive(Deserialize)]
struct CoinGeckoSolana {
    usd: f64,
}


async fn fetch_sol_usd_micro() -> Result<u64> {
    let url = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";

    // Build a client with a UA + timeout (helps with some edge cases)
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let resp = client
        .get(url)
        .header(header::USER_AGENT, "pumpfun-bot/0.1 (solana monitoring)")
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await?; // <-- raw body so we can debug it

    // If not OK, show body snippet
    if !status.is_success() {
        let snippet = body.chars().take(400).collect::<String>();
        return Err(anyhow!("CoinGecko HTTP {} body: {}", status, snippet));
    }

    // Parse dynamic JSON
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| anyhow!("CoinGecko JSON parse failed: {} | body_snippet={}", e, body.chars().take(400).collect::<String>()))?;

    // Expected: {"solana":{"usd":123.45}}
    if let Some(usd) = v
        .get("solana")
        .and_then(|x| x.get("usd"))
        .and_then(|x| x.as_f64())
    {
        let micro = (usd * 1_000_000.0).round();
        if micro <= 0.0 {
            return Err(anyhow!("CoinGecko returned non-positive usd={} body={}", usd, body.chars().take(200).collect::<String>()));
        }
        return Ok(micro as u64);
    }

    // Sometimes CoinGecko returns {"error":"..."}
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        return Err(anyhow!("CoinGecko error: {} | body_snippet={}", err, body.chars().take(400).collect::<String>()));
    }

    // Fallback: unknown shape (could be HTML or new schema)
    let snippet = body.chars().take(400).collect::<String>();
    Err(anyhow!("CoinGecko unexpected response shape (missing solana.usd). body_snippet={}", snippet))
}

/// --------------------
/// Convert lamports/token + sol_usd_micro -> token_usd_micro
/// token_usd_micro = lamports_per_token * sol_usd_micro / 1e9
/// --------------------
fn compute_token_price_usd_micro(price_lamports_per_token: u64, sol_usd_micro: u64) -> Result<u64> {
    let v = (price_lamports_per_token as u128)
        .checked_mul(sol_usd_micro as u128)
        .ok_or_else(|| anyhow!("token usd calc overflow"))?;

    Ok((v / 1_000_000_000u128) as u64)
}

/// --------------------
/// Market cap (USD micro) using mint supply:
/// mc_usd_micro = (supply_raw * token_usd_micro) / 10^decimals
/// --------------------
fn compute_market_cap_usd_micro(supply_raw: u64, decimals: u8, token_usd_micro: u64) -> Result<u128> {
    let scale = pow10_u128(decimals);
    let v = (supply_raw as u128)
        .checked_mul(token_usd_micro as u128)
        .ok_or_else(|| anyhow!("market cap overflow"))?;
    Ok(v / scale)
}



/// --------------------
/// Your original signature (kept), but internally we do fixed-point compares.
/// - sell_threshold_price: in SOL (e.g., 0.0008)
/// - sell_threshold_mc: in USD (whole dollars)
/// --------------------
/// Assumes you already have:
///   async fn perform_sell_all(...) -> Result<()>
/// somewhere else.
pub async fn run_monitoring_and_sell(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
    mint_pubkey: &Pubkey,
    sell_threshold_price: f64,
    sell_threshold_mc: u64,
    timeout_secs: u64,
) -> Result<()> {
    println!("Starting monitoring for mint: {}", mint_pubkey);
    println!(
        "Thresholds: Price > {} SOL, MC > {} USD, Timeout: {} seconds",
        sell_threshold_price, sell_threshold_mc, timeout_secs
    );

    // Convert thresholds ONCE into integers (no float comparisons in the loop)
    // price threshold: SOL -> lamports per token threshold isn't directly from SOL alone,
    // because it's SOL per token (not SOL amount). Your threshold is "token price in SOL".
    // We'll convert "token price in SOL" into lamports-per-token threshold:
    let sell_threshold_price_lamports_per_token: u64 = {
        if sell_threshold_price <= 0.0 {
            0
        } else {
            (sell_threshold_price * 1_000_000_000.0).round() as u64
        }
    };

    // market cap threshold in micro-USD:
    let sell_threshold_mc_usd_micro: u128 = (sell_threshold_mc as u128) * 1_000_000u128;

    // Mint info
    let (decimals, supply_raw) = fetch_mint_decimals_and_supply(rpc_client, mint_pubkey)?;
    println!("Mint decimals={}, supply_raw={}", decimals, supply_raw);

    // We can cache SOL/USD for a bit to avoid hammering CoinGecko.
    // Refresh every 20 polls (every ~200s if poll=10s), tweak as you like.
    
    let mut sol_usd_micro: u64 = 0;
    let mut have_sol_usd = false;
    match fetch_sol_usd_micro().await {
        Ok(v) => { sol_usd_micro = v; have_sol_usd = true; }
        Err(e) => { println!("WARN: initial SOL/USD fetch failed: {}", e); }
    }

    let mut poll_count: u64 = 0;

    let start_time = Instant::now();

    loop {
        // Timeout first
        if start_time.elapsed().as_secs() > timeout_secs {
            println!("Timeout reached! Triggering sell all...");
            perform_sell_all(rpc_client, payer, temp_wallets, mint_pubkey).await?;
            return Ok(());
        }

        // Refresh SOL/USD occasionally
        poll_count += 1;
        if poll_count == 1 || poll_count % 20 == 0 {
            match fetch_sol_usd_micro().await {
                Ok(v) => sol_usd_micro = v,
                Err(e) => {
                    // If CoinGecko fails, keep last price and continue
                    println!("WARN: SOL/USD refresh failed: {} (keeping last value={})", e, sol_usd_micro);
                }
            }
        }

        // On-chain poll: bonding curve
        let curve = fetch_bonding_curve(rpc_client, mint_pubkey)?;

        // Compute integer price + market cap
        let price_lamports_per_token = compute_price_lamports_per_token(&curve, decimals)?;
        let token_usd_micro = compute_token_price_usd_micro(price_lamports_per_token, sol_usd_micro)?;
        let mc_usd_micro = compute_market_cap_usd_micro(supply_raw, decimals, token_usd_micro)?;

        // Pretty print
        let token_price_sol_f64 = (price_lamports_per_token as f64) / 1_000_000_000.0;
        let token_price_usd_f64 = (token_usd_micro as f64) / 1_000_000.0;
        let mc_usd_f64 = (mc_usd_micro as f64) / 1_000_000.0;
        let sol_usd_f64 = (sol_usd_micro as f64) / 1_000_000.0;

        println!(
            "Stats | price={:.12} SOL/token | price=${:.8} | MC=${:.2} | SOL=${:.2} | vSOL={} vTOK={} | elapsed={}s",
            token_price_sol_f64,
            token_price_usd_f64,
            mc_usd_f64,
            sol_usd_f64,
            curve.virtual_sol_reserves,
            curve.virtual_token_reserves,
            start_time.elapsed().as_secs(),
        );

        // Integer threshold checks (no f64 comparisons)
        let price_hit = price_lamports_per_token >= sell_threshold_price_lamports_per_token;
        let mc_hit = mc_usd_micro >= sell_threshold_mc_usd_micro;

        if price_hit || mc_hit {
            println!(
                "Sell threshold met! price_hit={} mc_hit={} => selling all...",
                price_hit, mc_hit
            );
            perform_sell_all(rpc_client, payer, temp_wallets, mint_pubkey).await?;
            return Ok(());
        }

        // Poll interval
        sleep(Duration::from_secs(3)).await;
    }
}


// pub async fn run_monitoring_and_sell(
//     rpc_client: &RpcClient,
//     payer: &Keypair,
//     temp_wallets: &[Keypair],
//     mint_pubkey: &Pubkey,
//     sell_threshold_price: f64,
//     sell_threshold_mc: u64,
//     timeout_secs: u64,
// ) -> Result<()> {
//     println!("Starting monitoring for mint: {}", mint_pubkey);
//     println!("Thresholds: Price > {} SOL, MC > {} USD, Timeout: {} seconds", sell_threshold_price, sell_threshold_mc, timeout_secs);

//     let start_time = Instant::now();

//     loop {
//         let is_local = rpc_client.url().contains("127.0.0.1") || rpc_client.url().contains("localhost");

//         let (current_price_sol, current_mc_usd, current_holders) = if is_local {
//             // Simulated values (consistent tuple type)
//             let elapsed = start_time.elapsed().as_secs_f64();
//             (0.0008 + elapsed * 0.00001,  // price
//              80000 + (elapsed as u64 * 1000),  // MC
//              50 + (elapsed as u64 / 60))   // holders
//         } else {
//             // Real poll (placeholder for now)
//             (0.0005, 50000, 50)
//         };

//         println!("Monitoring stats: Price: {:.6} SOL, MC: {} USD, Holders: {}, Time elapsed: {}s",
//                  current_price_sol, current_mc_usd, current_holders, start_time.elapsed().as_secs());

//         if current_price_sol > sell_threshold_price || current_mc_usd > sell_threshold_mc {
//             println!("Sell threshold met! Triggering sell all...");
//             perform_sell_all(rpc_client, payer, temp_wallets, mint_pubkey).await?;
//             return Ok(());
//         }

//         if start_time.elapsed().as_secs() > timeout_secs {
//             println!("Timeout reached! Triggering sell all...");
//             perform_sell_all(rpc_client, payer, temp_wallets, mint_pubkey).await?;
//             return Ok(());
//         }

//         sleep(Duration::from_secs(30)).await;
//     }
// }
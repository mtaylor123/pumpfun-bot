use solana_sdk::{
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
    system_instruction,
};
use solana_client::rpc_client::RpcClient;

use rand::Rng;
use std::time::Duration;
use tokio::time::sleep;
use rand::seq::SliceRandom;
use crate::pumpfun::{buy_on_curve, sell_on_curve, buy_on_curve_async, 
    sell_on_curve_async, buy_on_curve_async_from_client, sell_on_curve_async_from_client};


use anyhow::{Result, anyhow};
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;
use tokio::time::{ Instant};
use spl_token::state::Account as SplTokenAccount;


/// How many wallets to actively use per “session”.
const ACTIVE_K: usize = 5;

/// Random session length in iterations before rotating wallets.
const SESSION_MIN_ITERS: usize = 4;
const SESSION_MAX_ITERS: usize = 8;

/// Conservative buffers (lamports)
const TX_FEE_BUFFER_LAMPORTS: u64 = 250_000;   // signatures + small priority fee cushion
const EXTRA_SAFETY_LAMPORTS: u64 = 200_000;    // extra cushion to avoid edge failures

/// Your observed minimum-ish dust (lamports) ~ 0.000895880 SOL.
/// We still compute rent-based dust, but never go below this.
const MIN_DUST_KEEP_LAMPORTS: u64 = 895_880;

/// Build + send a simple SOL transfer (blocking).
fn transfer_lamports_blocking(
    rpc_client: &RpcClient,
    from: &Keypair,
    to: &Pubkey,
    lamports: u64,
) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }

    let ix = system_instruction::transfer(&from.pubkey(), to, lamports);
    let recent = rpc_client.get_latest_blockhash()?;

    let msg = Message::new(&[ix], Some(&from.pubkey()));
    let tx = Transaction::new(&[from], msg, recent);

    rpc_client
        .send_and_confirm_transaction(&tx)
        .map_err(|e| anyhow!("transfer failed ({} lamports): {e}", lamports))?;

    Ok(())
}

/// Returns a dust_keep that covers:
/// - at least MIN_DUST_KEEP_LAMPORTS
/// - plus rent-exempt minimum for a token account (ATA) if it needs to be created
/// - plus fee buffer
///
/// We compute rent for token account length (165 bytes).
fn compute_dust_keep_lamports(rpc_client: &RpcClient) -> Result<u64> {
  let rent_min = rpc_client.get_minimum_balance_for_rent_exemption(165)?;
    let dust_keep = rent_min
        .saturating_add(TX_FEE_BUFFER_LAMPORTS)
        .max(MIN_DUST_KEEP_LAMPORTS);

    Ok(dust_keep)
}

/// Returns token balance for wallet’s ATA for mint (blocking).
fn get_token_balance_blocking(
    rpc_client: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> u64 {
    let ata = get_associated_token_address(owner, mint);
    match rpc_client.get_token_account_balance(&ata) {
        Ok(bal) => bal.amount.parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Choose K unique wallet indices from temp_wallets.
/// Prefers randomness.
fn choose_active_wallet_indices(
    temp_wallets_len: usize,
    k: usize,
    rng: &mut impl Rng,
) -> Vec<usize> {
    let mut all: Vec<usize> = (0..temp_wallets_len).collect();
    all.shuffle(rng);
    all.truncate(k.min(temp_wallets_len));
    all
}

/// Top-up each active wallet to a computed target balance.
/// Only sends the difference.
/// Target is based on trade sizes, so you can operate even if wallets started at 0.
fn top_up_active_wallets_to_target(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
    active: &[usize],
    dust_keep: u64,
    price_buy_amount_lamports: u64,
    volume_pair_amount_lamports: u64,
) -> Result<u64> {
    // Make the target large enough to:
    // - create ATA if needed (rent)
    // - do multiple buys/sells
    // - never dip below dust_keep
    let typical_trade = price_buy_amount_lamports.max(volume_pair_amount_lamports);

    // Per-wallet target = dust + ~3 trades + buffers
    let target_per_wallet = dust_keep
        .saturating_add(typical_trade.saturating_mul(3))
        .saturating_add(TX_FEE_BUFFER_LAMPORTS)
        .saturating_add(EXTRA_SAFETY_LAMPORTS);

    for &wi in active {
        let w = &temp_wallets[wi];
        let bal = rpc_client.get_balance(&w.pubkey()).unwrap_or(0);

        if bal < target_per_wallet {
            let need = target_per_wallet.saturating_sub(bal);
            transfer_lamports_blocking(rpc_client, payer, &w.pubkey(), need)?;
        }
    }

    Ok(target_per_wallet)
}

/// Refund SOL back to payer leaving dust_keep in each active wallet.
/// (Does NOT sell tokens — you said: only sell tokens if payer is low; keep it simple here.)
fn refund_active_wallets_leave_dust(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
    active: &[usize],
    dust_keep: u64,
) -> Result<()> {
    for &wi in active {
        let w = &temp_wallets[wi];
        let bal = rpc_client.get_balance(&w.pubkey()).unwrap_or(0);

        // Keep dust_keep always
        if bal > dust_keep {
            let refundable = bal.saturating_sub(dust_keep);
            transfer_lamports_blocking(rpc_client, w, &payer.pubkey(), refundable)?;
        }
    }
    Ok(())
}

/// Your function signature MUST remain unchanged — kept exactly.
pub async fn Invest_In_New_Coin_Strategy_loop(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
    mint_pubkey: &Pubkey,
    price_buy_amount_lamports: u64,
    volume_pair_amount_lamports: u64,
    buy_frequency: f64,
    frequency: f64,
    total_iterations: usize,
    main_delay_min_secs: u64,
    main_delay_max_secs: u64,
    short_delay_min_secs: u64,
    short_delay_max_secs: u64,
) -> Result<()> {
    println!("Starting loop...");

    if temp_wallets.is_empty() {
        return Err(anyhow!("temp_wallets is empty"));
    }

    let mut rng = rand::thread_rng();
    let is_local = rpc_client.url().contains("127.0.0.1") || rpc_client.url().contains("localhost");

    //let dust_keep = compute_dust_keep_lamports(rpc_client)?;
    let dust_keep = match compute_dust_keep_lamports(rpc_client) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[WARN] compute_dust_keep_lamports failed: {:#}. Using fallback.", e);
            895_880 + TX_FEE_BUFFER_LAMPORTS // fallback
        }
    };
    println!("dust_keep_lamports = {}", dust_keep);

    // Session state
    let mut active_indices: Vec<usize> = Vec::new();
    let mut session_iters_left: usize = 0;

    // Basic stabilizer so we don’t only buy forever
    let mut buy_minus_sell: i32 = 0;

    for iter in 0..total_iterations {
        println!("Iteration {} / {}", iter + 1, total_iterations);

        // Rotate session if needed
        if session_iters_left == 0 {
            // If we had a previous session, refund it before switching
            if !active_indices.is_empty() {
                println!("Session ended -> refunding active wallets back to payer (leave dust)...");
                //refund_active_wallets_leave_dust(rpc_client, payer, temp_wallets, &active_indices, dust_keep)?;
                if let Err(e) = refund_active_wallets_leave_dust(rpc_client, payer, temp_wallets, &active_indices, dust_keep) {
                    eprintln!(
                        "[WARN] refund_active_wallets_leave_dust failed. active_count={} err={:#}",
                        active_indices.len(),
                        e
                    );

                    // Fallback: clear active set anyway so we don't keep using wallets
                    // that we intended to rotate out.
                    active_indices.clear();

                    // Optional: add a short sleep to avoid hammering if RPC is flaky
                    sleep(Duration::from_millis(500)).await;
                }
            }

            // Pick new active set
            let k = ACTIVE_K.min(temp_wallets.len());
            active_indices = choose_active_wallet_indices(temp_wallets.len(), k, &mut rng);

            session_iters_left = rng.gen_range(SESSION_MIN_ITERS..=SESSION_MAX_ITERS);
            println!(
                "New active set: {} wallets, session_iters_left={}",
                active_indices.len(),
                session_iters_left
            );

            // Fund active set to target
            // let target = top_up_active_wallets_to_target(
            //     rpc_client,
            //     payer,
            //     temp_wallets,
            //     &active_indices,
            //     dust_keep,
            //     price_buy_amount_lamports,
            //     volume_pair_amount_lamports,
            // )?;
            // println!("Funded active wallets to target_per_wallet ≈ {} lamports", target);

            let target_res = top_up_active_wallets_to_target(
            rpc_client,
            payer,
            temp_wallets,
            &active_indices,
            dust_keep,
            price_buy_amount_lamports,
            volume_pair_amount_lamports,
            );

            let target = match target_res {
                Ok(t) => {
                    println!("Funded active wallets to target_per_wallet ≈ {} lamports", t);
                    t
                }
                Err(e) => {
                    eprintln!(
                        "[WARN] top_up_active_wallets_to_target failed. active_count={} err={:#}",
                        active_indices.len(),
                        e
                    );

                    // Fallback strategy:
                    // 1) Keep trading, but your existing per-trade balance checks
                    //    (bal >= amount + fee + dust_keep) will naturally skip wallets
                    //    that aren't funded.
                    // 2) To avoid repeated failures, shorten this session:
                    session_iters_left = 1;

                    // Provide a conservative "target" for logs only
                    0u64
                }
            };
        }

        session_iters_left = session_iters_left.saturating_sub(1);

        // -------------------------
        // Part A: occasional “price buy”
        // -------------------------
        if rng.gen_bool(buy_frequency.clamp(0.0, 1.0)) {
            // pick random active wallet
            let wi = *active_indices.choose(&mut rng).unwrap();
            let w = &temp_wallets[wi];

            // randomized amount
            let amount = (price_buy_amount_lamports as f64 * rng.gen_range(0.8..1.2)) as u64;

            // Balance guard: must have amount + fee buffer and must not dip below dust_keep
            let bal = rpc_client.get_balance(&w.pubkey()).unwrap_or(0);
            let required = amount
                .saturating_add(TX_FEE_BUFFER_LAMPORTS)
                .saturating_add(dust_keep);

            if bal < required {
                println!(
                    "PRICE BUY skip: wallet {} bal={} < required={}",
                    w.pubkey(),
                    bal,
                    required
                );
            } else {
                println!("PRICE BUY wallet {} amount={} lamports", w.pubkey(), amount);

                if is_local {
                    println!("  [LOCAL] simulated buy");
                } else {
                    // YOU SAID wrappers already exist — use them:
                   // crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w, mint_pubkey, amount).await?;
                    let res = crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w, mint_pubkey, amount).await;
                    match res {
                        Ok(_) => {
                            buy_minus_sell += 1;
                        }
                        Err(e) => {
                            eprintln!(
                                "[WARN] PRICE BUY failed wallet={} amount={} err={:#}",
                                w.pubkey(),
                                amount,
                                e
                            );
                            // keep going (DO NOT return)
                        }
                    }
                }

                buy_minus_sell += 1;
            }
        }

        // -------------------------
        // Part B: “volume pair” (2-leg)
        // -------------------------
        if rng.gen_bool(frequency.clamp(0.0, 1.0)) {
            // If we’ve net-bought too much, bias toward selling first
            let buy_first_prob: f64 = if buy_minus_sell > 2 { 0.30 } else { 0.70 };
            let buy_first = rng.gen_bool(buy_first_prob);

            // Choose two distinct wallets from active set
            if active_indices.len() < 2 {
                println!("Volume pair skipped: need >=2 active wallets.");
            } else {
                let mut pair = active_indices.clone();
                pair.shuffle(&mut rng);
                let wi1 = pair[0];
                let wi2 = pair[1];

                let w1 = &temp_wallets[wi1];
                let w2 = &temp_wallets[wi2];

                let short_delay = rng.gen_range(short_delay_min_secs..=short_delay_max_secs).max(1);

                // Helper closures for buy/sell checks
                let can_buy = |wallet: &Keypair, lamports_in: u64| -> bool {
                    let bal = rpc_client.get_balance(&wallet.pubkey()).unwrap_or(0);
                    let required = lamports_in
                        .saturating_add(TX_FEE_BUFFER_LAMPORTS)
                        .saturating_add(dust_keep);
                    bal >= required
                };

                let can_sell = |wallet: &Keypair, token_amt: u64| -> bool {
                    let bal = rpc_client.get_balance(&wallet.pubkey()).unwrap_or(0);
                    let tok = get_token_balance_blocking(rpc_client, &wallet.pubkey(), mint_pubkey);
                    // Need token_amt tokens AND enough SOL to pay tx fee while keeping dust
                    tok >= token_amt && bal >= dust_keep.saturating_add(TX_FEE_BUFFER_LAMPORTS)
                };

                if buy_first {
                    // BUY then SELL
                    let buy_amt = (volume_pair_amount_lamports as f64 * rng.gen_range(0.8..1.2)) as u64;

                    if can_buy(w1, buy_amt) {
                        println!("VOLUME BUY #1 wallet {} amount={}", w1.pubkey(), buy_amt);

                        if !is_local {
                           // crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w1, mint_pubkey, buy_amt).await?;
                            let res = crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w1, mint_pubkey, buy_amt).await;
                            if let Err(e) = res {
                                eprintln!(
                                    "[WARN] VOLUME BUY #1 failed wallet={} amount={} err={:#}",
                                    w1.pubkey(),
                                    buy_amt,
                                    e
                                );
                            } else {
                                buy_minus_sell += 1;
                            }
                        }else{
                            buy_minus_sell += 1;
                        }
                      
                    } else {
                        println!("VOLUME BUY #1 skip: insufficient SOL in {}", w1.pubkey());
                    }

                    sleep(Duration::from_secs(short_delay)).await;

                    // Sell a small-ish token amount (simple: sell a random fraction of what they hold)
                    let w2_tok = get_token_balance_blocking(rpc_client, &w2.pubkey(), mint_pubkey);
                    if w2_tok == 0 {
                        println!("VOLUME SELL #2 skip: wallet {} has 0 tokens", w2.pubkey());
                    } else {
                        let sell_amt = (w2_tok as f64 * rng.gen_range(0.05..0.20)).max(1.0) as u64;
                        if can_sell(w2, sell_amt) {
                            println!("VOLUME SELL #2 wallet {} token_amount={}", w2.pubkey(), sell_amt);

                            if !is_local {
                                //crate::pumpfun::sell_on_curve_async_from_client(rpc_client, w2, mint_pubkey, sell_amt).await?;
                                let res = crate::pumpfun::sell_on_curve_async_from_client(rpc_client, w2, mint_pubkey, sell_amt).await;
                                if let Err(e) = res {
                                    eprintln!(
                                        "[WARN] VOLUME SELL #2 failed wallet={} token_amount={} err={:#}",
                                        w2.pubkey(),
                                        sell_amt,
                                        e
                                    );
                                } else {
                                    buy_minus_sell -= 1;
                                }
                            }else{
                                buy_minus_sell -= 1;
                            }
                        } else {
                            println!("VOLUME SELL #2 skip: insufficient token/SOL in {}", w2.pubkey());
                        }
                    }
                } else {
                    // SELL then BUY
                    let w1_tok = get_token_balance_blocking(rpc_client, &w1.pubkey(), mint_pubkey);
                    if w1_tok == 0 {
                        println!("VOLUME SELL #1 skip: wallet {} has 0 tokens", w1.pubkey());
                    } else {
                        let sell_amt = (w1_tok as f64 * rng.gen_range(0.05..0.20)).max(1.0) as u64;
                        if can_sell(w1, sell_amt) {
                            println!("VOLUME SELL #1 wallet {} token_amount={}", w1.pubkey(), sell_amt);

                            if !is_local {
                                //crate::pumpfun::sell_on_curve_async_from_client(rpc_client, w1, mint_pubkey, sell_amt).await?;
                                let res = crate::pumpfun::sell_on_curve_async_from_client(rpc_client, w1, mint_pubkey, sell_amt).await;
                                if let Err(e) = res {
                                    eprintln!(
                                        "[WARN] VOLUME SELL #1 failed wallet={} token_amount={} err={:#}",
                                        w1.pubkey(),
                                        sell_amt,
                                        e
                                    );
                                } else {
                                    buy_minus_sell -= 1;
                                }
                            }else{
                                buy_minus_sell -= 1;
                            }
                        } else {
                            println!("VOLUME SELL #1 skip: insufficient token/SOL in {}", w1.pubkey());
                        }
                    }

                    sleep(Duration::from_secs(short_delay)).await;

                    let buy_amt = (volume_pair_amount_lamports as f64 * rng.gen_range(0.8..1.2)) as u64;
                    if can_buy(w2, buy_amt) {
                        println!("VOLUME BUY #2 wallet {} amount={}", w2.pubkey(), buy_amt);

                        if !is_local {
                           // crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w2, mint_pubkey, buy_amt).await?;
                           let res = crate::pumpfun::buy_on_curve_async_from_client(rpc_client, w2, mint_pubkey, buy_amt).await;
                            if let Err(e) = res {
                                eprintln!(
                                    "[WARN] VOLUME BUY #2 failed wallet={} amount={} err={:#}",
                                    w2.pubkey(),
                                    buy_amt,
                                    e
                                );
                            } else {
                                buy_minus_sell += 1;
                            }
                        }else{
                            buy_minus_sell += 1;
                        }
                    } else {
                        println!("VOLUME BUY #2 skip: insufficient SOL in {}", w2.pubkey());
                    }
                }
            }
        }

        // Main delay between iterations
        let delay_secs = rng.gen_range(main_delay_min_secs..=main_delay_max_secs).max(1);
        println!("Sleeping for {} seconds...", delay_secs);
        sleep(Duration::from_secs(delay_secs)).await;
    }

    // Final refund for the last session
    if !active_indices.is_empty() {
        println!("Loop finished -> refunding active wallets (leave dust)...");
        //refund_active_wallets_leave_dust(rpc_client, payer, temp_wallets, &active_indices, dust_keep)?;
        if let Err(e) = refund_active_wallets_leave_dust(rpc_client, payer, temp_wallets, &active_indices, dust_keep) {
            eprintln!("[WARN] final refund_active_wallets_leave_dust failed: {:#}", e);
            // Do NOT return Err; allow function to finish so main cleanup runs.
        }

    }

    println!("Loop complete!");
    Ok(())
}


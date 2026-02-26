use crate::pumpfun::sell_on_curve;
use crate::pumpfun::sell_on_curve_async;
use crate::pumpfun::sell_on_curve_async_from_client;


use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use solana_client::rpc_client::RpcClient;
use anyhow::Result;
use std::str::FromStr;
use anyhow::Context;
use solana_sdk::message::Message;

use spl_associated_token_account::get_associated_token_address;
use std::collections::HashSet;
use solana_program::program_pack::Pack;




/// Fetch raw token amount in the owner's ATA for `mint`.
/// Returns 0 if ATA doesn't exist or has no balance.
///
/// token_amount here is in **raw base units** (smallest units),
/// NOT SOL and NOT lamports. (For decimals=6, 1 token = 1_000_000 raw units)
fn get_ata_token_amount_raw(
    rpc_client: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    let ata = get_associated_token_address(owner, mint);

    // If ATA doesn't exist -> 0
    let ata_acc = match rpc_client.get_account(&ata) {
        Ok(a) => a,
        Err(_) => return Ok(0),
    };

    // Fast, reliable: use the SPL Token account layout directly.
    // spl_token::state::Account stores amount as u64.
    let token_acc = spl_token::state::Account::unpack(&ata_acc.data)?;
    Ok(token_acc.amount)
}




pub async fn perform_sell_all(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
    mint_pubkey: &Pubkey,
) -> Result<()> {
    println!("Performing sell all (payer + temp wallets)...");

    let is_local = rpc_client.url().contains("127.0.0.1") || rpc_client.url().contains("localhost");
    let mint_decimals = get_mint_decimals(rpc_client, mint_pubkey)?;

      // -------------------------
    // BEFORE balances
    // -------------------------
    println!("--- Balances BEFORE sell ---");
    print_wallet_balances(rpc_client, "payer", &payer.pubkey(), mint_pubkey, mint_decimals)?;
    for (i, w) in temp_wallets.iter().enumerate() {
        print_wallet_balances(rpc_client, &format!("temp[{}]", i), &w.pubkey(), mint_pubkey, mint_decimals)?;
    }
    println!("----------------------------");

    // Avoid double-selling if payer also appears in temp_wallets
    let mut seen: HashSet<Pubkey> = HashSet::new();

    // Helper closure to sell a single wallet
    let mut sell_wallet = |wallet: &Keypair| -> Result<u64> {
        if !seen.insert(wallet.pubkey()) {
            return Ok(0);
        }
        let amount_raw = get_ata_token_amount_raw(rpc_client, &wallet.pubkey(), mint_pubkey)?;
        Ok(amount_raw)
    };

    // 1) payer
    let payer_amount_raw = sell_wallet(payer)?;
    if payer_amount_raw == 0 {
        println!("  Payer has 0 tokens for mint {}, skipping.", mint_pubkey);
    } else if is_local {
        println!("  [LOCAL] Simulated sell from payer {}: amount_raw={}", payer.pubkey(), payer_amount_raw);
    } else {
        println!("  Selling from payer {}: amount_raw={}", payer.pubkey(), payer_amount_raw);
        sell_on_curve_async_from_client(rpc_client, payer, mint_pubkey, payer_amount_raw).await?;
    }

    // 2) temp wallets
    for wallet in temp_wallets {
        let amt_raw = sell_wallet(wallet)?;
        if amt_raw == 0 {
            println!("  Wallet {} has 0 tokens, skipping.", wallet.pubkey());
            continue;
        }

        if is_local {
            println!("  [LOCAL] Simulated sell from wallet {}: amount_raw={}", wallet.pubkey(), amt_raw);
        } else {
            println!("  Selling from wallet {}: amount_raw={}", wallet.pubkey(), amt_raw);
            sell_on_curve_async_from_client(rpc_client, wallet, mint_pubkey, amt_raw).await?;
        }
    }

    // -------------------------
    // AFTER balances
    // -------------------------
    println!("--- Balances AFTER sell ---");
    print_wallet_balances(rpc_client, "payer", &payer.pubkey(), mint_pubkey, mint_decimals)?;
    for (i, w) in temp_wallets.iter().enumerate() {
        print_wallet_balances(rpc_client, &format!("temp[{}]", i), &w.pubkey(), mint_pubkey, mint_decimals)?;
    }
    println!("---------------------------");

    println!("Sell all complete!");
    Ok(())
}

fn print_wallet_balances(
    rpc_client: &RpcClient,
    label: &str,
    owner: &Pubkey,
    mint: &Pubkey,
    mint_decimals: u8,
) -> Result<u64> {
    let sol_lamports = rpc_client.get_balance(owner)?;
    let sol = (sol_lamports as f64) / 1_000_000_000.0;

    let ata = get_associated_token_address(owner, mint);

    let token_raw: u64 = match rpc_client.get_account(&ata) {
        Ok(ata_acc) => {
            let token_acc = spl_token::state::Account::unpack(&ata_acc.data)?;
            token_acc.amount
        }
        Err(_) => 0,
    };

    let denom = 10u64.pow(mint_decimals as u32) as f64;
    let token_human = (token_raw as f64) / denom;

    println!(
        "[{}] owner={} SOL={:.9} | ATA={} | token_raw={} token_human={:.6}",
        label, owner, sol, ata, token_raw, token_human
    );

    Ok(token_raw)
}

fn get_mint_decimals(rpc_client: &RpcClient, mint: &Pubkey) -> Result<u8> {
    let mint_acc = rpc_client.get_account(mint)?;
    let mint_state = spl_token::state::Mint::unpack(&mint_acc.data)?;
    Ok(mint_state.decimals)
}




pub async fn refund_temp_wallets_to_payer(
    rpc_client: &RpcClient,
    payer: &Keypair,
    temp_wallets: &[Keypair],
) -> Result<()> {
    println!("Refunding SOL from temp wallets back to payer...");

    let payer_pubkey = payer.pubkey();

    // Buffer for tx fees. If you ever set CU price / priority fees for refunds,
    // bump this (e.g. 25_000–50_000).
    let fee_buffer: u64 = 10_000;

    for wallet in temp_wallets {
        let wallet_pubkey = wallet.pubkey();

        // Pull full account so we can compute rent min for its real data length
        let acct = match rpc_client.get_account(&wallet_pubkey) {
            Ok(a) => a,
            Err(_) => {
                // If account doesn't exist on-chain, balance is effectively 0
                println!("  Wallet {} has no on-chain account — skipping", wallet_pubkey);
                continue;
            }
        };

        let balance_lamports = acct.lamports;
        let data_len = acct.data.len();

        if balance_lamports == 0 {
            println!("  Wallet {} has 0 lamports — skipping", wallet_pubkey);
            continue;
        }

            println!(
    "refund-check wallet={} owner={} data_len={} lamports={}",
    wallet_pubkey,
    acct.owner,
    acct.data.len(),
    acct.lamports
    );
        // Rent-exempt minimum depends on data length
        let rent_min = rpc_client
            .get_minimum_balance_for_rent_exemption(data_len)
            .with_context(|| format!("Failed to get rent-exempt minimum for data_len={}", data_len))?;

        // ✅ Keep enough to remain rent-exempt + pay fees
        let keep_lamports = rent_min.saturating_add(fee_buffer);

        if balance_lamports <= keep_lamports {
            println!(
                "  Wallet {} balance={} lamports ({} SOL) too small to refund. \
                 data_len={} rent_min={} keep={} (rent+fee)",
                wallet_pubkey,
                balance_lamports,
                (balance_lamports as f64) / 1_000_000_000.0,
                data_len,
                rent_min,
                keep_lamports
            );
            continue;
        }

        let send_amount = balance_lamports - keep_lamports;

        println!(
            "  Wallet {}: balance={} ({} SOL), data_len={}, rent_min={}, sending={}, keeping={}",
            wallet_pubkey,
            balance_lamports,
            (balance_lamports as f64) / 1_000_000_000.0,
            data_len,
            rent_min,
            send_amount,
            keep_lamports
        );

        let ix = system_instruction::transfer(&wallet_pubkey, &payer_pubkey, send_amount);

        let recent_blockhash = rpc_client.get_latest_blockhash()?;
        let message = Message::new(&[ix], Some(&wallet_pubkey));
        let tx = Transaction::new(&[wallet], message, recent_blockhash);

        let sig = rpc_client.send_and_confirm_transaction(&tx)?;
        println!("    Refund tx sig: {}", sig);

        // ✅ Print balances AFTER
        let new_wallet_bal = rpc_client
            .get_balance(&wallet_pubkey)
            .with_context(|| format!("Failed to re-check balance for wallet {}", wallet_pubkey))?;
        let new_payer_bal = rpc_client
            .get_balance(&payer_pubkey)
            .with_context(|| format!("Failed to re-check payer balance {}", payer_pubkey))?;

        println!(
            "    AFTER: wallet={} lamports ({} SOL) | payer={} lamports ({} SOL)",
            new_wallet_bal,
            (new_wallet_bal as f64) / 1_000_000_000.0,
            new_payer_bal,
            (new_payer_bal as f64) / 1_000_000_000.0
        );
    }

    println!("Refund complete!");
    Ok(())
}







// pub async fn perform_sell_all(
//     rpc_client: &RpcClient,
//     payer: &Keypair,
//     temp_wallets: &[Keypair],
//     mint_pubkey: &Pubkey,
// ) -> Result<()> {
//     println!("Performing sell all (payer + temp wallets)...");

//     let is_local = rpc_client.url().contains("127.0.0.1") || rpc_client.url().contains("localhost");

//     // Sell from payer
//     if !is_local {
//         println!("  Simulated sell from payer: full balance");
//     } else {
//         sell_on_curve(rpc_client, payer, mint_pubkey, 1_000_000_000).await?;  // full balance placeholder
//     }

//     // Sell from each temp wallet
//     for wallet in temp_wallets {
//         if !is_local {
//             println!("  Simulated sell from wallet: {}", wallet.pubkey());
//         } else {
//             sell_on_curve(rpc_client, wallet, mint_pubkey, 1_000_000_000).await?;  // full balance placeholder
//         }
//     }

//     println!("Sell all complete!");
//     Ok(())
// }






// pub async fn refund_temp_wallets_to_payer(
//     rpc_client: &RpcClient,
//     payer: &Keypair,
//     temp_wallets: &[Keypair],
// ) -> Result<()> {
//     println!("Refunding all SOL from temp wallets back to payer...");

//     let payer_pubkey = payer.pubkey();
//    // let rent_exempt_minimum = 890_880;  // Rent-exempt minimum (approx)
//     // let rent_exempt_minimum = rpc_client
//     //     .get_minimum_balance_for_rent_exemption(0)
//     //     .context("Failed to fetch rent-exempt minimum")?;

//     let fee_buffer: u64 = 10_000;
//     for wallet in temp_wallets {
//         let wallet_pubkey = wallet.pubkey();

//         let balance_lamports = rpc_client.get_balance(&wallet_pubkey)
//             .with_context(|| format!("Failed to get balance for wallet {}", wallet_pubkey))?;

//         if balance_lamports == 0 {
//             println!("  Wallet {} has 0 SOL — skipping", wallet_pubkey);
//             continue;
//         }

//         let send_amount = if balance_lamports > rent_exempt_minimum + 10_000 {
//             balance_lamports - rent_exempt_minimum - 10_000
//         } else {
//             println!("  Wallet {} has too little SOL ({}) — skipping refund", wallet_pubkey, balance_lamports);
//             continue;
//         };

//         let ix = solana_sdk::system_instruction::transfer(
//             &wallet_pubkey,
//             &payer_pubkey,
//             send_amount,
//         );

//         let recent_blockhash = rpc_client.get_latest_blockhash()?;
//         let message = Message::new(&[ix], Some(&wallet_pubkey));
//         let tx = Transaction::new(&[wallet], message, recent_blockhash);

//         let sig = rpc_client.send_and_confirm_transaction(&tx)?;
//         println!("  Refunded {} lamports from {} to payer. Signature: {}", send_amount, wallet_pubkey, sig);
//     }

//     println!("Refund complete! All eligible SOL sent back to payer.");
//     Ok(())
// }
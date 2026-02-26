use crate::pumpfun::buy_on_curve;
use crate::pumpfun::buy_on_curve_async;
use crate::pumpfun::buy_on_curve_async_from_client;


use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_client::rpc_client::RpcClient;
use anyhow::Result;

pub async fn perform_initial_buy(
    rpc_client: &RpcClient,
    payer: &Keypair,
    mint_pubkey: &Pubkey,
    amount_lamports: u64,
) -> Result<()> {
    let is_local = rpc_client.url().contains("127.0.0.1") || rpc_client.url().contains("localhost");

    if is_local {
        println!("  Simulated initial buy: {} lamports", amount_lamports);
        return Ok(());
    }

    println!("Performing real initial buy: {} lamports", amount_lamports);  
    println!("Attempting buy for mint {}", mint_pubkey);
    buy_on_curve_async_from_client(rpc_client, payer, mint_pubkey, amount_lamports).await?;

    println!("Initial buy complete!");

    Ok(())
}
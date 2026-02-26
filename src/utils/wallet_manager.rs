use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use solana_client::rpc_client::RpcClient;
use anyhow::{Context, Result};
use rand::seq::SliceRandom;

pub struct WalletManager {
    pub payer: Keypair,
    pub temp_wallets: Vec<Keypair>,
    pub rpc_client: RpcClient,
}

impl WalletManager {
    pub fn new(payer: Keypair, rpc_url: &str, num_wallets: usize, fund_amount_lamports: u64) -> Result<Self> {
        let rpc_client = RpcClient::new(rpc_url.to_string());

        // Generate temp wallets
        let mut temp_wallets = Vec::with_capacity(num_wallets);
        for _ in 0..num_wallets {
            temp_wallets.push(Keypair::new());
        }

        println!("Funding each of {} temp wallets with {} lamports ({} SOL)", 
         num_wallets, 
         fund_amount_lamports, 
         fund_amount_lamports as f64 / 1_000_000_000.0);
         
        // Batch fund them (send one tx per ~20 wallets to avoid size limits)
        let mut i = 0;
        while i < temp_wallets.len() {
            let mut instructions = vec![];
            let batch_end = (i + 20).min(temp_wallets.len());

            for j in i..batch_end {
                let wallet = &temp_wallets[j];
                instructions.push(system_instruction::transfer(
                    &payer.pubkey(),
                    &wallet.pubkey(),
                    fund_amount_lamports,
                ));
            }

            let recent_blockhash = rpc_client.get_latest_blockhash()?;
            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&payer.pubkey()),
                &[&payer],
                recent_blockhash,
            );

            let sig = rpc_client.send_and_confirm_transaction(&tx)?;
            println!("Funded batch {}–{}: {}", i, batch_end - 1, sig);

            i = batch_end;
        }

       
        println!("Generated {} temp wallets and funded them with {} lamports each.", num_wallets, fund_amount_lamports);

        // Now query and print balances for all temp wallets
        println!("All funded temp wallets (with balances):");
        for (i, wallet) in temp_wallets.iter().enumerate() {
            let pubkey = wallet.pubkey();

            // Get SOL balance (in lamports)
            let balance_lamports = rpc_client.get_balance(&pubkey)
                .context("Failed to get balance for temp wallet")?;

            // Convert to SOL (divide by 1_000_000_000)
            let balance_sol = balance_lamports as f64 / 1_000_000_000.0;

            println!("Wallet #{}: {} | Balance: {:.9} SOL ({} lamports)", 
                    i + 1, 
                    pubkey, 
                    balance_sol, 
                    balance_lamports);
        }


        Ok(Self {
            payer,
            temp_wallets,
            rpc_client,
        })
    }

    pub fn select_random_wallet(&self) -> &Keypair {
        self.temp_wallets.choose(&mut rand::thread_rng()).unwrap()
    }

    pub fn select_least_used_wallet(&self) -> &Keypair {
        // Simple round-robin for now (can improve with usage tracking later)
        self.temp_wallets.choose(&mut rand::thread_rng()).unwrap()
    }

    pub fn get_payer_pubkey(&self) -> Pubkey {
        self.payer.pubkey()
    }
}
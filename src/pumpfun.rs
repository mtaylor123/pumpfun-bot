use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    system_instruction,
    
    transaction::Transaction,
    sysvar::rent::Rent,
};
use solana_client::rpc_client::RpcClient;
use anyhow::{anyhow, Context, Result};
use std::str::FromStr;
use spl_token::ID as SPL_TOKEN_ID;
use spl_token_2022::ID as SPL_TOKEN_2022_ID;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_associated_token_account::{get_associated_token_address, instruction::create_associated_token_account};
use solana_program::program_pack::Pack;
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use borsh::{BorshDeserialize, BorshSerialize};

use std::sync::Arc;
use tokio::task;




//6EF8rrecthR5Dkzon8NwuQ15o2BpU89hT3GiQGkR9Tw

const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_FEE_PROGRAM_ID: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
const BUY_EXACT_SOL_IN_DISCRIMINATOR: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];

const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_FEE_PROGRAM_ID_STR: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";


// Offset helpers (Anchor account layouts)
// BondingCurve fields: 5*u64 + bool + creator pubkey ...
// creator starts after: 8(discriminator) + 40(u64*5) + 1(bool) = 49
const BONDING_CURVE_CREATOR_OFFSET: usize = 49;


// Global fields: bool + authority pubkey + fee_recipient pubkey ...

// Global account layout offsets (INCLUDING the 8-byte Anchor discriminator)
const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 41; // you already have this
const GLOBAL_FEE_BASIS_POINTS_OFFSET: usize = 105;
const GLOBAL_CREATOR_FEE_BASIS_POINTS_OFFSET: usize = 154;

// GlobalVolumeAccumulator size:
// discriminator (8) + struct size (536) = 544
const GLOBAL_VOLUME_ACC_SPACE: usize = 544;



// --------------------
// Anchor account structs (from pump.json `types`)
// NOTE: Anchor adds an 8-byte discriminator prefix in account data.
// We strip that before BorshDeserialize.
// --------------------
#[derive(BorshDeserialize, Debug)]
pub struct Global {
    pub initialized: bool,                    // Unused
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub initial_virtual_token_reserves: u64,
    pub initial_virtual_sol_reserves: u64,
    pub initial_real_token_reserves: u64,
    pub token_total_supply: u64,
    pub fee_basis_points: u64,
    pub withdraw_authority: Pubkey,
    pub enable_migrate: bool,                 // Unused
    pub pool_migration_fee: u64,
    pub creator_fee_basis_points: u64,
    pub fee_recipients: [Pubkey; 7],
    pub set_creator_authority: Pubkey,
    pub admin_set_creator_authority: Pubkey,
    pub create_v2_enabled: bool,
    pub whitelist_pda: Pubkey,
    pub reserved_fee_recipient: Pubkey,
    pub mayhem_mode_enabled: bool,
    pub reserved_fee_recipients: [Pubkey; 7],
}

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

// --------------------
// IDL-defined OptionBool (pump.json shows a struct with a single bool)
// --------------------
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy)]
pub struct OptionBool(pub bool);

// --------------------
// Anchor instruction args for buy_exact_sol_in
// discriminator + borsh(u64, u64, OptionBool)
// --------------------
#[derive(BorshSerialize, Debug)]
struct BuyExactSolInArgs {
    spendable_sol_in: u64,
    min_tokens_out: u64,
    track_volume: OptionBool,
}

// --------------------
// Anchor instruction args for init_user_volume_accumulator (no args)
// discriminator only
// --------------------

// --------------------
// Helpers
// --------------------
fn pda(program_id: &Pubkey, seeds: &[&[u8]]) -> (Pubkey, u8) {
    Pubkey::find_program_address(seeds, program_id)
}

fn strip_anchor_discriminator(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 8 {
        return Err(anyhow!("account data too short for discriminator"));
    }
    Ok(&data[8..])
}

// Quote formulas are documented in your IDL for buy_exact_sol_in.
// This implements the SOL→tokens steps shown there.
fn quote_tokens_out_for_spendable_sol(
    spendable_sol_in: u64,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    protocol_fee_bps: u64,
    creator_fee_bps: u64,
) -> Result<(u64, u64)> {
    let total_fee_bps = protocol_fee_bps
        .checked_add(creator_fee_bps)
        .ok_or_else(|| anyhow!("fee bps overflow"))?;

    if spendable_sol_in == 0 {
        return Err(anyhow!("spendable_sol_in is zero (would trigger BuyZeroAmount)"));
    }

    // 1) net_sol = floor(spendable_sol_in * 10_000 / (10_000 + total_fee_bps))
    let denom = 10_000u64
        .checked_add(total_fee_bps)
        .ok_or_else(|| anyhow!("fee denom overflow"))?;

    let net_sol_initial = spendable_sol_in
        .saturating_mul(10_000)
        / denom;

    // Helper: ceil(a/b) = (a + b - 1)/b
    let ceil_div = |a: u64, b: u64| -> u64 {
        if b == 0 { return 0; }
        (a + b - 1) / b
    };

    // 2) fees = ceil(net_sol * protocol_fee_bps / 10_000) + ceil(net_sol * creator_fee_bps / 10_000)
    let protocol_fee = ceil_div(net_sol_initial.saturating_mul(protocol_fee_bps), 10_000);
    let creator_fee = ceil_div(net_sol_initial.saturating_mul(creator_fee_bps), 10_000);
    let mut fees = protocol_fee.saturating_add(creator_fee);

    // 3) if net_sol + fees > spendable_sol_in: net_sol = net_sol - (net_sol + fees - spendable_sol_in)
    let mut net_sol = net_sol_initial;
    if net_sol.saturating_add(fees) > spendable_sol_in {
        let over = net_sol.saturating_add(fees).saturating_sub(spendable_sol_in);
        net_sol = net_sol.saturating_sub(over);

        // recompute fees with adjusted net_sol (keeps behavior closer to on-chain intent)
        let protocol_fee2 = ceil_div(net_sol.saturating_mul(protocol_fee_bps), 10_000);
        let creator_fee2 = ceil_div(net_sol.saturating_mul(creator_fee_bps), 10_000);
        fees = protocol_fee2.saturating_add(creator_fee2);
    }

    // 4) tokens_out = floor((net_sol - 1) * virtual_token_reserves / (virtual_sol_reserves + net_sol - 1))
    if net_sol <= 1 {
        return Err(anyhow!(
            "net_sol <= 1 after fees; tokens_out would be 0 (BuyZeroAmount risk). Increase amount_lamports."
        ));
    }

    let net_minus_1 = net_sol - 1;

    let numerator = (net_minus_1 as u128)
        .saturating_mul(virtual_token_reserves as u128);

    let denominator = (virtual_sol_reserves as u128)
        .saturating_add(net_minus_1 as u128);

    if denominator == 0 {
        return Err(anyhow!("invalid reserves: denominator is zero"));
    }

    let tokens_out = (numerator / denominator) as u64;

    Ok((tokens_out, fees))
}




fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    if data.len() < offset + 32 {
        return Err(anyhow!("account data too short to read pubkey at offset {}", offset));
    }
    Ok(Pubkey::new(&data[offset..offset + 32]))
}

/// Best-effort encoding for OptionBool used by pump IDL.
/// Many Anchor programs encode Option<T> as: 1-byte tag (0/1) then T.
/// If pump actually uses a different layout, we can adjust this quickly.
fn encode_option_bool(v: bool) -> [u8; 1] {
    [if v { 1 } else { 0 }]
}




fn short(pk: &solana_sdk::pubkey::Pubkey) -> String {
    let s = pk.to_string();
    format!("{}…{}", &s[..4], &s[s.len()-4..])
}

fn print_ixs(label: &str, ixs: &[solana_sdk::instruction::Instruction]) {
    println!("--- {}: {} instructions ---", label, ixs.len());
    for (i, ix) in ixs.iter().enumerate() {
        println!(
            "ix[{}] program={} accounts={} data_len={}",
            i,
            short(&ix.program_id),
            ix.accounts.len(),
            ix.data.len()
        );
        // Optional: show which accounts are writable/signer
        for (j, am) in ix.accounts.iter().enumerate() {
            println!(
                "  acct[{}] {} writable={} signer={}",
                j,
                short(&am.pubkey),
                am.is_writable,
                am.is_signer
            );
        }
    }
    println!("-------------------------------");
}

fn print_key_accounts(
    global: &Pubkey,
    bonding_curve: &Pubkey,
    assoc_user: &Pubkey,
    assoc_curve: &Pubkey,
    creator_vault: &Pubkey,
    fee_config: &Pubkey,
    fee_program: &Pubkey,
    uva: &Pubkey,
    gva: &Pubkey,
) {
    println!("--- Key Accounts ---");
    println!("global                 = {}", global);
    println!("bonding_curve          = {}", bonding_curve);
    println!("associated_user_ata    = {}", assoc_user);
    println!("associated_curve_ata   = {}", assoc_curve);
    println!("creator_vault          = {}", creator_vault);
    println!("fee_config             = {}", fee_config);
    println!("fee_program            = {}", fee_program);
    println!("user_volume_accumulator= {}", uva);
    println!("global_volume_accum    = {}", gva);
    println!("---------------------");
}

fn simulate_and_print(rpc_client: &solana_client::rpc_client::RpcClient, tx: &Transaction) -> anyhow::Result<()> {
    use solana_client::rpc_config::RpcSimulateTransactionConfig;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::transaction::TransactionError;

    println!("--- Simulating transaction ---");
    let cfg = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        accounts: None,
        min_context_slot: None,
        inner_instructions: true,
        ..RpcSimulateTransactionConfig::default()
    };

    let sim = rpc_client.simulate_transaction_with_config(tx, cfg)?;

    // Print units if provided
    if let Some(units) = sim.value.units_consumed {
        println!("units_consumed: {}", units);
    } else {
        println!("units_consumed: <none returned>");
    }

    // Print logs
    if let Some(logs) = sim.value.logs {
        println!("--- logs ({} lines) ---", logs.len());
        for l in &logs {
            println!("{}", l);
        }
        println!("--- end logs ---");
    } else {
        println!("logs: <none returned>");
    }

    // Print error
    if let Some(err) = sim.value.err {
        println!("SIM ERROR: {:?}", err);
        // if you want: try to pretty-print
    } else {
        println!("SIM: OK");
    }

    // Print inner instructions count (if present)
    if let Some(inner) = sim.value.inner_instructions {
        println!("inner_instructions sets: {}", inner.len());
    }

    Ok(())
}



pub fn create_pumpfun_token(
    rpc_client: &RpcClient,
    payer: &Keypair,
    name: &str,
    symbol: &str,
    uri: &str,
) -> Result<Pubkey> {
    use std::str::FromStr;

    use anyhow::anyhow;
    use solana_client::rpc_config::RpcSimulateTransactionConfig;
    use solana_sdk::{
        commitment_config::CommitmentConfig,
        instruction::{AccountMeta, Instruction},
        message::Message,
        pubkey::Pubkey,
        sysvar::rent as rent_sysvar,
        system_program,
        transaction::Transaction,
    };
    use spl_associated_token_account::get_associated_token_address;

    // -----------------------------
    // MAINNET guard (keep if you want)
    // -----------------------------
    let url = rpc_client.url();
    let is_mainnet = url.contains("mainnet");
    if !is_mainnet {
        return Err(anyhow!(
            "Refusing to create Pump.fun token: RPC is not mainnet. rpc_url={}",
            url
        ));
    }

    // -----------------------------
    // Program IDs from IDL
    // -----------------------------
    // pump.json "address" :contentReference[oaicite:4]{index=4}
    let pump_program = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")?;

    // Metaplex Token Metadata program (also in IDL accounts)
    let token_metadata_program =
        Pubkey::from_str("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s")?;

    // Token program for `create` is classic SPL Token in the IDL
    // :contentReference[oaicite:5]{index=5}
    let token_program = spl_token::ID;

    // Associated token program (standard)
    let ata_program = spl_associated_token_account::ID;

    // -----------------------------
    // Create mint keypair (signer account)
    // -----------------------------
    let mint = Keypair::new();
    let mint_pubkey = mint.pubkey();

    // -----------------------------
    // PDAs per IDL
    // global PDA seed: "global" :contentReference[oaicite:6]{index=6}
    let (global_pda, _) =
        Pubkey::find_program_address(&[b"global"], &pump_program);

    // mint_authority PDA seed: "mint-authority" :contentReference[oaicite:7]{index=7}
    let (mint_authority_pda, _) =
        Pubkey::find_program_address(&[b"mint-authority"], &pump_program);

    // bonding_curve PDA seed: "bonding-curve", mint :contentReference[oaicite:8]{index=8}
    let (bonding_curve_pda, _) =
        Pubkey::find_program_address(&[b"bonding-curve", mint_pubkey.as_ref()], &pump_program);

    // associated_bonding_curve = ATA(owner=bonding_curve, mint=mint)
    // IDL shows this is derived from bonding_curve + token_program + mint (classic ATA pattern)
    // :contentReference[oaicite:9]{index=9}
    let associated_bonding_curve_ata = get_associated_token_address(&bonding_curve_pda, &mint_pubkey);

    // metadata PDA seeds: ["metadata", mpl_token_metadata_program_id, mint]
    // :contentReference[oaicite:10]{index=10}
    let (metadata_pda, _) = Pubkey::find_program_address(
        &[b"metadata", token_metadata_program.as_ref(), mint_pubkey.as_ref()],
        &token_metadata_program,
    );

    // event_authority PDA seed: "__event_authority" :contentReference[oaicite:11]{index=11}
    let (event_authority_pda, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &pump_program);

    // -----------------------------
    // Instruction data = discriminator + args (Anchor/Borsh style)
    // create discriminator: [24,30,200,40,5,28,7,119]
    // :contentReference[oaicite:12]{index=12}
    // args: name:string, symbol:string, uri:string, creator:pubkey
    // :contentReference[oaicite:13]{index=13}
    // -----------------------------
    let mut data: Vec<u8> = vec![24, 30, 200, 40, 5, 28, 7, 119];

    // Anchor encodes string as: u32 little-endian length + UTF-8 bytes
    fn push_anchor_string(dst: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        dst.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        dst.extend_from_slice(bytes);
    }

    push_anchor_string(&mut data, name);
    push_anchor_string(&mut data, symbol);
    push_anchor_string(&mut data, uri);

    // creator pubkey (use payer as creator by default)
    data.extend_from_slice(payer.pubkey().as_ref());

    // -----------------------------
    // Build `create` instruction accounts IN THE SAME ORDER AS THE IDL
    // :contentReference[oaicite:14]{index=14}
    // -----------------------------
    let create_ix = Instruction {
        program_id: pump_program,
        accounts: vec![
            AccountMeta::new(mint_pubkey, true),                  // mint (writable, signer)
            AccountMeta::new_readonly(mint_authority_pda, false), // mint_authority (PDA)
            AccountMeta::new(bonding_curve_pda, false),           // bonding_curve (writable)
            AccountMeta::new(associated_bonding_curve_ata, false),// associated_bonding_curve (writable)
            AccountMeta::new_readonly(global_pda, false),         // global
            AccountMeta::new_readonly(token_metadata_program, false), // mpl_token_metadata
            AccountMeta::new(metadata_pda, false),                // metadata (writable)
            AccountMeta::new(payer.pubkey(), true),               // user (writable, signer)
            AccountMeta::new_readonly(system_program::ID, false), // system_program
            AccountMeta::new_readonly(token_program, false),      // token_program
            AccountMeta::new_readonly(ata_program, false),        // associated_token_program
            AccountMeta::new_readonly(rent_sysvar::ID, false),    // rent
            AccountMeta::new_readonly(event_authority_pda, false),// event_authority (PDA)
            AccountMeta::new_readonly(pump_program, false),       // program (address == program_id in IDL)
        ],
        data,
    };

    // -----------------------------
    // Simulate first (prints logs)
    // -----------------------------
    let message = Message::new(&[create_ix], Some(&payer.pubkey()));

    {
        let sim_blockhash = rpc_client.get_latest_blockhash()?;
        let sim_tx = Transaction::new(&[payer, &mint], message.clone(), sim_blockhash);

        let sim = rpc_client.simulate_transaction_with_config(
            &sim_tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                commitment: Some(CommitmentConfig::processed()),
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )?;

        if let Some(err) = sim.value.err {
            eprintln!("[SIMULATION FAILED] {:?}", err);
            if let Some(logs) = sim.value.logs {
                eprintln!("--- Simulation logs ---");
                for l in logs {
                    eprintln!("{}", l);
                }
            }
            return Err(anyhow!("Transaction simulation failed (see logs above)"));
        } else {
            println!("[SIMULATION OK] proceeding to send...");
        }
    }

    // -----------------------------
    // Send
    // -----------------------------
    let send_blockhash = rpc_client.get_latest_blockhash()?;
    let send_tx = Transaction::new(&[payer, &mint], message, send_blockhash);

    let sig = rpc_client.send_and_confirm_transaction(&send_tx)?;
    println!("Pump.fun create sent! Signature: {}", sig);
    println!("Mint: {}", mint_pubkey);

    Ok(mint_pubkey)
}



fn borsh_decode_prefix_dbg<T: BorshDeserialize>(data: &[u8], label: &str) -> anyhow::Result<T> {
    let mut cursor = data;
    let value = T::deserialize(&mut cursor)?;
    println!(
        "{} decoded, bytes consumed = {}, leftover = {}",
        label,
        data.len() - cursor.len(),
        cursor.len()
    );
    Ok(value)
}



// --------------------
// Main function requested
// --------------------
pub fn buy_on_curve(
    rpc_client: &RpcClient,
    payer: &Keypair,
    mint_pubkey: &Pubkey,
    amount_lamports: u64,
) -> Result<()> {
    let pump_program_id = Pubkey::from_str(PUMP_PROGRAM_ID_STR)?;
    let fee_program_id = Pubkey::from_str(PUMP_FEE_PROGRAM_ID_STR)?;

    // ----- PDAs (from pump.json instruction account definitions) -----
    let (global_pda, _) = pda(&pump_program_id, &[b"global"]);
    let (bonding_curve_pda, _) = pda(&pump_program_id, &[b"bonding-curve", mint_pubkey.as_ref()]);
    let (event_authority_pda, _) = pda(&pump_program_id, &[b"__event_authority"]);
    let (global_volume_accumulator_pda, _) = pda(&pump_program_id, &[b"global_volume_accumulator"]);
    let (user_volume_accumulator_pda, _) = pda(&pump_program_id, &[b"user_volume_accumulator", payer.pubkey().as_ref()]);

    // fee_config PDA is under the fee program, seeded by:
    // ["fee_config", fee_program]
    const FEE_CONFIG_SEED_32: [u8; 32] = [
        1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170,
        81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
    ];

    let (fee_config_pda, _) = Pubkey::find_program_address(
        &[b"fee_config", &FEE_CONFIG_SEED_32],
        &fee_program_id,
    );


    // Associated token accounts
    let associated_user_ata = get_associated_token_address_with_program_id(
        &payer.pubkey(),
        mint_pubkey,
        &SPL_TOKEN_ID,
    );

    let associated_bonding_curve_ata = get_associated_token_address_with_program_id(
        &bonding_curve_pda,
        mint_pubkey,
        &SPL_TOKEN_ID,
    );

    // ----- Load Global account to get fee_recipient + fee bps -----
    let global_acc = rpc_client.get_account(&global_pda)?;
    let global_data = strip_anchor_discriminator(&global_acc.data)?;
    let global: Global = borsh_decode_prefix_dbg(global_data, "Global")?;

    // ----- Load BondingCurve account to get reserves + creator -----
    let curve_acc = rpc_client.get_account(&bonding_curve_pda)?;
    let curve_data = strip_anchor_discriminator(&curve_acc.data)?;
    let curve: BondingCurve = borsh_decode_prefix_dbg(curve_data, "BondingCurve")?;

    // creator_vault PDA = ["creator-vault", creator_pubkey] under pump program
    let (creator_vault_pda, _) = pda(&pump_program_id, &[b"creator-vault", curve.creator.as_ref()]);

    // Creator fee bps is only counted if creator is set (common behavior; matches your event)
    let creator_fee_bps = if curve.creator == Pubkey::default() {
        0u64
    } else {
        global.creator_fee_basis_points
    };

    // ----- Quote tokens_out from IDL formula (buy_exact_sol_in) -----
    let protocol_fee_bps = global.fee_basis_points;

    let (tokens_out, _fees) = quote_tokens_out_for_spendable_sol(
        amount_lamports,
        curve.virtual_sol_reserves,
        curve.virtual_token_reserves,
        protocol_fee_bps,
        creator_fee_bps,
    )?;

    // Slippage guard (tweak as needed)
    // 0.5% slippage: min_tokens_out = floor(tokens_out * 9950 / 10000)
    let min_tokens_out = tokens_out.saturating_mul(9_950) / 10_000;

    if min_tokens_out == 0 {
        return Err(anyhow!(
            "min_tokens_out computed as 0 (would risk BuyZeroAmount). Increase amount_lamports."
        ));
    }

    // ----- Build instructions -----
    let mut ixs: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(120_000),
        // Example priority fee; adjust if needed
        ComputeBudgetInstruction::set_compute_unit_price(833_333), // micro-lamports per CU
    ];

    // Create user's ATA if it doesn't exist
    if rpc_client.get_account(&associated_user_ata).is_err() {
        ixs.push(create_associated_token_account(
            &payer.pubkey(),
            &payer.pubkey(),
            mint_pubkey,
            &SPL_TOKEN_ID,
        ));
    }

    // Init user_volume_accumulator if missing
    if rpc_client.get_account(&user_volume_accumulator_pda).is_err() {
        // discriminator for init_user_volume_accumulator from pump.json:
        // [94, 6, 202, 115, 255, 96, 232, 183]
        let data = vec![94, 6, 202, 115, 255, 96, 232, 183];

        let init_ux = Instruction {
            program_id: pump_program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),                  // payer (writable, signer)
                AccountMeta::new_readonly(payer.pubkey(), false),        // user (readonly)
                AccountMeta::new(user_volume_accumulator_pda, false),    // user_volume_accumulator (writable)
                AccountMeta::new_readonly(system_program::id(), false),  // system_program
                AccountMeta::new_readonly(event_authority_pda, false),   // event_authority
                AccountMeta::new_readonly(pump_program_id, false),       // program (pump)
            ],
            data,
        };

        ixs.push(init_ux);
    }

    // buy_exact_sol_in discriminator from pump.json:
    // You can read it from your IDL; I'm using the known discriminator field there.
    // Let's encode as: discriminator + Borsh(args)
    let buy_exact_discriminator: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95]; // <-- replace if your pump.json differs

       
    // If you want to be 100% consistent with YOUR pump.json, replace the above
    // with the `buy_exact_sol_in.discriminator` bytes from that file.
    // (Your earlier screenshots showed `buy`, not `buy_exact_sol_in`, so verify this.)

    let args = BuyExactSolInArgs {
        spendable_sol_in: amount_lamports,
        min_tokens_out,
        track_volume: OptionBool(true),
    };

    let mut buy_data = Vec::with_capacity(8 + 8 + 8 + 1);
    buy_data.extend_from_slice(&buy_exact_discriminator);
    buy_data.extend_from_slice(&args.try_to_vec()?);

    let buy_ix = Instruction {
        program_id: pump_program_id,
        accounts: vec![
            AccountMeta::new_readonly(global_pda, false),                    // global
            AccountMeta::new(global.fee_recipient, false),                   // fee_recipient (writable in IDL, but safe as writable)
            AccountMeta::new_readonly(*mint_pubkey, false),                  // mint
            AccountMeta::new(bonding_curve_pda, false),                      // bonding_curve (writable)
            AccountMeta::new(associated_bonding_curve_ata, false),           // associated_bonding_curve (writable)
            AccountMeta::new(associated_user_ata, false),                    // associated_user (writable)
            AccountMeta::new(payer.pubkey(), true),                          // user (writable, signer)
            AccountMeta::new_readonly(system_program::id(), false),          // system_program
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),              // token_program
            AccountMeta::new(creator_vault_pda, false),                      // creator_vault (writable)
            AccountMeta::new_readonly(event_authority_pda, false),           // event_authority
            AccountMeta::new_readonly(pump_program_id, false),               // program
            AccountMeta::new_readonly(global_volume_accumulator_pda, false), // global_volume_accumulator
            AccountMeta::new(user_volume_accumulator_pda, false),            // user_volume_accumulator (writable)
            AccountMeta::new_readonly(fee_config_pda, false),                // fee_config
            AccountMeta::new_readonly(fee_program_id, false),                // fee_program
        ],
        data: buy_data,
    };

    // Fix writable flags to match IDL exactly where required:
    // fee_recipient, bonding_curve, associated_bonding_curve, associated_user, user, creator_vault, user_volume_accumulator must be writable.
    // (Solana will reject if something that must be writable isn't.)
    let mut buy_ix_fixed = buy_ix;
    buy_ix_fixed.accounts[1].is_writable = true;  // fee_recipient
    buy_ix_fixed.accounts[3].is_writable = true;  // bonding_curve
    buy_ix_fixed.accounts[4].is_writable = true;  // associated_bonding_curve
    buy_ix_fixed.accounts[5].is_writable = true;  // associated_user
    buy_ix_fixed.accounts[6].is_writable = true;  // user
    buy_ix_fixed.accounts[9].is_writable = true;  // creator_vault
    buy_ix_fixed.accounts[13].is_writable = true; // user_volume_accumulator

    ixs.push(buy_ix_fixed);

    // ----- Send transaction -----
    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    // 1) Print key accounts you derived (so we can sanity-check PDAs)
    print_key_accounts(
        &global_pda,
        &bonding_curve_pda,
        &associated_user_ata,
        &associated_bonding_curve_ata,
        &creator_vault_pda,
        &fee_config_pda,
        &fee_program_id,
        &user_volume_accumulator_pda,
        &global_volume_accumulator_pda,
    );

    // 2) Print instruction index -> program mapping (so we know what "Instruction 4" is)
    print_ixs("TX", &ixs);

    // 3) Simulate and print logs BEFORE sending (this is the main thing we need)
    simulate_and_print(rpc_client, &tx)?;



    rpc_client.send_and_confirm_transaction(&tx)?;
    Ok(())
}






// Conservative sell quote:
// gross_sol_out ≈ token_in * V_sol / (V_token + token_in)
// then apply fee haircut and slippage haircut.
fn quote_min_sol_out_conservative(
    token_in: u64,
    v_sol: u64,
    v_token: u64,
    protocol_fee_bps: u64,
    creator_fee_bps: u64,
    slippage_bps: u64, // e.g. 50 = 0.50%
) -> Result<u64> {
    if token_in == 0 {
        return Err(anyhow!("token_in is 0"));
    }

    let denom = (v_token as u128).saturating_add(token_in as u128);
    if denom == 0 {
        return Err(anyhow!("invalid virtual reserves"));
    }

    let gross_sol_out_u128 =
        (token_in as u128).saturating_mul(v_sol as u128) / denom;

    let mut gross_sol_out = gross_sol_out_u128 as u64;

    // Fee haircut (protocol + creator)
    let total_fee_bps = protocol_fee_bps
        .checked_add(creator_fee_bps)
        .ok_or_else(|| anyhow!("fee bps overflow"))?;

    // net ≈ gross * (10_000 - total_fee_bps) / 10_000
    if total_fee_bps >= 10_000 {
        return Err(anyhow!("total_fee_bps >= 100%"));
    }
    gross_sol_out = gross_sol_out
        .saturating_mul(10_000u64.saturating_sub(total_fee_bps))
        / 10_000;

    // Slippage haircut
    if slippage_bps >= 10_000 {
        return Err(anyhow!("slippage_bps >= 100%"));
    }
    let min_sol_out = gross_sol_out
        .saturating_mul(10_000u64.saturating_sub(slippage_bps))
        / 10_000;

    Ok(min_sol_out)
}

pub fn sell_on_curve(
    rpc_client: &RpcClient,
    payer: &Keypair,
    mint_pubkey: &Pubkey,
    token_amount: u64,
) -> Result<()> {
    let pump_program_id = Pubkey::from_str(PUMP_PROGRAM_ID_STR)?;
    let fee_program_id = Pubkey::from_str(PUMP_FEE_PROGRAM_ID_STR)?;

    println!("[sell] mint={}", mint_pubkey);
    println!("[sell] token_amount_raw={}", token_amount);
    println!("[sell] fetching global + bonding curve accounts...");

    if token_amount < 10 {
        println!("[sell] token_amount too small, skipping");
        return Ok(());
    }
    // PDAs from pump.json
    let (global_pda, _) = pda(&pump_program_id, &[b"global"]);
    let (bonding_curve_pda, _) = pda(&pump_program_id, &[b"bonding-curve", mint_pubkey.as_ref()]);
    let (event_authority_pda, _) = pda(&pump_program_id, &[b"__event_authority"]);

    // fee_config PDA (IDL shows 2 const seeds: "fee_config" + pump_program_id bytes)
    let (fee_config_pda, _) = Pubkey::find_program_address(
        &[b"fee_config", pump_program_id.as_ref()],
        &fee_program_id,
    );

    // ATAs
    let associated_user_ata = get_associated_token_address_with_program_id(
        &payer.pubkey(),
        mint_pubkey,
        &SPL_TOKEN_ID,
    );

    let associated_bonding_curve_ata = get_associated_token_address_with_program_id(
        &bonding_curve_pda,
        mint_pubkey,
        &SPL_TOKEN_ID,
    );

    // Load Global
    let global_acc = rpc_client.get_account(&global_pda)?;
    let global_data = strip_anchor_discriminator(&global_acc.data)?;
    let global: Global = borsh_decode_prefix_dbg(global_data, "Global")?;
    println!("[sell] decoded Global ok");


    // Load BondingCurve
    let curve_acc = rpc_client.get_account(&bonding_curve_pda)?;
    let curve_data = strip_anchor_discriminator(&curve_acc.data)?;
    let curve: BondingCurve = borsh_decode_prefix_dbg(curve_data, "BondingCurve")?;
    println!("[sell] decoded BondingCurve ok");

    // creator_vault PDA = ["creator-vault", creator] under pump program
    let (creator_vault_pda, _) = pda(&pump_program_id, &[b"creator-vault", curve.creator.as_ref()]);

    // Estimate min SOL out (conservative)
    let protocol_fee_bps = global.fee_basis_points;
    let creator_fee_bps = if curve.creator == Pubkey::default() {
        0
    } else {
        global.creator_fee_basis_points
    };

    // 0.50% extra slippage safety
    let min_sol_output = quote_min_sol_out_conservative(
        token_amount,
        curve.virtual_sol_reserves,
        curve.virtual_token_reserves,
        protocol_fee_bps,
        creator_fee_bps,
        50, // 0.50%
    )?;

    // Discriminator for `sell` from pump.json:
    let sell_discriminator: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

    // Args: amount(u64) + min_sol_output(u64)
    let mut ix_data = Vec::with_capacity(8 + 8 + 8);
    ix_data.extend_from_slice(&sell_discriminator);
    ix_data.extend_from_slice(&token_amount.to_le_bytes());
    ix_data.extend_from_slice(&min_sol_output.to_le_bytes());

    // Build sell instruction with exact account order from pump.json
    let sell_ix = Instruction {
        program_id: pump_program_id,
        accounts: vec![
            AccountMeta::new_readonly(global_pda, false),           // global
            AccountMeta::new(global.fee_recipient, false),   //        // fee_recipient (writable)
            AccountMeta::new_readonly(*mint_pubkey, false),         // mint
            AccountMeta::new(bonding_curve_pda, false),  //            // bonding_curve (writable)
            AccountMeta::new(associated_bonding_curve_ata, false), //  // associated_bonding_curve (writable)
            AccountMeta::new(associated_user_ata, false),  //          // associated_user (writable)
            AccountMeta::new(payer.pubkey(), true),                 // user (writable, signer)
            AccountMeta::new_readonly(system_program::id(), false), // system_program
            AccountMeta::new(creator_vault_pda, false),   //           // creator_vault (writable)
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),     // token_program
            AccountMeta::new_readonly(event_authority_pda, false),  // event_authority
            AccountMeta::new_readonly(pump_program_id, false),      // program
            AccountMeta::new_readonly(fee_config_pda, false),       // fee_config
            AccountMeta::new_readonly(fee_program_id, false),       // fee_program
        ],
        data: ix_data,
    };

     println!("--- [sell] instruction accounts (signers) ---");
    for (i, a) in sell_ix.accounts.iter().enumerate() {
        if a.is_signer {
            println!("  signer acct[{}] = {}", i, a.pubkey);
        }
    }
    println!("--------------------------------------------");

    // (Optional) compute budget — keep it similar to buy
    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(120_000),
        // If you set price too high, it will burn SOL fast. Start small or omit.
        // ComputeBudgetInstruction::set_compute_unit_price(200_000),
        sell_ix,
    ];

   

    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );


    let sig = rpc_client.send_and_confirm_transaction(&tx)?;
    println!("[sell] signature = {}", sig);
    Ok(())
}




pub async fn buy_on_curve_async(
    rpc_url: String,
    payer_bytes: Vec<u8>,
    mint: Pubkey,
    amount_lamports: u64,
) -> Result<()> {
    task::spawn_blocking(move || -> Result<()> {
        let client = RpcClient::new(rpc_url);
        let payer = Keypair::from_bytes(&payer_bytes)
            .map_err(|e| anyhow!("Keypair::from_bytes failed: {e}"))?;
        buy_on_curve(&client, &payer, &mint, amount_lamports)
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
}

pub async fn sell_on_curve_async(
    rpc_url: String,
    payer_bytes: Vec<u8>,
    mint: Pubkey,
    token_amount: u64,
) -> Result<()> {
    task::spawn_blocking(move || -> Result<()> {
        let client = RpcClient::new(rpc_url);
        let payer = Keypair::from_bytes(&payer_bytes)
            .map_err(|e| anyhow!("Keypair::from_bytes failed: {e}"))?;
        sell_on_curve(&client, &payer, &mint, token_amount)
    })
    .await
    .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
}


pub async fn buy_on_curve_async_from_client(
    rpc_client: &RpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    amount_lamports: u64,
) -> Result<()> {
    buy_on_curve_async(
        rpc_client.url().to_string(),
        payer.to_bytes().to_vec(),
        *mint,
        amount_lamports,
    ).await
}

pub async fn sell_on_curve_async_from_client(
    rpc_client: &RpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    token_amount: u64,
) -> Result<()> {
    sell_on_curve_async(
        rpc_client.url().to_string(),
        payer.to_bytes().to_vec(),
        *mint,
        token_amount,
    ).await
}
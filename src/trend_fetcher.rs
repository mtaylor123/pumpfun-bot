use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use mpl_token_metadata::accounts::Metadata;

use base64::{engine::general_purpose, Engine as _};
use serde_json::json;
use std::env;


// Metaplex Token Metadata program
const MPL_TOKEN_METADATA: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

#[derive(Debug, Clone)]
pub struct TrendToken {
    pub chain_id: String,
    pub token_address: String,
    pub name: String,
    pub symbol: String,
    pub image_url: Option<String>, // "photo"
    pub pair_url: Option<String>,
    pub description: Option<String>,
    pub pair_created_at_ms: Option<u64>,
    pub boosts_total_amount: Option<u64>, // best-effort
}

#[derive(Debug, Deserialize, Clone)]
struct BoostEntry {
    #[serde(rename = "chainId")]
    chain_id: String,
    #[serde(rename = "tokenAddress")]
    token_address: String,

    #[serde(rename = "totalAmount")]
    total_amount: Option<u64>,
    icon: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct PairTokenInfo {
    address: String,
    name: String,
    symbol: String,
}

#[derive(Debug, Deserialize, Clone)]
struct PairInfo {
    #[serde(rename = "imageUrl")]
    image_url: Option<String>,

    // Best-effort: may or may not exist
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct PairResp {
    #[serde(rename = "chainId")]
    chain_id: String,
    url: Option<String>,

    #[serde(rename = "pairCreatedAt")]
    pair_created_at: Option<u64>, // ms epoch in practice

    #[serde(rename = "baseToken")]
    base_token: PairTokenInfo,

    info: Option<PairInfo>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis() as u64
}

/// Dexscreener sometimes returns either a single object or an array for these endpoints.
fn parse_boosts_lenient(body: &str) -> Result<Vec<BoostEntry>> {
    if let Ok(v) = serde_json::from_str::<Vec<BoostEntry>>(body) {
        return Ok(v);
    }
    if let Ok(one) = serde_json::from_str::<BoostEntry>(body) {
        return Ok(vec![one]);
    }
    Err(anyhow!("Unexpected boosts response format"))
}

/// Fetch top 10 recent trending tokens (ASYNC).
/// - chain_id: e.g. "solana"
/// - max_age_secs: only keep pairs created within this window (e.g. 6 hours)
///
/// “trending” source here is boosted lists (top + latest).
pub async fn fetch_recent_trends(chain_id: &str, max_age_secs: u64) -> Result<Vec<TrendToken>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()?;

    // 1) Pull boosted lists
    let top_url = "https://api.dexscreener.com/token-boosts/top/v1";
    let latest_url = "https://api.dexscreener.com/token-boosts/latest/v1";

    let top_body = client
        .get(top_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let latest_body = client
        .get(latest_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let mut boosts = Vec::new();
    boosts.extend(parse_boosts_lenient(&top_body)?);
    boosts.extend(parse_boosts_lenient(&latest_body)?);

    // 2) Filter by chain and dedupe by token address, keeping max totalAmount
    let mut best_by_token: HashMap<String, BoostEntry> = HashMap::new();

    for b in boosts.into_iter().filter(|b| b.chain_id == chain_id) {
        let key = b.token_address.clone();
        best_by_token
            .entry(key)
            .and_modify(|cur| {
                let cur_t = cur.total_amount.unwrap_or(0);
                let new_t = b.total_amount.unwrap_or(0);
                if new_t > cur_t {
                    *cur = b.clone();
                }
            })
            .or_insert(b);
    }

    // Prefer higher totalAmount first
    let mut boost_list: Vec<BoostEntry> = best_by_token.into_values().collect();
    boost_list.sort_by_key(|b| std::cmp::Reverse(b.total_amount.unwrap_or(0)));

    // Cap enrichment to reduce API calls
    let enrich_cap = 60usize;
    boost_list.truncate(enrich_cap);

    // 3) Enrich via /tokens/v1/{chainId}/{tokenAddresses} (up to 30 per call)
    let mut tokens: Vec<TrendToken> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let now = now_ms();
    let max_age_ms = max_age_secs.saturating_mul(1000);

    for chunk in boost_list.chunks(30) {
        let addrs: Vec<String> = chunk.iter().map(|b| b.token_address.clone()).collect();

        let url = format!(
            "https://api.dexscreener.com/tokens/v1/{}/{}",
            chain_id,
            addrs.join(",")
        );

        let body = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        let pairs: Vec<PairResp> = serde_json::from_str(&body)
            .map_err(|e| anyhow!("Failed to parse tokens/v1 response: {e}"))?;

        // Pick best pair per baseToken.address (most recent pairCreatedAt)
        let mut pairs_by_base: HashMap<String, PairResp> = HashMap::new();
        for p in pairs {
            let key = p.base_token.address.clone();
            let replace = match pairs_by_base.get(&key) {
                None => true,
                Some(existing) => p.pair_created_at.unwrap_or(0) > existing.pair_created_at.unwrap_or(0),
            };
            if replace {
                pairs_by_base.insert(key, p);
            }
        }

        for b in chunk {
            if seen.contains(&b.token_address) {
                continue;
            }

            // Need a pair where token is baseToken
            let p = match pairs_by_base.get(&b.token_address) {
                Some(p) => p,
                None => continue,
            };

            // Recent filter
            if let Some(created_ms) = p.pair_created_at {
                if now.saturating_sub(created_ms) > max_age_ms {
                    continue;
                }
            } else {
                continue;
            }

            let image_url = p
                .info
                .as_ref()
                .and_then(|i| i.image_url.clone())
                .or_else(|| b.icon.clone());

            let image_url = image_url.filter(|u| u.starts_with("http"));
            if image_url.is_none() {
                continue; // skip tokens without a usable image URL
            }
            let description = p
            .info
            .as_ref()
            .and_then(|i| i.description.clone());

            let name_fixed = cap_name(&p.base_token.name);
            let symbol_fixed = cap_symbol(&p.base_token.symbol);

            tokens.push(TrendToken {
                chain_id: chain_id.to_string(),
                token_address: b.token_address.clone(),
                name: name_fixed,
                symbol: symbol_fixed,
                image_url,
                description, 
                pair_url: p.url.clone().or_else(|| b.url.clone()),
                pair_created_at_ms: p.pair_created_at,
                boosts_total_amount: b.total_amount,
            });

            seen.insert(b.token_address.clone());

            if tokens.len() >= 10 {
                break;
            }
        }

        if tokens.len() >= 10 {
            break;
        }
    }

    // Prefer higher boosts_total_amount
    tokens.sort_by_key(|t| std::cmp::Reverse(t.boosts_total_amount.unwrap_or(0)));
    tokens.truncate(10);

    Ok(tokens)
}


pub fn fetch_metaplex_uri_for_mint(rpc_client: &RpcClient, mint: &Pubkey) -> Result<String> {
    let mpl = Pubkey::from_str(MPL_TOKEN_METADATA)?;
    let (metadata_pda, _) = Pubkey::find_program_address(
        &[b"metadata", mpl.as_ref(), mint.as_ref()],
        &mpl,
    );

    let acct = match rpc_client.get_account(&metadata_pda) {
        Ok(a) => a,
        Err(e) => {
            // If the metadata account doesn't exist yet, don't fail the program.
            // Return empty string to signal "no URI available".
            if e.to_string().contains("AccountNotFound") {
                return Ok(String::new());
            }
            return Err(anyhow!("failed to fetch metadata account {}: {}", metadata_pda, e));
        }
    };

    let metadata = match Metadata::from_bytes(&acct.data) {
        Ok(m) => m,
        Err(e) => {
            // If we can't decode, treat it as "no URI" instead of killing execution.
            eprintln!(
                "[WARN] failed to decode metaplex metadata for mint {} (pda {}): {}",
                mint, metadata_pda, e
            );
            return Ok(String::new());
        }
    };

    let uri = metadata.uri.trim_matches(char::from(0)).trim().to_string();
    if uri.is_empty() {
        Ok(String::new())
    } else {
        Ok(uri)
    }
}

/// Upload metadata JSON to GitHub repo and return GitHub Pages URI.
/// - file_key becomes metadata/{file_key}.json
/// - returns: https://mtaylor123.github.io/konovu-metadata/metadata/{file_key}.json


pub async fn upload_metadata_and_get_uri(
    github: &crate::config::GithubConfig, // adjust path to where your Config types live
    file_key: &str,
    name: &str,
    symbol: &str,
    image_url: Option<&str>,
    description: Option<&str>,
) -> Result<String> {
    let owner = github.owner.trim();
    let repo = github.repo.trim();
    let token = github.token.trim();

    if token.is_empty() {
        return Err(anyhow!("github.token is empty in config"));
    }

    // Basic sanitization
    let name = name.trim();
    let symbol = symbol.trim();

    let name_trunc: String = name.chars().take(32).collect();
    let mut symbol_trunc: String = symbol.chars().take(10).collect();
    symbol_trunc.make_ascii_uppercase();

    let image = image_url.unwrap_or("").trim();
    let image_field = if image.starts_with("http") { image } else { "" };

    let desc = description
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "".to_string());

    let meta = json!({
        "name": if name_trunc.is_empty() { "UNTITLED".to_string() } else { name_trunc },
        "symbol": if symbol_trunc.is_empty() { "TKN".to_string() } else { symbol_trunc },
        "description": desc,
        "image": image_field,
        "attributes": []
    });

    let content_str = meta.to_string();
    let content_b64 = general_purpose::STANDARD.encode(content_str.as_bytes());

    let path = format!("metadata/{}.json", file_key);
    let url = format!("https://api.github.com/repos/{owner}/{repo}/contents/{path}");

    let client = Client::builder()
        .user_agent("konovu-metadata-uploader/1.0")
        .build()?;

    // If file exists, GitHub requires sha to update
    let existing_sha: Option<String> = {
        let resp = client.get(&url).bearer_auth(token).send().await?;
        if resp.status().is_success() {
            let v: serde_json::Value = resp.json().await?;
            v.get("sha").and_then(|x| x.as_str()).map(|s| s.to_string())
        } else {
            None
        }
    };

    let mut body = json!({
        "message": format!("add/update metadata {}", file_key),
        "content": content_b64
    });

    if let Some(sha) = existing_sha {
        body["sha"] = json!(sha);
    }

    let put_resp = client
        .put(&url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;

    if !put_resp.status().is_success() {
        let status = put_resp.status();
        let txt = put_resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub upload failed {}: {}", status, txt));
    }

    // GitHub Pages URL
    Ok(format!(
        "https://{}.github.io/{}/metadata/{}.json",
        owner, repo, file_key
    ))
}

pub fn sanitize_symbol_for_filename(symbol: &str) -> String {
    let mut out = String::new();
    for c in symbol.trim().chars() {
        let up = c.to_ascii_uppercase();
        if up.is_ascii_alphanumeric() {
            out.push(up);
        } else if up == '_' || up == '-' {
            out.push('_');
        }
        // no cap
    }
    if out.is_empty() { "TKN".to_string() } else { out }
}


fn cap_name(name: &str) -> String {
    // Metaplex name max is 32
    name.trim().chars().take(32).collect()
}

fn cap_symbol(symbol: &str) -> String {
    // Metaplex symbol max is 10
    let mut s: String = symbol.trim().chars().take(10).collect();
    s.make_ascii_uppercase();
    s
}
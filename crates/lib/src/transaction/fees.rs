use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL, pubkey::Pubkey, rent::Rent, transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use std::time::Duration;
use utoipa::ToSchema;

use crate::{
    error::KoraError,
    oracle::{get_price_oracle, PriceSource, RetryingPriceOracle},
    token::{TokenInterface, TokenProgram, TokenType},
};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TokenPriceInfo {
    pub price: f64,
}

pub async fn estimate_transaction_fee(
    rpc_client: &RpcClient,
    transaction: &Transaction,
) -> Result<u64, KoraError> {
    // Get base transaction fee
    let base_fee = rpc_client
        .get_fee_for_message(&transaction.message)
        .await
        .map_err(|e| KoraError::RpcError(e.to_string()))?;

    // Get account creation fees (for ATA creation)
    let account_creation_fee = get_associated_token_account_creation_fees(rpc_client, transaction)
        .await
        .map_err(|e| KoraError::RpcError(e.to_string()))?;

    // Get priority fee from recent blocks
    let priority_stats = rpc_client
        .get_recent_prioritization_fees(&[])
        .await
        .map_err(|e| KoraError::RpcError(e.to_string()))?;
    let priority_fee = priority_stats.iter().map(|fee| fee.prioritization_fee).max().unwrap_or(0);

    Ok(base_fee + priority_fee + account_creation_fee)
}

async fn get_associated_token_account_creation_fees(
    rpc_client: &RpcClient,
    transaction: &Transaction,
) -> Result<u64, KoraError> {
    const ATA_ACCOUNT_SIZE: usize = 165; // Standard ATA size
    let mut ata_count = 0u64;

    // Check each instruction in the transaction for ATA creation
    for instruction in &transaction.message.instructions {
        let program_id = transaction.message.account_keys[instruction.program_id_index as usize];

        // Skip if not an ATA program instruction
        if program_id != spl_associated_token_account::id() {
            continue;
        }

        let ata = transaction.message.account_keys[instruction.accounts[1] as usize];
        let owner = transaction.message.account_keys[instruction.accounts[2] as usize];
        let mint = transaction.message.account_keys[instruction.accounts[3] as usize];

        let expected_ata = get_associated_token_address(&owner, &mint);

        if ata == expected_ata && rpc_client.get_account(&ata).await.is_err() {
            ata_count += 1;
        }
    }

    // Get rent cost in lamports for ATA creation
    let rent = Rent::default();
    let exempt_min = rent.minimum_balance(ATA_ACCOUNT_SIZE);

    Ok(exempt_min * ata_count)
}

pub async fn calculate_token_value_in_lamports(
    amount: u64,
    mint: &Pubkey,
    price_source: PriceSource,
    rpc_client: &RpcClient,
) -> Result<u64, KoraError> {
    // Fetch mint account data to determine token decimals
    let mint_account =
        rpc_client.get_account(mint).await.map_err(|e| KoraError::RpcError(e.to_string()))?;

    let token_program = TokenProgram::new(TokenType::Spl);
    let decimals = token_program.get_mint_decimals(&mint_account.data)?;

    // Initialize price oracle with retries for reliability
    let oracle =
        RetryingPriceOracle::new(3, Duration::from_secs(1), get_price_oracle(price_source));

    // Get token price in SOL directly
    let token_price = oracle
        .get_token_price(&mint.to_string())
        .await
        .map_err(|e| KoraError::RpcError(format!("Failed to fetch token price: {}", e)))?;

    // Convert token amount to its real value based on decimals and multiply by SOL price
    let token_amount = amount as f64 / 10f64.powi(decimals as i32);
    let sol_amount = token_amount * token_price.price;

    // Convert SOL to lamports and round down
    let lamports = (sol_amount * LAMPORTS_PER_SOL as f64).floor() as u64;

    Ok(lamports)
}

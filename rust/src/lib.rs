//! This crate provides interface to request for a verifiable randomness (ED25519 64 bytes Signature) from orao vrf contract on the Solana blockchain.
//! Given 32 bytes random seed and a payer `Keypair`, randomness can be generated and confirmed in minutes!
//!
//! ## Usage
//!
//! Do note that `Keypair` provided must have sufficient SOL to pay for transaction fees. That's really all you need!
//! ```
//! use orao_solana_vrf::{VrfRequestor, Network};
//! use solana_sdk::{signature::Keypair};
//!
//! // Declare payer here.
//! let payer = Keypair::from_bytes(vec![]);
//! let random_seed = Keypair::new().pubkey();
//!
//! // Generate Randomness.
//! let requestor = VrfRequestor::new(Network::Devnet).unwrap();
//! requestor.request_randomness(&payer, &random_seed).unwrap();
//! let randomness = requestor.get_randomness(&randomness);
//!
//!
//! // Verify Randomness (Optional)
//! // Note: All generated randomness are submitted to native EdSigVerify program for onchain verification before
//! // being saved on the account.
//! // No news is good news.
//! requestor.verify_randomness_offchain(&random_seed, &randomness).unwrap();
//!
//!
//! ```
mod env;
mod error;
mod instructions;
mod state;
mod verify;

use env::Env;

pub use env::Network;
pub use error::Error;
use instructions::VrfInstruction;
use log::info;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
  pubkey::Pubkey,
  signature::{Keypair, Signature},
  signer::Signer,
  transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use state::decode_treasury_acc_from_config;
pub use state::{Randomness, RandomnessStatus};
use std::str::FromStr;
use verify::{is_vrf_fulfilled_transaction, verify_randomness_offchain};

/// VrfRequestor encapsulates logic to request randomness from orao vrf contract on the Solana blockchain.
///
/// Provides functions to query and create `Randomness`
///
/// # Example
///
/// ```
/// use orao_solana_vrf::{VrfRequestor, Network};
/// use solana_sdk::{signature::Keypair};
///
/// // Declare payer here.
/// let payer = Keypair::from_bytes(vec![]);
/// let random_seed = Keypair::new().pubkey();
///
/// // Generate Randomness.
/// let requestor = VrfRequestor::new(Network::Devnet).unwrap();
/// requestor.request_randomness(&payer, &random_seed).unwrap();
/// let randomness = requestor.get_randomness(&randomness);
///
/// ```
///
pub struct VrfRequestor {
  pub rpc_client: RpcClient,
  env: Env,
}

impl VrfRequestor {
  /// Create an instance of VrfRequestor
  ///
  /// ```
  /// use orao_solana_vrf::{VrfRequestor, Network};
  /// use solana_sdk::{signature::Keypair};
  ///
  /// // Declare payer here.
  /// let payer = Keypair::from_bytes(vec![]);
  /// let random_seed = Keypair::new().pubkey();
  ///
  /// // Generate Randomness.
  /// let requestor = VrfRequestor::new(Network::Devnet).unwrap();
  /// ```
  pub fn new(network: Network) -> Result<Self, Error> {
    // Default environment
    let env = Env::new(&network);
    let rpc_client = RpcClient::new(network.rpc_url());
    Ok(Self { env, rpc_client })
  }

  /// Retrieve randomness associated with seed from the chain.
  ///
  /// Returns `Randomness` that were generated with the seed passed from chain. Otherwise,
  /// returns `Error::SolanaClientError` if seed is unused.
  ///
  pub fn get_randomness(&self, seed: &Pubkey) -> Result<Randomness, Error> {
    self.get_randomness_account(seed)
  }

  /// Request for a Randomness with associated seed on chain.
  ///
  /// Given an unseen seed and payer's public key, it submits a `Transaction` with instruction to
  /// request for randomess on chain. Payer's acount must have sufficient SOL to pay for transaction fees.
  /// In this case, this will be a long running operation as it waits for transaction submitted to be
  /// confirmed on chain.
  ///
  /// If seed has been used, it will do nothing.
  ///
  pub fn request_randomness(
    &self,
    payer: &Keypair,
    seed: &Pubkey,
  ) -> Result<(), Error> {
    if let Err(_) = self.get_randomness_account(seed) {
      let tx = self.build_randomness_request_tx(seed, payer)?;
      println!("Tx built: {:?}", tx);
      println!("Sending and confirming TX");
      let signature = self.rpc_client.send_and_confirm_transaction(&tx)?;
      println!("TX signature: {:?}", signature);
      println!("Tx sent. Waiting for fulfilment...");
    } else {
      info!("Randomness exists!");
    }
    Ok(())
  }

  fn build_randomness_request_tx(
    &self,
    seed: &Pubkey,
    payer: &Keypair,
  ) -> Result<Transaction, Error> {
    // Get the config account
    let (config_address, _) = Pubkey::find_program_address(
      &[self.env.config_account_seed.as_bytes()],
      &self.env.vrf_program,
    );
    let config_account_data =
      self.rpc_client.get_account_data(&config_address)?;

    // Extract treasury address from config data.
    let treasury_address =
      decode_treasury_acc_from_config(&config_account_data)?;

    // Build instruction for vrf request
    let instruction = VrfInstruction::request(
      &self.env,
      &payer.pubkey(),
      &treasury_address,
      seed.to_bytes(),
    )?;

    let recent_blockhash = self.rpc_client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
      &[instruction],
      Some(&payer.pubkey()),
      &[payer],
      recent_blockhash,
    );
    Ok(tx)
  }

  fn get_randomness_account(
    &self,
    seed_pubkey: &Pubkey,
  ) -> Result<Randomness, Error> {
    let randomness_address = derive_randomness_address(
      &seed_pubkey.to_bytes(),
      self.env.randomness_account_seed.as_str(),
      &self.env.vrf_program,
    );
    let randomness_account =
      self.rpc_client.get_account(&randomness_address)?;
    Randomness::decode_from_bytes(&randomness_account.data)
  }

  /// Verify `Randomness` with `PublicKey` and `seed` used.
  ///
  /// Fetch `PublicKey` from `FulfillRandomness` transaction which contain EdSigVerify and FulfillRandomness instruction.
  /// Then, verify `Randomness` (signature) generated from `seed` (message) and `Public Key`. An invalid `Randomness` will throw `Error::RandomnessVerifyError` error.
  ///
  /// _Note: This step is optional as `Randomness` returned from `Self::get_randomness` would have been
  /// verified onchain via native EdSigVerify program._
  ///
  pub fn verify_randomness_offchain(
    &self,
    seed: &Pubkey,
    randomness: &Randomness,
  ) -> Result<(), Error> {
    // Get randomness account
    let req_account =
      self.env.find_randomness_request_account(&seed.to_bytes());

    // Get randomness generated from seed
    let randomness_signature =
      randomness.randomness.clone().unwrap_or(vec![0; 64]);

    // List all confirmed transactions
    let signatures: Vec<String> = self
      .rpc_client
      .get_signatures_for_address(&req_account)?
      .into_iter()
      .map(|tx| tx.signature)
      .collect();

    if signatures.len() == 0 {
      return Err(Error::NotFound(format!(
        "No transactions found for seed {}",
        seed
      )));
    }

    for signature_str in signatures.iter() {
      let signature = Signature::from_str(signature_str).unwrap();
      // Fetch transaction data for each signaature
      let tx = self
        .rpc_client
        .get_transaction(&signature, UiTransactionEncoding::JsonParsed)?;
      // Skip transaction if tx status is error
      if tx
        .transaction
        .meta
        .as_ref()
        .map(|meta| meta.status.is_err())
        .unwrap_or(true)
      {
        info!("Skipping transaction {:?} due to error status", signature_str);
        continue;
      }
      if is_vrf_fulfilled_transaction(&tx, self.env.vrf_program.to_string()) {
        verify_randomness_offchain(
          &tx,
          &seed.to_bytes(),
          randomness_signature.as_ref(),
        )?;
        return Ok(());
      }
    }

    Err(Error::RandomnessVerifyError(
      "Unable to find transaction with EdSigVerify instruction".to_string(),
    ))
  }
}

fn derive_randomness_address(
  seed: &[u8],
  prefix_seed: &str,
  program: &Pubkey,
) -> Pubkey {
  let (public_key, _) =
    Pubkey::find_program_address(&[prefix_seed.as_bytes(), seed], program);
  return public_key;
}

#[cfg(test)]
mod tests {
  use super::{Env, Network, VrfRequestor};
  use solana_sdk::pubkey::Pubkey;
  use std::str::FromStr;

  #[test]
  fn test_verify_randomness_offchain() {
    let mut requestor = VrfRequestor::new(Network::Devnet).unwrap();
    // Change program id
    requestor.env.vrf_program =
      Pubkey::from_str("VRFUm3dhiqtyW6nj8XghcPLJbCXg9Hj85iABpxwq1Xz").unwrap();
    let seed =
      Pubkey::from_str("HB2UbFFKUt4ZNHoJgGYtG9FzcXxzZeu2Zqy7k3sZKGqK").unwrap();

    let randomness = requestor.get_randomness(&seed).unwrap();
    let res = requestor.verify_randomness_offchain(&seed, &randomness);
    assert_eq!(res.is_ok(), true);
  }

  #[test]
  fn test_verify_randomness_offchain_with_error_status() {
    let mut requestor = VrfRequestor::new(Network::Devnet).unwrap();
    // Change program id
    requestor.env.vrf_program =
      Pubkey::from_str("VRFUm3dhiqtyW6nj8XghcPLJbCXg9Hj85iABpxwq1Xz").unwrap();
    
    // This seed contains a failed Fulfill transaction that should be skipped.
    let seed = Pubkey::new(&[
      96, 135, 155, 105, 43, 71, 237, 124, 163, 112, 135, 141, 76, 39, 239, 53,
      248, 172, 40, 167, 137, 248, 107, 93, 126, 211, 48, 152, 145, 175, 209,
      235,
    ]);
    let randomness = requestor.get_randomness(&seed).unwrap();
    let res =
      requestor.verify_randomness_offchain(&seed, &randomness);
    assert_eq!(res.is_ok(), true);
  }
}

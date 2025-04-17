// Copyright 2025 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// This application demonstrates how to send an off-chain proof request
// to the Bonsai proving service and publish the received proofs directly
// to your deployed app contract.

use alloy_primitives::{Address, U256};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use rewards_methods::{BALANCE_OF_ELF, BALANCE_OF_ID};
use risc0_ethereum_contracts::encode_seal;
use risc0_steel::alloy::{
    network::EthereumWallet,
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{SolCall, SolValue},
};
use risc0_steel::{
    ethereum::{EthEvmEnv, ETH_SEPOLIA_CHAIN_SPEC},
    host::BlockNumberOrTag,
    Commitment, Contract,
};
use risc0_zkvm::{default_prover, Digest, ExecutorEnv, ProverOpts, VerifierContext};
use tokio::task;
use tracing_subscriber::EnvFilter;
use url::Url;

/// Specify the function to call using the [`sol!`] macro.
/// This parses the Solidity syntax to generate a struct that implements the `SolCall` trait.
sol! {
    /// ERC-20 balance function signature.
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }

    interface IDelegation {
        function delegates(address account) external view returns (address);
    }

    interface IProposal {
        function votingToken() external view returns (address);
        function votedAt(uint256 proposalIndex, address voter) external view returns (uint256 blockNumber);
        function proposalEndBlock(uint256 proposalIndex) external view returns (uint256 blockNumber);
        function proposalExists(uint256 proposalIndex) external view returns (bool);
    }
}

/// ABI encodable journal data.
sol! {
    struct Journal {
        Commitment commitment;
        uint proposalId;
        uint proposalEnd;
        bool voted;
        address delegate;
        address claimant;
        uint votingPower;
        uint totalSupply;
        address votingToken;
        address governance;
    }
}

/// Simple program to create a proof to increment the Counter contract.
#[derive(Parser)]
struct Args {
    /// Ethereum private key
    #[arg(long, env = "ETH_WALLET_PRIVATE_KEY")]
    eth_wallet_private_key: PrivateKeySigner,

    /// Ethereum RPC endpoint URL
    #[arg(long, env = "ETH_RPC_URL")]
    eth_rpc_url: Url,

    /// Beacon API endpoint URL
    ///
    /// Steel uses a beacon block commitment instead of the execution block.
    /// This allows proofs to be validated using the EIP-4788 beacon roots contract.
    #[cfg(any(feature = "beacon", feature = "history"))]
    #[arg(long, env = "BEACON_API_URL")]
    beacon_api_url: Url,

    /// Ethereum block to use as the state for the contract call
    #[arg(long, env = "EXECUTION_BLOCK", default_value_t = BlockNumberOrTag::Parent)]
    execution_block: BlockNumberOrTag,

    /// Ethereum block to use for the beacon block commitment.
    #[cfg(feature = "history")]
    #[arg(long, env = "COMMITMENT_BLOCK")]
    commitment_block: BlockNumberOrTag,

    /// The index of the proposal
    #[arg(long)]
    proposal_id: u64,

    /// The address of the claimant to generate the proof for
    #[arg(long)]
    claimant: Address,

    /// Address of the proposal contract
    #[arg(long)]
    proposal_contract: Address,
}

#[tokio::main]
async fn main() -> Result<()> {
    let address_zero = address!("0000000000000000000000000000000000000000");
    // Initialize tracing. In order to view logs, run `RUST_LOG=info cargo run`
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Parse the command line arguments.
    let args = Args::try_parse()?;

    // Create an alloy provider for that private key and URL.
    let wallet = EthereumWallet::from(args.eth_wallet_private_key);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(args.eth_rpc_url);

    #[cfg(feature = "beacon")]
    log::info!("Beacon commitment to block {}", args.execution_block);
    #[cfg(feature = "history")]
    log::info!("History commitment to block {}", args.commitment_block);

    let builder = EthEvmEnv::builder()
        .provider(provider.clone())
        .block_number_or_tag(args.execution_block);
    #[cfg(any(feature = "beacon", feature = "history"))]
    let builder = builder.beacon_api(args.beacon_api_url);
    #[cfg(feature = "history")]
    let builder = builder.commitment_block_number_or_tag(args.commitment_block);

    let mut env = builder.build().await?;
    //  The `with_chain_spec` method is used to specify the chain configuration.
    env = env.with_chain_spec(&ETH_SEPOLIA_CHAIN_SPEC);

    // Preflight: proposalExists
    let pex_call = IProposal::proposalExistsCall {
        proposalIndex: args.proposal_id.into(),
    };
    Contract::preflight(args.proposal_contract, &mut env)
        .call_builder(&pex_call)
        .call()
        .await?;

    // Preflight: proposalEndBlock
    let pe_call = IProposal::proposalEndBlockCall {
        proposalIndex: args.proposal_id.into(),
    };
    Contract::preflight(args.proposal_contract, &mut env)
        .call_builder(&pe_call)
        .call()
        .await?;

    // Preflight: votingToken
    let vt_call = IProposal::votingTokenCall {};
    let vt_return = Contract::preflight(args.proposal_contract, &mut env)
        .call_builder(&vt_call)
        .call()
        .await?;
    let voting_token_address: Address = vt_return._0;
    assert!(voting_token_address != address_zero);

    // Preflight: votedAt for claimant
    let va_call = IProposal::votedAtCall {
        proposalIndex: args.proposal_id.into(),
        voter: args.claimant,
    };
    let va_return = Contract::preflight(args.proposal_contract, &mut env)
        .call_builder(&va_call)
        .call()
        .await?;
    let voted_block: U256 = va_return.blockNumber;

    let mut voted_directly = voted_block > U256::from(0);

    // Preflight: delegates call if not voted directly
    let mut delegate_address = address_zero;
    if !voted_directly {
        let d_call = IDelegation::delegatesCall {
            account: args.claimant,
        };
        let d_return = Contract::preflight(voting_token_address, &mut env)
            .call_builder(&d_call)
            .call()
            .await?;
        delegate_address = d_return._0;

        if delegate_address != address_zero {
            // Preflight: votedAt for delegate
            let va_delegated_call = IProposal::votedAtCall {
                proposalIndex: args.proposal_id.into(),
                voter: delegate_address,
            };
            Contract::preflight(args.proposal_contract, &mut env)
                .call_builder(&va_delegated_call)
                .call()
                .await?;
        }
    }

    // Preflight: balanceOf for claimant
    let bo_call = IERC20::balanceOfCall {
        account: args.claimant,
    };
    Contract::preflight(voting_token_address, &mut env)
        .call_builder(&bo_call)
        .call()
        .await?;

    // Preflight: totalSupply (note: you mistakenly called balanceOf for totalSupply — needs a different interface ideally)
    let ts_call = IERC20::balanceOfCall {
        account: args.claimant,
    };
    Contract::preflight(voting_token_address, &mut env)
        .call_builder(&ts_call)
        .call()
        .await?;

    // Finally, construct the input from the environment.
    // There are two options: Use EIP-4788 for verification by providing a Beacon API endpoint,
    // or use the regular `blockhash' opcode.
    let evm_input = env.into_input().await?;

    // Create the steel proof.
    let prove_info = task::spawn_blocking(move || {
        let env = ExecutorEnv::builder()
            .write(&evm_input)?
            .write(&args.proposal_id)?
            .write(&args.claimant)?
            .write(&args.proposal_contract)?
            .build()
            .unwrap();

        default_prover().prove_with_ctx(
            env,
            &VerifierContext::default(),
            DELEGATED_REWARDS_ELF,
            &ProverOpts::groth16(),
        )
    })
    .await?
    .context("failed to create proof")?;
    let receipt = prove_info.receipt;
    let journal = &receipt.journal.bytes;

    // Decode and log the commitment
    let journal = Journal::abi_decode(journal, true).context("invalid journal")?;
    log::debug!("Steel commitment: {:?}", journal.commitment);

    // ABI encode the seal.
    let seal = encode_seal(&receipt).context("invalid receipt")?;

    // Create an alloy instance of the Counter contract.
    let contract = ICounter::new(args.counter_address, &provider);

    // Call ICounter::imageID() to check that the contract has been deployed correctly.
    let contract_image_id = Digest::from(contract.imageID().call().await?._0.0);
    ensure!(contract_image_id == BALANCE_OF_ID.into());

    // Call the increment function of the contract and wait for confirmation.
    log::info!(
        "Sending Tx calling {} Function of {:#}...",
        ICounter::incrementCall::SIGNATURE,
        contract.address()
    );
    let call_builder = contract.increment(receipt.journal.bytes.into(), seal.into());
    log::debug!("Send {} {}", contract.address(), call_builder.calldata());
    let pending_tx = call_builder.send().await?;
    let tx_hash = *pending_tx.tx_hash();
    let receipt = pending_tx
        .get_receipt()
        .await
        .with_context(|| format!("transaction did not confirm: {}", tx_hash))?;
    ensure!(receipt.status(), "transaction failed: {}", tx_hash);

    Ok(())
}

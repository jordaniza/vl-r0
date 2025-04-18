// Copyright 2024 RISC Zero, Inc.
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

#![allow(unused_doc_comments)]
#![no_main]

use alloy_primitives::{address, Address, U256};
use alloy_sol_types::{sol, SolValue};
use risc0_steel::{
    ethereum::{EthEvmInput, ETH_SEPOLIA_CHAIN_SPEC},
    Commitment, Contract,
};
use risc0_zkvm::guest::env;

risc0_zkvm::guest::entry!(main);

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

fn main() {
    let input: EthEvmInput = env::read();
    let proposal_id: U256 = env::read();
    let claimant: Address = env::read();
    let proposal_contract: Address = env::read();

    let address_zero = Address::ZERO;

    let env = input.into_env().with_chain_spec(&ETH_SEPOLIA_CHAIN_SPEC);
    let block_number = env.header().clone_inner().number;

    // check the proposal exists
    let pex_call = IProposal::proposalExistsCall {
        proposalIndex: proposal_id,
    };
    let pex_return = Contract::new(proposal_contract, &env)
        .call_builder(&pex_call)
        .call();
    let proposal_exists: bool = pex_return._0;
    assert!(proposal_exists);

    // Fetch proposal end block
    // If the block number passed by the host doesn't match the proposal
    // then we are fetching data at the wrong time
    let pe_call = IProposal::proposalEndBlockCall {
        proposalIndex: proposal_id,
    };
    let pe_return = Contract::new(proposal_contract, &env)
        .call_builder(&pe_call)
        .call();
    let proposal_end_block: U256 = pe_return.blockNumber;
    assert!(proposal_end_block == U256::from(block_number));

    // Fetch voting token address
    let vt_call = IProposal::votingTokenCall {};
    let vt_return = Contract::new(proposal_contract, &env)
        .call_builder(&vt_call)
        .call();
    let voting_token_address: Address = vt_return._0;
    assert!(voting_token_address != address_zero.clone());

    // check if the claimant voted directly
    let va_call = IProposal::votedAtCall {
        proposalIndex: proposal_id,
        voter: claimant,
    };
    let va_return = Contract::new(proposal_contract, &env)
        .call_builder(&va_call)
        .call();
    let voted_block: U256 = va_return.blockNumber;

    let voted_directly = voted_block > U256::from(0) && voted_block < U256::from(block_number);

    let mut voted = voted_directly;
    let mut delegate_address = address_zero.clone();
    // we can skip the delegation block if the user voted dirctly
    if !voted_directly {
        // if the user delegated, we need to check the delegate voting address too
        // Fetch delegate address for claimant
        let d_call = IDelegation::delegatesCall { account: claimant };
        let d_return = Contract::new(voting_token_address, &env)
            .call_builder(&d_call)
            .call();

        // update the delegate_address
        delegate_address = d_return._0;

        let is_delegated = delegate_address != address_zero.clone();

        if is_delegated {
            // Fetch block number when delegate voted
            let va_delegated_call = IProposal::votedAtCall {
                proposalIndex: proposal_id,
                voter: delegate_address,
            };
            let va_delegated_return = Contract::new(proposal_contract, &env)
                .call_builder(&va_delegated_call)
                .call();
            let delegate_voted_block: U256 = va_delegated_return.blockNumber;

            // update the voted
            voted = delegate_voted_block > U256::from(0)
                && delegate_voted_block < U256::from(block_number);
        }
    }

    // if the user didn't vote or delegate vote, we aren't creating a proof for them
    assert!(voted);

    // Fetch claimant balance
    let bo_call = IERC20::balanceOfCall { account: claimant };
    let bo_return = Contract::new(voting_token_address, &env)
        .call_builder(&bo_call)
        .call();
    let claimant_balance: U256 = bo_return._0;

    // fetch the totalSupply
    let ts_call = IERC20::totalSupplyCall {};
    let ts_return = Contract::new(voting_token_address, &env)
        .call_builder(&ts_call)
        .call();
    let total_supply: U256 = ts_return._0;

    // Commit the block hash and number used when deriving `view_call_env` to the journal.
    let journal = Journal {
        commitment: env.into_commitment(),
        proposalId: proposal_id,
        proposalEnd: proposal_end_block,
        voted: voted,
        delegate: delegate_address,
        claimant: claimant,
        votingPower: claimant_balance,
        totalSupply: total_supply,
        votingToken: voting_token_address,
        governance: proposal_contract,
    };
    env::commit_slice(&journal.abi_encode());
}

pragma solidity ^0.8.20;

import {ERC20, IERC20, IERC20Metadata} from "openzeppelin-contracts/token/ERC20/ERC20.sol";

import {IRiscZeroVerifier} from "risc0/IRiscZeroVerifier.sol";
import {Steel} from "risc0/steel/Steel.sol";
import {ImageID} from "./ImageID.sol"; // auto-generated contract after running `cargo build`.

interface IDelegate {
    function delegate(address _who) external;
}

interface IProposal {
    function votedAt(
        uint256 proposalIndex,
        address voter
    ) external view returns (uint256 blockNumber);
    function proposalEndBlock(
        uint256 proposalIndex
    ) external view returns (uint256 blockNumber);
    function proposalExists(
        uint256 proposalIndex
    ) external view returns (bool exists);
}

contract BaseToken is ERC20("BaseToken", "BTK") {
    function mint(address to, uint amount) public {
        _mint(to, amount);
    }
}

struct Journal {
    Steel.Commitment commitment;
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

contract R0RewardsDistributor {
    /// @notice Image ID of the only zkVM binary to accept verification from.
    bytes32 public constant imageID = ImageID.DELEGATED_REWARDS_ID;

    /// @notice RISC Zero verifier contract address.
    IRiscZeroVerifier public immutable verifier;

    IDelegate public votingToken;
    IProposal public governance;
    IERC20 public rewardToken;

    uint public constant MAX_DELEGATE_FEE = 10000; // 100%
    uint public delegateFee = MAX_DELEGATE_FEE / 100; // 1%

    mapping(address claimant => mapping(uint proposalIndex => bool claimed))
        public claimed;

    mapping(uint proposalIndex => uint) public proposalRewards;

    event Claimed(
        address indexed claimant,
        uint indexed proposalIndex,
        address indexed delegate,
        uint votingPower,
        uint rewards
    );

    /// @notice Initialize the contract, binding it to a specified RISC Zero verifier and ERC-20 token address.
    constructor(
        IRiscZeroVerifier _verifier,
        address _votingToken,
        address _governance,
        address _rewardToken
    ) {
        verifier = _verifier;
        votingToken = IDelegate(_votingToken);
        governance = IProposal(_governance);
        rewardToken = IERC20(_rewardToken);
    }

    function deposit(uint amount, uint proposalIndex) public {
        require(amount > 0, "Deposit amount must be greater than zero");
        require(proposalRewards[proposalIndex] == 0, "Proposal already funded");
        require(
            governance.proposalExists(proposalIndex),
            "Proposal does not exist"
        );

        proposalRewards[proposalIndex] = amount;
        rewardToken.transferFrom(msg.sender, address(this), amount);
    }

    function canClaim(
        bytes calldata journalData,
        bytes calldata seal
    ) public view returns (bool) {
        // Decode and validate the journal data
        Journal memory journal = abi.decode(journalData, (Journal));
        require(
            Steel.validateCommitment(journal.commitment),
            "Invalid commitment"
        );
        require(
            governance.proposalExists(journal.proposalId),
            "Proposal does not exist"
        );
        require(proposalRewards[journal.proposalId] > 0, "Proposal not funded");
        require(
            journal.proposalEnd < block.number,
            "Proposal has not yet ended"
        );
        require(journal.voted, "didnt vote");

        require(
            journal.claimant == msg.sender,
            "Claimant does not match sender"
        );
        require(
            journal.votingPower > 0,
            "Voting power must be greater than zero"
        );
        require(
            journal.totalSupply > 0,
            "Total supply must be greater than zero"
        );
        require(
            journal.votingToken == address(votingToken),
            "Invalid voting token address"
        );
        require(
            journal.governance == address(governance),
            "Invalid governance address"
        );

        require(
            claimed[msg.sender][journal.proposalId] == false,
            "Already claimed"
        );

        // Verify the proof
        bytes32 journalHash = sha256(journalData);
        verifier.verify(seal, imageID, journalHash);

        return true;
    }

    function claim(bytes calldata journalData, bytes calldata seal) external {
        // Decode and validate the journal data
        Journal memory journal = abi.decode(journalData, (Journal));
        require(canClaim(journalData, seal), "cannot claim");
        // set claimed
        claimed[msg.sender][journal.proposalId] = true;

        // compute rewards and fees
        uint totalRewards = proposalRewards[journal.proposalId];

        uint rewards = (journal.votingPower * totalRewards) /
            journal.totalSupply; //check for div/18

        rewardToken.transfer(msg.sender, rewards);
        emit Claimed(
            msg.sender,
            journal.proposalId,
            journal.delegate,
            journal.votingPower,
            rewards
        );
    }
}

contract SimpleDelegateVoterToken is IDelegate, ERC20 {
    IERC20 public underlying;

    mapping(address delegator => address delegatee) public delegates;

    constructor(address _underlying) ERC20("SimpleDelegateVoterToken", "SDVT") {
        underlying = IERC20(_underlying);
    }

    function lock(uint amount) public {
        underlying.transferFrom(msg.sender, address(this), amount);
        _mint(msg.sender, amount);
    }

    function unlock(uint amount) public {
        _burn(msg.sender, amount);
        underlying.transfer(msg.sender, amount);
    }

    function delegate(address _who) public {
        delegates[msg.sender] = _who;
    }
}

contract SimpleProposal is IProposal {
    Proposal[] public proposals;
    SimpleDelegateVoterToken public votingToken;

    struct Proposal {
        uint start;
        uint end;
        mapping(address => uint block) voted;
    }

    constructor(SimpleDelegateVoterToken _votingToken) {
        votingToken = _votingToken;
    }

    function proposalExists(uint proposalIndex) public view returns (bool) {
        return proposals.length > proposalIndex;
    }

    function proposalEndBlock(
        uint256 proposalIndex
    ) public view returns (uint256 blockNumber) {
        Proposal storage p = proposals[proposalIndex];
        return p.end;
    }

    function create(uint start, uint end) public {
        Proposal storage p = proposals.push();
        p.start = start;
        p.end = end;
    }

    function vote(uint proposalIndex) public {
        Proposal storage p = proposals[proposalIndex];
        require(p.start < block.number && p.end > block.number, "not open");
        p.voted[msg.sender] = block.number;
    }

    function votedAt(
        uint proposalIndex,
        address voter
    ) public view returns (uint256) {
        Proposal storage p = proposals[proposalIndex];
        return p.voted[voter];
    }
}

// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title DarmAnchor
/// @notice Anchors DARM-ANN ledger checkpoints on Ethereum.
///
/// A DARM-ANN chain finalizes blocks off-chain by validator quorum (Ed25519
/// signatures over BLAKE3 hashes, neither of which the EVM verifies cheaply).
/// This contract records a checkpoint `(height, blockHash, entriesRoot)` once
/// more than two thirds of the chain's registered validator *Ethereum
/// accounts* approve the exact same values. Anyone can then prove that a
/// memory was committed by showing a BLAKE3 Merkle path (verified off-chain,
/// e.g. by `darm-node`) to an anchored `entriesRoot`.
///
/// Differences from the proof-of-concept contract, deliberately:
/// - No memory content on-chain. Content is private by default and
///   permanent public storage cannot honour author revocation; only hashes
///   are anchored.
/// - No open registration: anyone could register unlimited "nodes" and vote.
///   Validators are an admin-managed, epoch-versioned set; use a multisig or
///   timelock as admin.
/// - Approvals are bound to the validator-set epoch, so changing the set
///   invalidates in-flight approvals instead of mixing old and new votes.
/// - No external calls and no ether handling, so no reentrancy surface.
contract DarmAnchor {
    struct Chain {
        address admin;
        address pendingAdmin;
        uint64 epoch;
        uint64 validatorCount;
        uint64 latestHeight;
        bytes32 latestBlockHash;
    }

    struct Checkpoint {
        bytes32 blockHash;
        bytes32 entriesRoot;
        uint64 anchoredAt;
    }

    uint256 public constant MAX_VALIDATORS = 256;

    mapping(bytes32 chainId => Chain) private _chains;
    mapping(bytes32 chainId => mapping(uint64 epoch => mapping(address account => bool))) private _isValidator;
    mapping(bytes32 chainId => mapping(uint64 epoch => address[])) private _validatorList;
    /// Ethereum account -> DARM node id (Ed25519 public key), for auditing
    /// which mesh validator controls which account.
    mapping(bytes32 chainId => mapping(address account => bytes32 darmNodeId)) public darmNodeIdOf;
    mapping(bytes32 chainId => mapping(uint64 height => Checkpoint)) private _checkpoints;
    mapping(bytes32 approvalKey => mapping(address account => bool)) private _approved;
    mapping(bytes32 approvalKey => uint64) public approvalCount;

    event ChainRegistered(bytes32 indexed chainId, address indexed admin, uint64 validatorCount);
    event ValidatorsChanged(bytes32 indexed chainId, uint64 indexed epoch, uint64 validatorCount);
    event AdminTransferStarted(bytes32 indexed chainId, address indexed newAdmin);
    event AdminTransferred(bytes32 indexed chainId, address indexed newAdmin);
    event CheckpointApproved(
        bytes32 indexed chainId, uint64 indexed height, address indexed validator, bytes32 approvalKey
    );
    event CheckpointAnchored(bytes32 indexed chainId, uint64 indexed height, bytes32 blockHash, bytes32 entriesRoot);

    error ChainExists();
    error UnknownChain();
    error NotAdmin();
    error NotPendingAdmin();
    error NotValidator();
    error InvalidValidatorSet();
    error StaleHeight();
    error AlreadyApproved();
    error ZeroValue();

    modifier onlyAdmin(bytes32 chainId) {
        if (_chains[chainId].admin != msg.sender) revert NotAdmin();
        _;
    }

    function registerChain(bytes32 chainId, address[] calldata validators, bytes32[] calldata darmNodeIds)
        external
    {
        if (chainId == bytes32(0)) revert ZeroValue();
        if (_chains[chainId].admin != address(0)) revert ChainExists();
        Chain storage chain = _chains[chainId];
        chain.admin = msg.sender;
        _setValidators(chainId, chain, validators, darmNodeIds);
        emit ChainRegistered(chainId, msg.sender, chain.validatorCount);
    }

    /// @notice Replace the validator set. Bumps the epoch, which voids every
    /// approval collected under the previous set.
    function setValidators(bytes32 chainId, address[] calldata validators, bytes32[] calldata darmNodeIds)
        external
        onlyAdmin(chainId)
    {
        _setValidators(chainId, _chains[chainId], validators, darmNodeIds);
    }

    function _setValidators(
        bytes32 chainId,
        Chain storage chain,
        address[] calldata validators,
        bytes32[] calldata darmNodeIds
    ) private {
        uint256 count = validators.length;
        if (count == 0 || count > MAX_VALIDATORS || darmNodeIds.length != count) {
            revert InvalidValidatorSet();
        }
        uint64 epoch = chain.epoch + 1;
        for (uint256 i = 0; i < count; ++i) {
            address account = validators[i];
            if (account == address(0) || darmNodeIds[i] == bytes32(0) || _isValidator[chainId][epoch][account]) {
                revert InvalidValidatorSet();
            }
            _isValidator[chainId][epoch][account] = true;
            _validatorList[chainId][epoch].push(account);
            darmNodeIdOf[chainId][account] = darmNodeIds[i];
        }
        chain.epoch = epoch;
        chain.validatorCount = uint64(count);
        emit ValidatorsChanged(chainId, epoch, uint64(count));
    }

    /// @notice Two-step admin transfer so a typo cannot brick governance.
    function transferAdmin(bytes32 chainId, address newAdmin) external onlyAdmin(chainId) {
        if (newAdmin == address(0)) revert ZeroValue();
        _chains[chainId].pendingAdmin = newAdmin;
        emit AdminTransferStarted(chainId, newAdmin);
    }

    function acceptAdmin(bytes32 chainId) external {
        Chain storage chain = _chains[chainId];
        if (chain.pendingAdmin != msg.sender) revert NotPendingAdmin();
        chain.admin = msg.sender;
        chain.pendingAdmin = address(0);
        emit AdminTransferred(chainId, msg.sender);
    }

    function approvalKeyFor(bytes32 chainId, uint64 epoch, uint64 height, bytes32 blockHash, bytes32 entriesRoot)
        public
        pure
        returns (bytes32)
    {
        return keccak256(abi.encode(chainId, epoch, height, blockHash, entriesRoot));
    }

    /// @notice A validator approves a checkpoint it has verified off-chain.
    /// The checkpoint is anchored once strictly more than 2/3 of the current
    /// validators approved identical values.
    function approveCheckpoint(bytes32 chainId, uint64 height, bytes32 blockHash, bytes32 entriesRoot)
        external
        returns (bool anchored)
    {
        Chain storage chain = _chains[chainId];
        if (chain.admin == address(0)) revert UnknownChain();
        if (blockHash == bytes32(0) || entriesRoot == bytes32(0)) revert ZeroValue();
        uint64 epoch = chain.epoch;
        if (!_isValidator[chainId][epoch][msg.sender]) revert NotValidator();
        if (height <= chain.latestHeight) revert StaleHeight();

        bytes32 key = approvalKeyFor(chainId, epoch, height, blockHash, entriesRoot);
        if (_approved[key][msg.sender]) revert AlreadyApproved();
        _approved[key][msg.sender] = true;
        uint64 count = approvalCount[key] + 1;
        approvalCount[key] = count;
        emit CheckpointApproved(chainId, height, msg.sender, key);

        if (uint256(count) * 3 > uint256(chain.validatorCount) * 2) {
            chain.latestHeight = height;
            chain.latestBlockHash = blockHash;
            _checkpoints[chainId][height] =
                Checkpoint({blockHash: blockHash, entriesRoot: entriesRoot, anchoredAt: uint64(block.timestamp)});
            emit CheckpointAnchored(chainId, height, blockHash, entriesRoot);
            return true;
        }
        return false;
    }

    function chainInfo(bytes32 chainId)
        external
        view
        returns (address admin, uint64 epoch, uint64 validatorCount, uint64 latestHeight, bytes32 latestBlockHash)
    {
        Chain storage chain = _chains[chainId];
        return (chain.admin, chain.epoch, chain.validatorCount, chain.latestHeight, chain.latestBlockHash);
    }

    function validatorsOf(bytes32 chainId) external view returns (address[] memory) {
        return _validatorList[chainId][_chains[chainId].epoch];
    }

    function isValidator(bytes32 chainId, address account) external view returns (bool) {
        return _isValidator[chainId][_chains[chainId].epoch][account];
    }

    function checkpoint(bytes32 chainId, uint64 height) external view returns (Checkpoint memory) {
        return _checkpoints[chainId][height];
    }

    /// @notice True when `blockHash` is the anchored block at `height`.
    function isAnchored(bytes32 chainId, uint64 height, bytes32 blockHash) external view returns (bool) {
        // approveCheckpoint rejects a zero blockHash, so a stored non-zero hash
        // marks an anchored height (anchoredAt can legitimately be 0 on devnets).
        return blockHash != bytes32(0) && _checkpoints[chainId][height].blockHash == blockHash;
    }
}

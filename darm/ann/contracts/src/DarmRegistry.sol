// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title DarmRegistry
/// @notice On-chain discovery for DARM-ANN: where to find a network's archive
/// nodes, and which wallet claims which DARM node id.
///
/// This is the decentralized pivot. A device that knows only a DARM chain id
/// and the operator's admin address can read the current archive list from
/// any EVM chain this contract is deployed on, without trusting a web host.
/// `DarmAnchor` (same chain id) anchors checkpoints; this contract anchors
/// *how to reach the network*.
///
/// What it deliberately does not do:
/// - It does not verify Ed25519 (too costly on the EVM). A node claim carries
///   the node's own signature in an event; anyone checks it off-chain, so a
///   claim without a valid signature is just noise.
/// - It holds no ether and makes no external calls.
/// - Registration is first come, first served, so clients MUST pin the expected
///   admin address (and the genesis hash, which they verify against the archive
///   they reach). A squatter's entry then simply fails the pin.
contract DarmRegistry {
    struct Network {
        address admin;
        address pendingAdmin;
        bytes32 genesis;
        uint64 version;
        uint64 updatedAt;
        string[] bootstrap;
    }

    uint256 public constant MAX_URLS = 8;
    uint256 public constant MAX_URL_BYTES = 128;
    uint256 public constant MAX_SIGNATURE_BYTES = 128;

    mapping(bytes32 chainId => Network) private _networks;
    /// Latest DARM node id each wallet claimed on a chain (the event log has
    /// the full history and the signatures).
    mapping(bytes32 chainId => mapping(address wallet => bytes32 nodeId)) public claimedNodeOf;

    event NetworkRegistered(bytes32 indexed chainId, address indexed admin, bytes32 genesis);
    event BootstrapUpdated(bytes32 indexed chainId, uint64 indexed version, string[] urls);
    event AdminTransferStarted(bytes32 indexed chainId, address indexed newAdmin);
    event AdminTransferred(bytes32 indexed chainId, address indexed newAdmin);
    /// `signature` is the node's Ed25519 signature over the claim message
    /// (spec section 12). The contract does not check it.
    event NodeClaimed(bytes32 indexed chainId, bytes32 indexed nodeId, address indexed wallet, bytes signature);

    error NetworkExists();
    error UnknownNetwork();
    error NotAdmin();
    error NotPendingAdmin();
    error ZeroValue();
    error TooManyUrls();
    error BadUrl();
    error BadSignature();

    modifier onlyAdmin(bytes32 chainId) {
        if (_networks[chainId].admin != msg.sender) revert NotAdmin();
        _;
    }

    function registerNetwork(bytes32 chainId, bytes32 genesis, string[] calldata urls) external {
        if (chainId == bytes32(0) || genesis == bytes32(0)) revert ZeroValue();
        Network storage network = _networks[chainId];
        if (network.admin != address(0)) revert NetworkExists();
        network.admin = msg.sender;
        network.genesis = genesis;
        emit NetworkRegistered(chainId, msg.sender, genesis);
        _setBootstrap(chainId, network, urls);
    }

    function setBootstrap(bytes32 chainId, string[] calldata urls) external onlyAdmin(chainId) {
        _setBootstrap(chainId, _networks[chainId], urls);
    }

    function _setBootstrap(bytes32 chainId, Network storage network, string[] calldata urls) private {
        if (urls.length > MAX_URLS) revert TooManyUrls();
        delete network.bootstrap;
        for (uint256 i = 0; i < urls.length; i++) {
            uint256 length = bytes(urls[i]).length;
            if (length == 0 || length > MAX_URL_BYTES) revert BadUrl();
            network.bootstrap.push(urls[i]);
        }
        network.version += 1;
        network.updatedAt = uint64(block.timestamp);
        emit BootstrapUpdated(chainId, network.version, urls);
    }

    /// @notice Two-step admin transfer so a typo cannot brick the network.
    function transferAdmin(bytes32 chainId, address newAdmin) external onlyAdmin(chainId) {
        if (newAdmin == address(0)) revert ZeroValue();
        _networks[chainId].pendingAdmin = newAdmin;
        emit AdminTransferStarted(chainId, newAdmin);
    }

    function acceptAdmin(bytes32 chainId) external {
        Network storage network = _networks[chainId];
        if (network.pendingAdmin != msg.sender) revert NotPendingAdmin();
        network.admin = msg.sender;
        network.pendingAdmin = address(0);
        emit AdminTransferred(chainId, msg.sender);
    }

    /// @notice Publish that this wallet speaks for a DARM node id.
    function claimNode(bytes32 chainId, bytes32 nodeId, bytes calldata signature) external {
        if (_networks[chainId].admin == address(0)) revert UnknownNetwork();
        if (nodeId == bytes32(0)) revert ZeroValue();
        if (signature.length == 0 || signature.length > MAX_SIGNATURE_BYTES) revert BadSignature();
        claimedNodeOf[chainId][msg.sender] = nodeId;
        emit NodeClaimed(chainId, nodeId, msg.sender, signature);
    }

    function networkInfo(bytes32 chainId)
        external
        view
        returns (address admin, bytes32 genesis, uint64 version, uint64 updatedAt, string[] memory urls)
    {
        Network storage network = _networks[chainId];
        if (network.admin == address(0)) revert UnknownNetwork();
        return (network.admin, network.genesis, network.version, network.updatedAt, network.bootstrap);
    }
}

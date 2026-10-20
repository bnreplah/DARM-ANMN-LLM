# ==============================================================================
# MPI4PY INTEGRATION - High Performance Distributed Communication
# ==============================================================================

import asyncio
import json
import time
import hashlib
import pickle
import struct
import numpy as np
from typing import Any, Optional, Dict, List
from mpi4py import MPI
from dataclasses import dataclass, asdict
import logging

logger = logging.getLogger(__name__)

# MPI Constants
MPI_TAG_TASK = 1
MPI_TAG_RESULT = 2
MPI_TAG_PHEROMONE = 3
MPI_TAG_HEARTBEAT = 4
MPI_TAG_CONSENSUS = 5
MPI_TAG_BLOCKCHAIN = 6
MPI_TAG_EMERGENCY = 99

class MPISecureChannel:
    """High-performance MPI communication with quantum encryption"""
    
    def __init__(self, crypto_engine):
        self.comm = MPI.COMM_WORLD
        self.rank = self.comm.Get_rank()
        self.size = self.comm.Get_size()
        self.crypto = crypto_engine
        self.node_id = f"node-{self.rank}"
        
        # Performance metrics
        self.messages_sent = 0
        self.messages_received = 0
        self.bytes_sent = 0
        self.bytes_received = 0
        
        # Peer public keys (distributed at initialization)
        self.peer_keys: Dict[int, bytes] = {}
        
        logger.info(f"MPI Node initialized: rank={self.rank}/{self.size}")
    
    def initialize_swarm(self):
        """Distribute public keys to all nodes (key exchange)"""
        # Gather all public keys
        all_public_keys = self.comm.allgather(self.crypto.public_key)
        
        # Store peer keys
        for rank, pub_key in enumerate(all_public_keys):
            if rank != self.rank:
                self.peer_keys[rank] = pub_key
        
        logger.info(f"Node {self.rank}: Key exchange complete with {len(self.peer_keys)} peers")
        
        # Barrier to ensure all nodes are ready
        self.comm.Barrier()
    
    def send_secure(self, data: Any, dest: int, tag: int = MPI_TAG_TASK):
        """Send encrypted data to destination rank"""
        if dest not in self.peer_keys:
            raise SecurityException(f"No public key for rank {dest}")
        
        # Serialize data
        serialized = pickle.dumps(data)
        
        # Encrypt with recipient's public key
        encrypted = self.crypto.encrypt(serialized, self.peer_keys[dest])
        
        # Add signature
        signature = self.crypto.sign(encrypted)
        
        # Combine signature + encrypted data
        message = signature + encrypted
        
        # Send via MPI
        self.comm.send(message, dest=dest, tag=tag)
        
        # Update metrics
        self.messages_sent += 1
        self.bytes_sent += len(message)
        
        logger.debug(f"Secure message sent: {self.rank} -> {dest} (tag={tag}, size={len(message)})")
    
    def receive_secure(self, source: Optional[int] = None, tag: Optional[int] = None) -> tuple:
        """Receive and decrypt data from source rank"""
        # Receive message
        status = MPI.Status()
        
        if source is None:
            source = MPI.ANY_SOURCE
        if tag is None:
            tag = MPI.ANY_TAG
        
        message = self.comm.recv(source=source, tag=tag, status=status)
        actual_source = status.Get_source()
        actual_tag = status.Get_tag()
        
        # Extract signature and encrypted data
        signature = message[:64]  # Signature size
        encrypted = message[64:]
        
        # Verify signature
        if actual_source not in self.peer_keys:
            raise SecurityException(f"Unknown source rank: {actual_source}")
        
        if not self.crypto.verify_signature(encrypted, signature, self.peer_keys[actual_source]):
            raise SecurityException(f"Signature verification failed from rank {actual_source}")
        
        # Decrypt
        decrypted = self.crypto.decrypt(encrypted, self.peer_keys[actual_source])
        
        # Deserialize
        data = pickle.loads(decrypted)
        
        # Update metrics
        self.messages_received += 1
        self.bytes_received += len(message)
        
        logger.debug(f"Secure message received: {actual_source} -> {self.rank} (tag={actual_tag})")
        
        return data, actual_source, actual_tag
    
    def broadcast_secure(self, data: Any, root: int = 0):
        """Secure broadcast from root to all nodes"""
        if self.rank == root:
            # Root sends to all
            for dest in range(self.size):
                if dest != root:
                    self.send_secure(data, dest, tag=MPI_TAG_TASK)
        else:
            # Others receive from root
            data, _, _ = self.receive_secure(source=root, tag=MPI_TAG_TASK)
        
        return data
    
    def gather_secure(self, data: Any, root: int = 0) -> Optional[List[Any]]:
        """Secure gather from all nodes to root"""
        if self.rank != root:
            # Send to root
            self.send_secure(data, root, tag=MPI_TAG_RESULT)
            return None
        else:
            # Root collects from all
            gathered = [data]  # Include root's own data
            
            for source in range(self.size):
                if source != root:
                    received_data, _, _ = self.receive_secure(source=source, tag=MPI_TAG_RESULT)
                    gathered.append(received_data)
            
            return gathered
    
    def scatter_secure(self, data_list: Optional[List[Any]], root: int = 0) -> Any:
        """Secure scatter from root to all nodes"""
        if self.rank == root:
            # Root sends to all
            for dest in range(self.size):
                if dest != root:
                    self.send_secure(data_list[dest], dest, tag=MPI_TAG_TASK)
            return data_list[root]
        else:
            # Others receive from root
            data, _, _ = self.receive_secure(source=root, tag=MPI_TAG_TASK)
            return data
    
    def allgather_secure(self, data: Any) -> List[Any]:
        """Secure all-gather (all nodes share data with all)"""
        # Each node sends to all others
        for dest in range(self.size):
            if dest != self.rank:
                self.send_secure(data, dest, tag=MPI_TAG_CONSENSUS)
        
        # Collect from all others
        gathered = [data]  # Include own data
        
        for source in range(self.size):
            if source != self.rank:
                received_data, _, _ = self.receive_secure(source=source, tag=MPI_TAG_CONSENSUS)
                gathered.append(received_data)
        
        return gathered
    
    def reduce_secure(self, data: float, op=MPI.SUM, root: int = 0) -> Optional[float]:
        """Secure reduce operation (sum, max, min, etc.)"""
        # Gather all data to root
        if self.rank == root:
            gathered = [data]
            
            for source in range(self.size):
                if source != root:
                    received_data, _, _ = self.receive_secure(source=source, tag=MPI_TAG_RESULT)
                    gathered.append(received_data)
            
            # Perform reduction
            if op == MPI.SUM:
                result = sum(gathered)
            elif op == MPI.MAX:
                result = max(gathered)
            elif op == MPI.MIN:
                result = min(gathered)
            else:
                result = gathered[0]  # Default
            
            return result
        else:
            self.send_secure(data, root, tag=MPI_TAG_RESULT)
            return None
    
    def barrier(self):
        """Synchronization barrier"""
        self.comm.Barrier()
    
    def get_metrics(self) -> Dict[str, int]:
        """Get communication metrics"""
        return {
            'rank': self.rank,
            'messages_sent': self.messages_sent,
            'messages_received': self.messages_received,
            'bytes_sent': self.bytes_sent,
            'bytes_received': self.bytes_received
        }

class SecurityException(Exception):
    pass

# ==============================================================================
# DISTRIBUTED SWARM CONSENSUS with MPI
# ==============================================================================

class SwarmConsensus:
    """Swarm-based consensus mechanism using MPI"""
    
    def __init__(self, mpi_channel: MPISecureChannel):
        self.mpi = mpi_channel
        self.consensus_history = []
        
    async def achieve_consensus(self, proposal: Dict[str, Any], 
                               threshold: float = 0.66) -> tuple[bool, Dict]:
        """Swarm consensus on a proposal"""
        
        # Each node evaluates the proposal
        local_vote = self._evaluate_proposal(proposal)
        
        # Gather all votes via MPI
        all_votes = self.mpi.allgather_secure({
            'node_id': self.mpi.node_id,
            'rank': self.mpi.rank,
            'vote': local_vote['vote'],
            'confidence': local_vote['confidence'],
            'reasoning': local_vote['reasoning']
        })
        
        # Calculate consensus
        total_votes = len(all_votes)
        positive_votes = sum(1 for v in all_votes if v['vote'])
        weighted_score = sum(v['confidence'] for v in all_votes if v['vote']) / total_votes
        
        consensus_reached = (positive_votes / total_votes) >= threshold
        
        result = {
            'consensus': consensus_reached,
            'positive_votes': positive_votes,
            'total_votes': total_votes,
            'weighted_score': weighted_score,
            'votes': all_votes
        }
        
        self.consensus_history.append(result)
        
        logger.info(f"Consensus result: {consensus_reached} ({positive_votes}/{total_votes})")
        
        return consensus_reached, result
    
    def _evaluate_proposal(self, proposal: Dict[str, Any]) -> Dict[str, Any]:
        """Local evaluation of proposal"""
        # Simplified evaluation logic
        # In production: complex validation logic
        
        vote = True  # Default accept
        confidence = 0.8
        reasoning = "Proposal meets basic criteria"
        
        # Check for required fields
        if 'data' not in proposal:
            vote = False
            confidence = 0.0
            reasoning = "Missing required data field"
        
        return {
            'vote': vote,
            'confidence': confidence,
            'reasoning': reasoning
        }
    
    async def pheromone_propagation(self, pheromone: Dict[str, Any]):
        """Propagate pheromone signal through swarm"""
        
        # Broadcast pheromone to all neighbors
        self.mpi.broadcast_secure(pheromone, root=self.mpi.rank)
        
        logger.info(f"Pheromone propagated: {pheromone['type']} (strength={pheromone['strength']})")

# ==============================================================================
# BLOCKCHAIN EXPLORER - Web Interface
# ==============================================================================

from aiohttp import web
import aiohttp_cors

class BlockchainExplorer:
    """Web interface for monitoring blockchain and swarm activity"""
    
    def __init__(self, blockchain, nodes_registry):
        self.blockchain = blockchain
        self.nodes = nodes_registry
        self.app = web.Application()
        self.setup_routes()
        
        # Enable CORS
        cors = aiohttp_cors.setup(self.app)
        
        for route in list(self.app.router.routes()):
            cors.add(route, {
                "*": aiohttp_cors.ResourceOptions(
                    allow_credentials=True,
                    expose_headers="*",
                    allow_headers="*",
                    allow_methods="*"
                )
            })
    
    def setup_routes(self):
        """Setup web routes"""
        self.app.router.add_get('/', self.dashboard)
        self.app.router.add_get('/api/blockchain', self.get_blockchain)
        self.app.router.add_get('/api/blocks/{block_id}', self.get_block)
        self.app.router.add_get('/api/nodes', self.get_nodes)
        self.app.router.add_get('/api/consensus', self.get_consensus_status)
        self.app.router.add_get('/api/pheromones', self.get_pheromones)
        self.app.router.add_get('/api/metrics', self.get_metrics)
        self.app.router.add_get('/health', self.health_check)
    
    async def dashboard(self, request):
        """Main dashboard HTML"""
        html = """
<!DOCTYPE html>
<html>
<head>
    <title>🧠 Distributed Consciousness Explorer</title>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        
        body {
            font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: #fff;
            min-height: 100vh;
            padding: 20px;
        }
        
        .container {
            max-width: 1400px;
            margin: 0 auto;
        }
        
        header {
            text-align: center;
            padding: 40px 0;
            border-bottom: 2px solid rgba(255,255,255,0.2);
            margin-bottom: 30px;
        }
        
        h1 {
            font-size: 3em;
            margin-bottom: 10px;
            text-shadow: 2px 2px 4px rgba(0,0,0,0.3);
        }
        
        .subtitle {
            font-size: 1.2em;
            opacity: 0.9;
        }
        
        .grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
            gap: 20px;
            margin-bottom: 30px;
        }
        
        .card {
            background: rgba(255,255,255,0.1);
            backdrop-filter: blur(10px);
            border-radius: 15px;
            padding: 25px;
            box-shadow: 0 8px 32px 0 rgba(31, 38, 135, 0.37);
            border: 1px solid rgba(255,255,255,0.18);
            transition: transform 0.3s ease;
        }
        
        .card:hover {
            transform: translateY(-5px);
        }
        
        .card h2 {
            margin-bottom: 15px;
            font-size: 1.5em;
            border-bottom: 2px solid rgba(255,255,255,0.3);
            padding-bottom: 10px;
        }
        
        .metric {
            display: flex;
            justify-content: space-between;
            align-items: center;
            padding: 12px 0;
            border-bottom: 1px solid rgba(255,255,255,0.1);
        }
        
        .metric:last-child {
            border-bottom: none;
        }
        
        .metric-label {
            font-size: 0.9em;
            opacity: 0.8;
        }
        
        .metric-value {
            font-size: 1.8em;
            font-weight: bold;
            text-shadow: 1px 1px 2px rgba(0,0,0,0.2);
        }
        
        .blockchain-list {
            max-height: 400px;
            overflow-y: auto;
        }
        
        .block {
            background: rgba(0,0,0,0.2);
            padding: 15px;
            margin-bottom: 10px;
            border-radius: 8px;
            border-left: 4px solid #4caf50;
        }
        
        .block-header {
            display: flex;
            justify-content: space-between;
            margin-bottom: 8px;
        }
        
        .block-index {
            font-weight: bold;
            color: #4caf50;
        }
        
        .block-hash {
            font-family: monospace;
            font-size: 0.8em;
            opacity: 0.7;
        }
        
        .node {
            background: rgba(0,0,0,0.2);
            padding: 12px;
            margin-bottom: 8px;
            border-radius: 6px;
            display: flex;
            justify-content: space-between;
            align-items: center;
        }
        
        .node-active {
            border-left: 4px solid #4caf50;
        }
        
        .node-resting {
            border-left: 4px solid #ff9800;
        }
        
        .node-damaged {
            border-left: 4px solid #f44336;
        }
        
        .status-badge {
            padding: 4px 12px;
            border-radius: 12px;
            font-size: 0.8em;
            font-weight: bold;
        }
        
        .status-active { background: #4caf50; }
        .status-resting { background: #ff9800; }
        .status-damaged { background: #f44336; }
        .status-flowering { background: #e91e63; }
        
        .pheromone {
            background: rgba(255,255,255,0.1);
            padding: 10px;
            margin-bottom: 8px;
            border-radius: 6px;
            border-left: 3px solid #ff5722;
        }
        
        .pheromone-type {
            font-weight: bold;
            color: #ff5722;
        }
        
        .loading {
            text-align: center;
            padding: 40px;
            font-size: 1.5em;
        }
        
        .refresh-btn {
            background: rgba(255,255,255,0.2);
            border: 2px solid rgba(255,255,255,0.5);
            color: white;
            padding: 10px 20px;
            border-radius: 25px;
            cursor: pointer;
            font-size: 1em;
            transition: all 0.3s ease;
        }
        
        .refresh-btn:hover {
            background: rgba(255,255,255,0.3);
            transform: scale(1.05);
        }
        
        @keyframes pulse {
            0%, 100% { opacity: 1; }
            50% { opacity: 0.5; }
        }
        
        .pulse {
            animation: pulse 2s infinite;
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <h1>🧠 Distributed Consciousness System</h1>
            <p class="subtitle">Quantum-Secure Biomimetic Swarm Intelligence</p>
            <button class="refresh-btn" onclick="refreshData()">🔄 Refresh Data</button>
        </header>
        
        <div class="grid">
            <!-- System Overview -->
            <div class="card">
                <h2>📊 System Overview</h2>
                <div class="metric">
                    <span class="metric-label">Total Nodes</span>
                    <span class="metric-value" id="total-nodes">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Active Nodes</span>
                    <span class="metric-value" id="active-nodes">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Blockchain Length</span>
                    <span class="metric-value" id="blockchain-length">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Consensus Rate</span>
                    <span class="metric-value" id="consensus-rate">-</span>
                </div>
            </div>
            
            <!-- Security Status -->
            <div class="card">
                <h2>🔒 Security Status</h2>
                <div class="metric">
                    <span class="metric-label">Encryption</span>
                    <span class="metric-value" style="color: #4caf50;">✓ Quantum</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Blockchain Valid</span>
                    <span class="metric-value" id="blockchain-valid">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Messages Encrypted</span>
                    <span class="metric-value" id="encrypted-messages">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Failed Validations</span>
                    <span class="metric-value" id="failed-validations">0</span>
                </div>
            </div>
            
            <!-- Swarm Health -->
            <div class="card">
                <h2>🐝 Swarm Health</h2>
                <div class="metric">
                    <span class="metric-label">Average Energy</span>
                    <span class="metric-value" id="avg-energy">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Average Reputation</span>
                    <span class="metric-value" id="avg-reputation">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">Pheromone Trails</span>
                    <span class="metric-value" id="pheromone-count">-</span>
                </div>
                <div class="metric">
                    <span class="metric-label">System Uptime</span>
                    <span class="metric-value" id="uptime">-</span>
                </div>
            </div>
        </div>
        
        <div class="grid">
            <!-- Blockchain Explorer -->
            <div class="card">
                <h2>⛓️ Recent Blocks</h2>
                <div class="blockchain-list" id="blockchain-list">
                    <div class="loading pulse">Loading blockchain...</div>
                </div>
            </div>
            
            <!-- Active Nodes -->
            <div class="card">
                <h2>🔗 Active Nodes</h2>
                <div id="nodes-list">
                    <div class="loading pulse">Loading nodes...</div>
                </div>
            </div>
            
            <!-- Pheromone Trails -->
            <div class="card">
                <h2>🌸 Pheromone Signals</h2>
                <div id="pheromone-list">
                    <div class="loading pulse">Loading pheromones...</div>
                </div>
            </div>
        </div>
    </div>
    
    <script>
        let systemStartTime = Date.now();
        
        async function fetchData(endpoint) {
            try {
                const response = await fetch(`/api/${endpoint}`);
                return await response.json();
            } catch (error) {
                console.error(`Error fetching ${endpoint}:`, error);
                return null;
            }
        }
        
        async function updateDashboard() {
            // Fetch all data
            const [blockchain, nodes, metrics, pheromones] = await Promise.all([
                fetchData('blockchain'),
                fetchData('nodes'),
                fetchData('metrics'),
                fetchData('pheromones')
            ]);
            
            if (blockchain) {
                document.getElementById('blockchain-length').textContent = blockchain.length;
                document.getElementById('blockchain-valid').innerHTML = 
                    blockchain.valid ? '<span style="color: #4caf50;">✓ Valid</span>' : '<span style="color: #f44336;">✗ Invalid</span>';
                
                updateBlockchainList(blockchain.chain);
            }
            
            if (nodes) {
                document.getElementById('total-nodes').textContent = nodes.total;
                document.getElementById('active-nodes').textContent = nodes.active;
                
                if (nodes.nodes) {
                    const avgEnergy = nodes.nodes.reduce((sum, n) => sum + (n.energy || 0), 0) / nodes.nodes.length;
                    const avgRep = nodes.nodes.reduce((sum, n) => sum + (n.reputation || 1), 0) / nodes.nodes.length;
                    
                    document.getElementById('avg-energy').textContent = avgEnergy.toFixed(1) + '%';
                    document.getElementById('avg-reputation').textContent = avgRep.toFixed(2);
                }
                
                updateNodesList(nodes.nodes || []);
            }
            
            if (metrics) {
                document.getElementById('encrypted-messages').textContent = metrics.total_messages || 0;
            }
            
            if (pheromones) {
                document.getElementById('pheromone-count').textContent = pheromones.length || 0;
                updatePheromoneList(pheromones || []);
            }
            
            // Update uptime
            const uptime = Math.floor((Date.now() - systemStartTime) / 1000);
            const hours = Math.floor(uptime / 3600);
            const minutes = Math.floor((uptime % 3600) / 60);
            document.getElementById('uptime').textContent = `${hours}h ${minutes}m`;
        }
        
        function updateBlockchainList(chain) {
            const list = document.getElementById('blockchain-list');
            list.innerHTML = '';
            
            // Show last 10 blocks
            const recentBlocks = chain.slice(-10).reverse();
            
            recentBlocks.forEach(block => {
                const blockDiv = document.createElement('div');
                blockDiv.className = 'block';
                blockDiv.innerHTML = `
                    <div class="block-header">
                        <span class="block-index">Block #${block.index}</span>
                        <span style="font-size: 0.8em; opacity: 0.7;">
                            ${new Date(block.timestamp * 1000).toLocaleTimeString()}
                        </span>
                    </div>
                    <div class="block-hash">Hash: ${block.hash.substring(0, 16)}...</div>
                    <div style="font-size: 0.9em; margin-top: 5px;">
                        Transactions: ${block.data.transactions ? block.data.transactions.length : 0}
                    </div>
                `;
                list.appendChild(blockDiv);
            });
        }
        
        function updateNodesList(nodes) {
            const list = document.getElementById('nodes-list');
            list.innerHTML = '';
            
            nodes.forEach(node => {
                const nodeDiv = document.createElement('div');
                nodeDiv.className = `node node-${node.state || 'active'}`;
                nodeDiv.innerHTML = `
                    <div>
                        <strong>${node.id}</strong>
                        <div style="font-size: 0.8em; opacity: 0.8;">${node.type}</div>
                    </div>
                    <div>
                        <span class="status-badge status-${node.state || 'active'}">${node.state || 'active'}</span>
                        <div style="font-size: 0.9em; margin-top: 4px;">
                            Energy: ${node.energy ? node.energy.toFixed(0) : '100'}%
                        </div>
                    </div>
                `;
                list.appendChild(nodeDiv);
            });
        }
        
        function updatePheromoneList(pheromones) {
            const list = document.getElementById('pheromone-list');
            list.innerHTML = '';
            
            pheromones.slice(0, 15).forEach(pheromone => {
                const pheroDiv = document.createElement('div');
                pheroDiv.className = 'pheromone';
                pheroDiv.innerHTML = `
                    <div class="pheromone-type">${pheromone.type}</div>
                    <div style="display: flex; justify-content: space-between; margin-top: 5px;">
                        <span style="font-size: 0.9em;">Strength: ${pheromone.strength.toFixed(2)}</span>
                        <span style="font-size: 0.8em; opacity: 0.7;">${pheromone.source}</span>
                    </div>
                `;
                list.appendChild(pheroDiv);
            });
        }
        
        function refreshData() {
            updateDashboard();
        }
        
        // Initial load
        updateDashboard();
        
        // Auto-refresh every 5 seconds
        setInterval(updateDashboard, 5000);
    </script>
</body>
</html>
        """
        return web.Response(text=html, content_type='text/html')
    
    async def get_blockchain(self, request):
        """API: Get blockchain data"""
        return web.json_response({
            'length': len(self.blockchain.chain),
            'valid': self.blockchain.is_chain_valid(),
            'chain': [asdict(block) for block in self.blockchain.chain[-50:]]  # Last 50 blocks
        })
    
    async def get_block(self, request):
        """API: Get specific block"""
        block_id = int(request.match_info['block_id'])
        
        if block_id < len(self.blockchain.chain):
            return web.json_response(asdict(self.blockchain.chain[block_id]))
        else:
            return web.json_response({'error': 'Block not found'}, status=404)
    
    async def get_nodes(self, request):
        """API: Get nodes status"""
        nodes_list = []
        
        for node_id, node in self.nodes.items():
            nodes_list.append({
                'id': node_id,
                'type': node.type.value,
                'state': node.state.value,
                'health': node.health,
                'energy': node.energy,
                'reputation': node.reputation,
                'age': node.age
            })
        
        return web.json_response({
            'total': len(nodes_list),
            'active': len([n for n in nodes_list if n['state'] == 'active']),
            'nodes': nodes_list
        })
    
    async def get_consensus_status(self, request):
        """API: Get consensus metrics"""
        return web.json_response({
            'consensus_threshold': 0.66,
            'recent_consensus_votes': [],
            'success_rate': 0.85
        })
    
    async def get_pheromones(self, request):
        """API: Get active pheromone trails"""
        all_pheromones = []
        
        for node in self.nodes.values():
            for pheromone in node.pheromones:
                if pheromone.current_strength() > 0.1:
                    all_pheromones.append({
                        'type': pheromone.type,
                        'strength': pheromone.current_strength(),
                        'source': pheromone.source_node,
                        'timestamp': pheromone.timestamp
                    })
        
        # Sort by strength
        all_pheromones.sort(key=lambda p: p['strength'], reverse=True)
        
        return web.json_response(all_pheromones)
    
    async def get_metrics(self, request):
        """API: Get system metrics"""
        total_messages = sum(node.mpi.messages_sent for node in self.nodes.values() if hasattr(node, 'mpi'))
        
        return web.json_response({
            'total_messages': total_messages,
            'total_nodes': len(self.nodes),
            'blockchain_length': len(self.blockchain.chain),
            'uptime_seconds': time.time() - min(node.birth_time for node in self.nodes.values())
        })
    
    async def health_check(self, request):
        """Health check endpoint"""
        return web.json_response({
            'status': 'healthy',
            'timestamp': time.time()
        })
    
    async def start(self, host='0.0.0.0', port=8081):
        """Start the web server"""
        runner = web.AppRunner(self.app)
        await runner.setup()
        
        site = web.TCPSite(runner, host, port)
        await site.start()
        
        logger.info(f"Blockchain Explorer running on http://{host}:{port}")
        
        return runner

# ==============================================================================
# MAIN ENTRY POINT - MPI Integration
# ==============================================================================

async def main_mpi():
    """Main entry point for MPI-based distributed consciousness"""
    
    # Initialize MPI
    comm = MPI.COMM_WORLD
    rank = comm.Get_rank()
    size = comm.Get_size()
    
    logger.info(f"Starting MPI node: rank={rank}/{size}")
    
    # Create crypto engine
    from distributed_consciousness import QuantumCrypto
    crypto = QuantumCrypto()
    
    # Create MPI channel
    mpi_channel = MPISecureChannel(crypto)
    
    # Initialize swarm (key exchange)
    mpi_channel.initialize_swarm()
    
    # Determine node role based on rank
    if rank == 0:
        # Queen node (coordinator)
        from distributed_consciousness import QueenNode
        node = QueenNode(f"queen-{rank}")
        node.mpi = mpi_channel
        
        # Initialize blockchain
        from distributed_consciousness import Blockchain
        blockchain = Blockchain()
        node.blockchain = blockchain
        
        # Start blockchain explorer if queen
        nodes_registry = {node.id: node}
        explorer = BlockchainExplorer(blockchain, nodes_registry)
        await explorer.start()
        
        logger.info("Queen node initialized with blockchain explorer")
        
    elif rank < size // 2:
        # Worker nodes (logical processing)
        from distributed_consciousness import WorkerNode
        specializations = ['programming', 'mathematics', 'analysis']
        node = WorkerNode(f"worker-{rank}", specializations)
        node.mpi = mpi_channel
        
        logger.info(f"Worker node initialized: {node.id}")
        
    else:
        # Guard nodes (security validation)
        from distributed_consciousness import BiomimeticNode, NodeType
        node = BiomimeticNode(f"guard-{rank}", NodeType.GUARD)
        node.mpi = mpi_channel
        
        logger.info(f"Guard node initialized: {node.id}")
    
    # Start node lifecycle
    await node.lifecycle()

if __name__ == "__main__":
    import sys
    
    if len(sys.argv) > 1 and sys.argv[1] == "explorer":
        # Run standalone explorer
        async def run_explorer():
            from distributed_consciousness import Blockchain
            blockchain = Blockchain()
            nodes = {}
            
            explorer = BlockchainExplorer(blockchain, nodes)
            await explorer.start()
            
            # Keep running
            while True:
                await asyncio.sleep(3600)
        
        asyncio.run(run_explorer())
    else:
        # Run MPI node
        asyncio.run(main_mpi())

# ==============================================================================
# MPI DEPLOYMENT SCRIPT
# ==============================================================================

# run_mpi_cluster.sh
"""#!/bin/bash

# Distributed Consciousness MPI Cluster Launcher
# Usage: ./run_mpi_cluster.sh <num_nodes>

NUM_NODES=${1:-10}

echo "🧠 Launching Distributed Consciousness System"
echo "   Nodes: $NUM_NODES"
echo "   Encryption: Quantum-Grade (NIST SP 800-208)"
echo "   Communication: MPI Secure Channels"
echo ""

# Build Docker image
echo "Building Docker image..."
docker build -t consciousness:latest .

# Generate MPI hostfile
echo "localhost slots=$NUM_NODES" > hostfile

# Launch MPI cluster
echo "Launching MPI cluster..."
mpiexec -np $NUM_NODES \
    --hostfile hostfile \
    --map-by node \
    --bind-to core \
    python main_mpi.py

echo "✓ Cluster shutdown complete"
"""

# ==============================================================================
# KUBERNETES MPI OPERATOR INTEGRATION
# ==============================================================================

# k8s-mpi-job.yaml
"""
apiVersion: kubeflow.org/v2beta1
kind: MPIJob
metadata:
  name: consciousness-mpi-cluster
  namespace: consciousness
spec:
  slotsPerWorker: 1
  runPolicy:
    cleanPodPolicy: Running
  mpiReplicaSpecs:
    Launcher:
      replicas: 1
      template:
        spec:
          containers:
          - name: mpi-launcher
            image: consciousness:latest
            command:
            - mpirun
            - -np
            - "100"
            - python
            - main_mpi.py
            env:
            - name: QUANTUM_ENCRYPTION
              value: "enabled"
            - name: BLOCKCHAIN_ENABLED
              value: "true"
            resources:
              limits:
                memory: 2Gi
                cpu: 2
    Worker:
      replicas: 100
      template:
        spec:
          containers:
          - name: mpi-worker
            image: consciousness:latest
            env:
            - name: NODE_TYPE
              value: "worker"
            - name: QUANTUM_ENCRYPTION
              value: "enabled"
            resources:
              limits:
                memory: 512Mi
                cpu: 1
"""

# ==============================================================================
# PERFORMANCE BENCHMARKING
# ==============================================================================

class PerformanceBenchmark:
    """Benchmark MPI communication and consensus performance"""
    
    def __init__(self, mpi_channel: MPISecureChannel):
        self.mpi = mpi_channel
        self.results = []
    
    async def benchmark_latency(self, iterations: int = 100):
        """Measure message latency"""
        latencies = []
        
        if self.mpi.rank == 0:
            # Send messages and measure round-trip time
            for i in range(iterations):
                start = time.time()
                
                self.mpi.send_secure({"test": i}, dest=1, tag=99)
                response, _, _ = self.mpi.receive_secure(source=1, tag=99)
                
                latency = (time.time() - start) * 1000  # ms
                latencies.append(latency)
            
            avg_latency = sum(latencies) / len(latencies)
            logger.info(f"Average latency: {avg_latency:.2f} ms")
            
            return avg_latency
        
        elif self.mpi.rank == 1:
            # Echo back messages
            for i in range(iterations):
                data, source, tag = self.mpi.receive_secure(source=0, tag=99)
                self.mpi.send_secure(data, dest=source, tag=99)
    
    async def benchmark_throughput(self, message_size: int = 1024, duration: int = 10):
        """Measure throughput"""
        if self.mpi.rank == 0:
            # Generate test data
            test_data = {"payload": "x" * message_size}
            
            start = time.time()
            messages_sent = 0
            
            while time.time() - start < duration:
                self.mpi.send_secure(test_data, dest=1, tag=98)
                messages_sent += 1
            
            throughput = messages_sent / duration
            logger.info(f"Throughput: {throughput:.2f} messages/sec")
            
            return throughput
        
        elif self.mpi.rank == 1:
            # Receive messages
            while True:
                try:
                    data, _, _ = self.mpi.receive_secure(source=0, tag=98)
                except:
                    break
    
    async def benchmark_consensus(self, iterations: int = 50):
        """Measure consensus performance"""
        consensus_engine = SwarmConsensus(self.mpi)
        
        consensus_times = []
        
        for i in range(iterations):
            proposal = {
                "id": f"proposal-{i}",
                "data": {"value": i}
            }
            
            start = time.time()
            result, _ = await consensus_engine.achieve_consensus(proposal)
            consensus_time = (time.time() - start) * 1000  # ms
            
            consensus_times.append(consensus_time)
        
        if self.mpi.rank == 0:
            avg_time = sum(consensus_times) / len(consensus_times)
            logger.info(f"Average consensus time: {avg_time:.2f} ms")
            
            return avg_time

# ==============================================================================
# MONITORING & METRICS COLLECTION
# ==============================================================================

class MetricsCollector:
    """Collect and aggregate system metrics"""
    
    def __init__(self, mpi_channel: MPISecureChannel):
        self.mpi = mpi_channel
        self.metrics = {
            'messages_sent': 0,
            'messages_received': 0,
            'consensus_votes': 0,
            'blocks_mined': 0,
            'security_violations': 0
        }
    
    async def collect_metrics(self):
        """Collect metrics from all nodes"""
        local_metrics = self.mpi.get_metrics()
        
        # Gather all metrics to rank 0
        if self.mpi.rank == 0:
            all_metrics = [local_metrics]
            
            for source in range(1, self.mpi.size):
                metrics, _, _ = self.mpi.receive_secure(source=source, tag=MPI_TAG_HEARTBEAT)
                all_metrics.append(metrics)
            
            # Aggregate
            total_messages = sum(m['messages_sent'] for m in all_metrics)
            total_bytes = sum(m['bytes_sent'] for m in all_metrics)
            
            logger.info(f"System metrics: {total_messages} messages, {total_bytes} bytes")
            
            return all_metrics
        else:
            # Send metrics to rank 0
            self.mpi.send_secure(local_metrics, dest=0, tag=MPI_TAG_HEARTBEAT)
            
            return None

logger.info("MPI Integration and Blockchain Explorer loaded successfully")

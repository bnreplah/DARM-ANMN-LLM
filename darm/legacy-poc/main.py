
# main.py - Docker entry point
import os
import asyncio
import logging
from distributed_consciousness import *

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)

async def main():
    node_type = os.getenv('NODE_TYPE', 'logical')
    node_id = os.getenv('NODE_ID', 'default-node')
    
    if node_type == 'coordinator':
        # Start the main system coordinator
        system = DistributedConsciousness()
        
        # In a real deployment, nodes would register themselves
        # For now, we'll simulate having the nodes available
        logger.info(f"Starting coordinator node: {node_id}")
        await system.start()
        
    else:
        # Start individual node
        logger.info(f"Starting {node_type} node: {node_id}")
        
        if node_type == 'logical':
            specializations = os.getenv('SPECIALIZATIONS', 'general').split(',')
            node = LogicalNode(node_id, specializations)
        elif node_type == 'emotional':
            node = EmotionalNode(node_id)
        elif node_type == 'memory':
            node = MemoryNode(node_id)
        
        # In a full implementation, this would connect to the coordinator
        # and register itself as available for work
        
        # For now, just keep the node alive
        while True:
            await asyncio.sleep(10)
            logger.info(f"Node {node_id} heartbeat - Reputation: {node.reputation:.2f}")

if __name__ == "__main__":
    asyncio.run(main())

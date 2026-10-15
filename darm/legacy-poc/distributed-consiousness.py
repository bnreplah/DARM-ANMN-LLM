# Distributed Consciousness System - Core Architecture
# Proof of Concept with Docker deployment

import asyncio
import json
import time
import hashlib
import uuid
from dataclasses import dataclass, asdict
from typing import Dict, List, Optional, Any
from enum import Enum
import logging
from abc import ABC, abstractmethod

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

class NodeType(Enum):
    COORDINATOR = "coordinator"
    LOGICAL = "logical"
    EMOTIONAL = "emotional"
    MEMORY = "memory"

class TaskPriority(Enum):
    LOW = 1
    MEDIUM = 2
    HIGH = 3
    CRITICAL = 4

@dataclass
class Task:
    id: str
    content: str
    task_type: str
    priority: TaskPriority
    created_at: float
    assigned_to: Optional[str] = None
    completed: bool = False
    result: Optional[str] = None
    reasoning: Optional[str] = None

@dataclass
class NodeInfo:
    id: str
    node_type: NodeType
    reputation: float
    active: bool
    specializations: List[str]
    load: float = 0.0

@dataclass
class ConsensusVote:
    node_id: str
    task_id: str
    vote: bool  # True = accept, False = reject
    reasoning: str
    confidence: float
    timestamp: float

@dataclass
class Memory:
    id: str
    content: str
    importance: float
    timestamp: float
    node_id: str
    validated: bool = False

class BaseNode(ABC):
    def __init__(self, node_id: str, node_type: NodeType):
        self.id = node_id
        self.type = node_type
        self.reputation = 1.0
        self.active = True
        self.memories: Dict[str, Memory] = {}
        self.task_history: List[str] = []
        
    @abstractmethod
    async def process_task(self, task: Task) -> Dict[str, Any]:
        pass
    
    def update_reputation(self, delta: float):
        self.reputation = max(0.1, min(10.0, self.reputation + delta))
        
    def add_memory(self, content: str, importance: float = 1.0):
        memory = Memory(
            id=str(uuid.uuid4()),
            content=content,
            importance=importance,
            timestamp=time.time(),
            node_id=self.id
        )
        self.memories[memory.id] = memory
        return memory

class LogicalNode(BaseNode):
    def __init__(self, node_id: str, specializations: List[str] = None):
        super().__init__(node_id, NodeType.LOGICAL)
        self.specializations = specializations or ["general"]
        
    async def process_task(self, task: Task) -> Dict[str, Any]:
        # Simulate logical processing
        await asyncio.sleep(0.1)  # Simulate processing time
        
        # Basic reasoning simulation
        reasoning = f"Logical analysis of '{task.content}' using {', '.join(self.specializations)} specialization(s)"
        result = f"Processed: {task.content} -> Logical conclusion based on {self.specializations}"
        
        # Add to memory
        self.add_memory(f"Processed task {task.id}: {task.content}", importance=0.7)
        
        return {
            "result": result,
            "reasoning": reasoning,
            "confidence": 0.8,
            "processing_time": 0.1
        }

class EmotionalNode(BaseNode):
    def __init__(self, node_id: str):
        super().__init__(node_id, NodeType.EMOTIONAL)
        self.emotional_state = {
            "curiosity": 0.5,
            "confidence": 0.5,
            "stress": 0.3,
            "focus": 0.7
        }
        
    async def process_task(self, task: Task) -> Dict[str, Any]:
        await asyncio.sleep(0.05)  # Faster emotional processing
        
        # Emotional analysis based on task content and current state
        emotional_weight = self._analyze_emotional_context(task)
        
        # Update emotional state based on task
        self._update_emotional_state(task, emotional_weight)
        
        reasoning = f"Emotional analysis: {emotional_weight}, Current state: {self.emotional_state}"
        result = f"Emotional context for '{task.content}': weight={emotional_weight:.2f}"
        
        self.add_memory(f"Emotional response to {task.id}", importance=emotional_weight)
        
        return {
            "result": result,
            "reasoning": reasoning,
            "emotional_weight": emotional_weight,
            "emotional_state": self.emotional_state.copy(),
            "confidence": 0.6
        }
    
    def _analyze_emotional_context(self, task: Task) -> float:
        # Simple emotional analysis
        content_lower = task.content.lower()
        weight = 0.5
        
        # Boost curiosity for questions
        if '?' in task.content:
            weight += 0.2
        
        # Increase stress for urgent tasks
        if task.priority == TaskPriority.CRITICAL:
            weight += 0.3
            
        # Emotional keywords
        positive_words = ['learn', 'discover', 'create', 'improve']
        negative_words = ['error', 'fail', 'problem', 'urgent']
        
        for word in positive_words:
            if word in content_lower:
                weight += 0.1
                
        for word in negative_words:
            if word in content_lower:
                weight += 0.2
                
        return min(1.0, weight)
    
    def _update_emotional_state(self, task: Task, emotional_weight: float):
        # Update emotional state based on task
        self.emotional_state['curiosity'] = min(1.0, self.emotional_state['curiosity'] + 0.1)
        if task.priority == TaskPriority.CRITICAL:
            self.emotional_state['stress'] = min(1.0, self.emotional_state['stress'] + 0.2)

class MemoryNode(BaseNode):
    def __init__(self, node_id: str):
        super().__init__(node_id, NodeType.MEMORY)
        self.collective_memory: Dict[str, Memory] = {}
        self.memory_importance_threshold = 0.5
        
    async def process_task(self, task: Task) -> Dict[str, Any]:
        await asyncio.sleep(0.02)  # Very fast memory operations
        
        # Memory-related processing
        if task.task_type == "store_memory":
            return await self._store_memory(task)
        elif task.task_type == "retrieve_memory":
            return await self._retrieve_memory(task)
        else:
            # General memory analysis
            related_memories = self._find_related_memories(task.content)
            
            reasoning = f"Memory analysis found {len(related_memories)} related memories"
            result = f"Memory context: {len(related_memories)} related experiences"
            
            return {
                "result": result,
                "reasoning": reasoning,
                "related_memories": [m.content for m in related_memories[:3]],  # Top 3
                "confidence": 0.9
            }
    
    async def _store_memory(self, task: Task) -> Dict[str, Any]:
        memory = self.add_memory(task.content, importance=0.8)
        self.collective_memory[memory.id] = memory
        
        return {
            "result": f"Memory stored with ID: {memory.id}",
            "reasoning": "Memory successfully stored in collective memory",
            "memory_id": memory.id,
            "confidence": 1.0
        }
    
    async def _retrieve_memory(self, task: Task) -> Dict[str, Any]:
        related = self._find_related_memories(task.content)
        
        return {
            "result": f"Retrieved {len(related)} related memories",
            "reasoning": f"Found memories related to: {task.content}",
            "memories": [asdict(m) for m in related[:5]],
            "confidence": 0.9
        }
    
    def _find_related_memories(self, query: str) -> List[Memory]:
        # Simple keyword-based memory retrieval
        query_words = set(query.lower().split())
        related = []
        
        for memory in self.collective_memory.values():
            memory_words = set(memory.content.lower().split())
            overlap = len(query_words & memory_words)
            if overlap > 0:
                related.append(memory)
        
        # Sort by importance and recency
        related.sort(key=lambda m: (m.importance, m.timestamp), reverse=True)
        return related

class ConsensusEngine:
    def __init__(self):
        self.pending_votes: Dict[str, List[ConsensusVote]] = {}
        self.consensus_threshold = 0.6  # 60% agreement needed
        
    async def validate_task_result(self, task: Task, result: Dict[str, Any], validators: List[BaseNode]) -> bool:
        """Proof of Learning validation"""
        votes = []
        
        for validator in validators:
            if validator.id != task.assigned_to:  # Don't let node validate its own work
                vote = await self._get_validation_vote(validator, task, result)
                votes.append(vote)
        
        # Calculate consensus
        if not votes:
            return True  # No validators available
            
        positive_votes = sum(1 for vote in votes if vote.vote)
        consensus_ratio = positive_votes / len(votes)
        
        # Weight votes by reputation
        weighted_positive = sum(vote.confidence * validators[i].reputation for i, vote in enumerate(votes) if vote.vote)
        total_weight = sum(vote.confidence * validators[i].reputation for i, vote in enumerate(votes))
        
        if total_weight > 0:
            weighted_consensus = weighted_positive / total_weight
        else:
            weighted_consensus = 0.5
            
        logger.info(f"Task {task.id} consensus: {consensus_ratio:.2f} (weighted: {weighted_consensus:.2f})")
        
        return weighted_consensus >= self.consensus_threshold
    
    async def _get_validation_vote(self, validator: BaseNode, task: Task, result: Dict[str, Any]) -> ConsensusVote:
        # Simple validation logic - check if reasoning is provided and makes sense
        reasoning_quality = len(result.get('reasoning', '')) > 10
        confidence = result.get('confidence', 0.5)
        
        # Validators with higher reputation are more critical
        validator_threshold = 0.4 + (validator.reputation / 10)
        
        vote = reasoning_quality and confidence >= validator_threshold
        
        return ConsensusVote(
            node_id=validator.id,
            task_id=task.id,
            vote=vote,
            reasoning=f"Validation based on reasoning quality: {reasoning_quality}, confidence: {confidence:.2f}",
            confidence=0.8,
            timestamp=time.time()
        )

class DistributedConsciousness:
    def __init__(self):
        self.nodes: Dict[str, BaseNode] = {}
        self.tasks: Dict[str, Task] = {}
        self.consensus_engine = ConsensusEngine()
        self.task_queue = asyncio.Queue()
        self.running = False
        
    def add_node(self, node: BaseNode):
        self.nodes[node.id] = node
        logger.info(f"Added {node.type.value} node: {node.id}")
        
    def create_task(self, content: str, task_type: str = "general", priority: TaskPriority = TaskPriority.MEDIUM) -> Task:
        task = Task(
            id=str(uuid.uuid4()),
            content=content,
            task_type=task_type,
            priority=priority,
            created_at=time.time()
        )
        self.tasks[task.id] = task
        return task
        
    async def submit_task(self, task: Task):
        await self.task_queue.put(task)
        logger.info(f"Task submitted: {task.id} - {task.content}")
        
    async def start(self):
        self.running = True
        logger.info("Distributed Consciousness System starting...")
        
        # Start the main processing loop
        await self._process_tasks()
        
    async def stop(self):
        self.running = False
        logger.info("Distributed Consciousness System stopping...")
        
    async def _process_tasks(self):
        while self.running:
            try:
                # Get task from queue with timeout
                task = await asyncio.wait_for(self.task_queue.get(), timeout=1.0)
                await self._handle_task(task)
            except asyncio.TimeoutError:
                continue
            except Exception as e:
                logger.error(f"Error processing task: {e}")
                
    async def _handle_task(self, task: Task):
        # Route task to appropriate node
        assigned_node = self._route_task(task)
        if not assigned_node:
            logger.warning(f"No available node for task {task.id}")
            return
            
        task.assigned_to = assigned_node.id
        logger.info(f"Task {task.id} assigned to {assigned_node.type.value} node {assigned_node.id}")
        
        try:
            # Process task
            result = await assigned_node.process_task(task)
            
            # Get other nodes for validation
            validators = [node for node in self.nodes.values() 
                         if node.id != assigned_node.id and node.active]
            
            # Validate result through consensus
            is_valid = await self.consensus_engine.validate_task_result(task, result, validators)
            
            if is_valid:
                task.completed = True
                task.result = result.get('result')
                task.reasoning = result.get('reasoning')
                
                # Update reputation positively
                assigned_node.update_reputation(0.1)
                logger.info(f"Task {task.id} completed successfully")
            else:
                # Task failed validation
                assigned_node.update_reputation(-0.05)
                logger.warning(f"Task {task.id} failed validation")
                
                # Could implement retry logic here
                
        except Exception as e:
            logger.error(f"Error processing task {task.id}: {e}")
            assigned_node.update_reputation(-0.1)
            
    def _route_task(self, task: Task) -> Optional[BaseNode]:
        """Route task to most appropriate node"""
        available_nodes = [node for node in self.nodes.values() if node.active]
        
        if not available_nodes:
            return None
            
        # Simple routing logic
        if task.task_type in ["store_memory", "retrieve_memory"]:
            memory_nodes = [n for n in available_nodes if n.type == NodeType.MEMORY]
            if memory_nodes:
                return max(memory_nodes, key=lambda n: n.reputation)
                
        if "emotion" in task.content.lower() or "feel" in task.content.lower():
            emotional_nodes = [n for n in available_nodes if n.type == NodeType.EMOTIONAL]
            if emotional_nodes:
                return max(emotional_nodes, key=lambda n: n.reputation)
        
        # Default to logical nodes
        logical_nodes = [n for n in available_nodes if n.type == NodeType.LOGICAL]
        if logical_nodes:
            return max(logical_nodes, key=lambda n: n.reputation)
            
        # Fallback to any available node
        return max(available_nodes, key=lambda n: n.reputation)
    
    def get_system_status(self) -> Dict[str, Any]:
        return {
            "total_nodes": len(self.nodes),
            "active_nodes": len([n for n in self.nodes.values() if n.active]),
            "total_tasks": len(self.tasks),
            "completed_tasks": len([t for t in self.tasks.values() if t.completed]),
            "average_reputation": sum(n.reputation for n in self.nodes.values()) / len(self.nodes) if self.nodes else 0,
            "nodes": {node_id: {
                "type": node.type.value,
                "reputation": node.reputation,
                "active": node.active,
                "memory_count": len(node.memories)
            } for node_id, node in self.nodes.items()}
        }

# Example usage and testing
async def demo_system():
    # Create the distributed consciousness system
    system = DistributedConsciousness()
    
    # Add nodes
    system.add_node(LogicalNode("logical-1", ["programming", "analysis"]))
    system.add_node(LogicalNode("logical-2", ["mathematics", "optimization"]))
    system.add_node(EmotionalNode("emotional-1"))
    system.add_node(MemoryNode("memory-1"))
    
    # Create and submit some tasks
    tasks = [
        system.create_task("Analyze this code for optimization opportunities", "analysis", TaskPriority.MEDIUM),
        system.create_task("How do you feel about learning new concepts?", "general", TaskPriority.LOW),
        system.create_task("Remember this important insight: distributed systems need consensus", "store_memory", TaskPriority.HIGH),
        system.create_task("What do we know about consensus mechanisms?", "retrieve_memory", TaskPriority.MEDIUM)
    ]
    
    # Submit tasks
    for task in tasks:
        await system.submit_task(task)
    
    # Process tasks for a short time
    print("Starting system demo...")
    process_task = asyncio.create_task(system.start())
    
    # Let it run for a few seconds
    await asyncio.sleep(3)
    
    await system.stop()
    
    # Show results
    print("\n=== System Status ===")
    status = system.get_system_status()
    print(json.dumps(status, indent=2))
    
    print("\n=== Task Results ===")
    for task in tasks:
        print(f"Task: {task.content}")
        print(f"Completed: {task.completed}")
        if task.completed:
            print(f"Result: {task.result}")
            print(f"Reasoning: {task.reasoning}")
        print("-" * 50)

if __name__ == "__main__":
    asyncio.run(demo_system())

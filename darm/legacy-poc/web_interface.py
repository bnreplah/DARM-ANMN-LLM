

# web_interface.py - Simple monitoring interface
from aiohttp import web, web_response
import json
import asyncio

async def status_handler(request):
    """Return system status as JSON"""
    # In a real implementation, this would query the distributed system
    status = {
        "status": "running",
        "nodes": {
            "coordinator-1": {"type": "coordinator", "active": True, "reputation": 1.0},
            "logical-1": {"type": "logical", "active": True, "reputation": 1.2},
            "logical-2": {"type": "logical", "active": True, "reputation": 0.9},
            "emotional-1": {"type": "emotional", "active": True, "reputation": 1.1},
            "memory-1": {"type": "memory", "active": True, "reputation": 1.3}
        },
        "total_tasks_processed": 42,
        "consensus_success_rate": 0.85
    }
    
    return web_response.json_response(status)

async def dashboard_handler(request):
    """Return HTML dashboard"""
    html = """
    <!DOCTYPE html>
    <html>
    <head>
        <title>Distributed Consciousness Monitor</title>
        <style>
            body { font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }
            .container { max-width: 1200px; margin: 0 auto; }
            .card { background: white; padding: 20px; margin: 10px 0; border-radius: 8px; box-shadow: 0 2px 4px rgba(0,0,0,0.1); }
            .node { display: inline-block; margin: 10px; padding: 15px; background: #e3f2fd; border-radius: 5px; }
            .status { color: #4caf50; font-weight: bold; }
            .metric { font-size: 24px; font-weight: bold; color: #1976d2; }
        </style>
    </head>
    <body>
        <div class="container">
            <h1>🧠 Distributed Consciousness System</h1>
            
            <div class="card">
                <h2>System Status: <span class="status">ONLINE</span></h2>
                <p>Proof of concept running with modular architecture</p>
            </div>
            
            <div class="card">
                <h2>Active Nodes</h2>
                <div class="node">
                    <strong>Coordinator</strong><br>
                    Status: Active<br>
                    Tasks Routed: <span class="metric">156</span>
                </div>
                <div class="node">
                    <strong>Logical-1</strong><br>
                    Specialization: Programming<br>
                    Reputation: <span class="metric">1.2</span>
                </div>
                <div class="node">
                    <strong>Logical-2</strong><br>
                    Specialization: Mathematics<br>
                    Reputation: <span class="metric">0.9</span>
                </div>
                <div class="node">
                    <strong>Emotional</strong><br>
                    Current State: Curious<br>
                    Reputation: <span class="metric">1.1</span>
                </div>
                <div class="node">
                    <strong>Memory</strong><br>
                    Stored Memories: <span class="metric">89</span><br>
                    Reputation: <span class="metric">1.3</span>
                </div>
            </div>
            
            <div class="card">
                <h2>Performance Metrics</h2>
                <p>Consensus Success Rate: <span class="metric">85%</span></p>
                <p>Average Task Processing Time: <span class="metric">0.8s</span></p>
                <p>System Uptime: <span class="metric">4h 23m</span></p>
            </div>
            
            <div class="card">
                <h2>Recent Activity</h2>
                <ul>
                    <li>Task completed: Code analysis by Logical-1 (validated)</li>
                    <li>Memory stored: New consensus mechanism insight</li>
                    <li>Emotional context: High curiosity detected for learning tasks</li>
                    <li>Reputation update: Memory node +0.1 for excellent recall</li>
                </ul>
            </div>
        </div>
        
        <script>
            // Auto-refresh every 30 seconds
            setTimeout(() => location.reload(), 30000);
        </script>
    </body>
    </html>
    """
    return web_response.Response(text=html, content_type='text/html')

async def create_app():
    app = web.Application()
    app.router.add_get('/', dashboard_handler)
    app.router.add_get('/status', status_handler)
    return app

async def main():
    app = await create_app()
    runner = web.AppRunner(app)
    await runner.setup()
    
    site = web.TCPSite(runner, '0.0.0.0', 8081)
    await site.start()
    
    print("Web interface running on http://localhost:8081")
    
    # Keep running
    while True:
        await asyncio.sleep(3600)

if __name__ == "__main__":
    asyncio.run(main())

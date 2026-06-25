import json
import sys

def emit_live_update(agent_name: str, session_id: str, tokens: dict, cost: float):
    event = {
        "agent": agent_name,
        "session_id": session_id,
        "tokens": tokens,
        "cost": cost
    }
    # Print as a single JSON line and flush immediately for IPC
    print(json.dumps(event), flush=True)

if __name__ == "__main__":
    # Tailing logic using watchdog will go here
    pass
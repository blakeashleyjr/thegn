import json

class ClaudeSession:
    def __init__(self, id, cost, input_tokens, output_tokens):
        self.id = id
        self.cost = cost
        self.input_tokens = input_tokens
        self.output_tokens = output_tokens

def parse_claude_output(json_str: str) -> ClaudeSession:
    data = json.loads(json_str)
    return ClaudeSession(
        id=data.get("session_id"),
        cost=data.get("total_cost_usd", 0.0),
        input_tokens=data.get("usage", {}).get("input_tokens", 0),
        output_tokens=data.get("usage", {}).get("output_tokens", 0)
    )

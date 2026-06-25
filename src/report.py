import sys

def generate_report(mock_adapters=False):
    # In a real run, this will import the adapters and fetch real data.
    # For now, it provides the skeleton.
    print("=== Unified Agent Metrics ===")
    
    if mock_adapters:
        print("Claude Code: $0.10 (500 tokens)")
        print("Pi Agent: $0.05 (250 tokens)")
        print("Total Cost: $0.15")
    else:
        # TODO: wire up actual adapters here
        pass

if __name__ == "__main__":
    generate_report()
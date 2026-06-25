def test_parse_pi_events():
    jsonl = '{"type":"session","id":"pi-123"}\n{"type":"turn_end","usage":{"input": 10, "output": 5}}'
    
    from src.adapters.pi import parse_pi_jsonl
    
    session_id, usage = parse_pi_jsonl(jsonl.splitlines())
    assert session_id == "pi-123"
    assert usage["input"] == 10
    assert usage["output"] == 5

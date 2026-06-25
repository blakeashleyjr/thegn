def test_parse_claude_json_output():
    raw_output = '{"type": "result", "session_id": "1234", "total_cost_usd": 0.05, "usage": {"input_tokens": 10, "output_tokens": 20}}'
    
    from src.adapters.claude_code import parse_claude_output
    
    session = parse_claude_output(raw_output)
    assert session.id == "1234"
    assert session.cost == 0.05
    assert session.input_tokens == 10
    assert session.output_tokens == 20

def test_unified_report(capsys):
    import src.report as report
    report.generate_report(mock_adapters=True)
    captured = capsys.readouterr()
    assert "Unified Agent Metrics" in captured.out
    assert "Total Cost: $" in captured.out
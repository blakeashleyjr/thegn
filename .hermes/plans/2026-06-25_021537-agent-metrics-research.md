# Coding Agents Metrics & API Integration Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Create a unified dashboard/integration layer capable of retrieving session titles, agent status, token usage, and costs directly from various autonomous coding agents (Claude Code, Codex, OpenCode, Hermes, Pi).

**Architecture:** We will build a set of robust parser/adapter modules in Python or Go that interface with the programmatic JSON outputs, APIs, and local databases of each agent CLI. These modules will normalize the data into a common schema: `Session(id, title, status, token_usage, cost, created_at, provider)`.

**Tech Stack:** Python, jq, SQLite, sub-process orchestration.

---

### Task 1: Claude Code Adapter

**Objective:** Extract session metrics, costs, and statuses from Claude Code's JSON outputs.

**Implementation Details:**

- Run commands using `claude -p '<prompt>' --output-format json`.
- Parse the resulting JSON to extract `session_id`, `num_turns`, `total_cost_usd`, and `usage` (`input_tokens`, `output_tokens`).
- Use `claude -p '<prompt>' --output-format stream-json --verbose --include-partial-messages` to read real-time streaming events for live status tracking.
- Create a reader for `claude auth status` to track readiness.

**Files:**

- Create: `src/adapters/claude_code.py`
- Test: `tests/adapters/test_claude_code.py`

**Step 1: Write failing test**

```python
def test_parse_claude_json_output():
    raw_output = '{"type": "result", "session_id": "1234", "total_cost_usd": 0.05, "usage": {"input_tokens": 10, "output_tokens": 20}}'
    session = parse_claude_output(raw_output)
    assert session.id == "1234"
    assert session.cost == 0.05
```

**Step 2: Run test to verify failure**
Run: `pytest tests/adapters/test_claude_code.py`
Expected: FAIL

**Step 3: Write minimal implementation**

```python
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
```

**Step 4: Run test to verify pass**
Run: `pytest tests/adapters/test_claude_code.py`
Expected: PASS

**Step 5: Commit**

```bash
git add src/adapters/claude_code.py tests/adapters/test_claude_code.py
git commit -m "feat: add claude code JSON adapter"
```

---

### Task 2: Codex Adapter

**Objective:** Parse Codex JSONL execution streams to track session IDs and tool execution.

**Implementation Details:**

- Use `codex exec '<prompt>' --json` to retrieve live JSONL events.
- Parse standard output line-by-line using `json.loads` to aggregate cost and tool call execution status.
- Monitor `~/.codex/sessions/` for durable session files and parse session metadata.

**Files:**

- Create: `src/adapters/codex.py`
- Test: `tests/adapters/test_codex.py`

**Step 1: Write failing test**

```python
def test_codex_jsonl_parser():
    jsonl = '{"type": "start", "session": "abc"}\n{"type": "end"}'
    events = list(parse_codex_stream(jsonl.splitlines()))
    assert len(events) == 2
    assert events[0]["session"] == "abc"
```

**Step 2: Run test to verify failure**
Run: `pytest tests/adapters/test_codex.py`
Expected: FAIL

**Step 3: Write minimal implementation**

```python
import json

def parse_codex_stream(lines):
    for line in lines:
        if line.strip():
            yield json.loads(line)
```

**Step 4: Run test to verify pass**
Run: `pytest tests/adapters/test_codex.py`
Expected: PASS

**Step 5: Commit**

```bash
git add src/adapters/codex.py tests/adapters/test_codex.py
git commit -m "feat: add codex JSONL adapter"
```

---

### Task 3: OpenCode Adapter

**Objective:** Fetch OpenCode session titles, stats, and live execution json.

**Implementation Details:**

- Invoke `opencode session list --format json` to get a structured list of sessions containing `{ id, title, updatedAt, model }`.
- Invoke `opencode stats --format json` to get overarching token usage and cost metrics.
- For active runs: use `opencode run '<prompt>' --format json`.

**Files:**

- Create: `src/adapters/opencode.py`
- Test: `tests/adapters/test_opencode.py`

**Step 1: Write failing test**

```python
def test_opencode_session_list():
    raw_json = '[{"id": "ses_123", "title": "Refactor auth", "model": "claude"}]'
    sessions = parse_opencode_sessions(raw_json)
    assert sessions[0]["title"] == "Refactor auth"
```

**Step 2: Run test to verify failure**
Run: `pytest tests/adapters/test_opencode.py`
Expected: FAIL

**Step 3: Write minimal implementation**

```python
import json

def parse_opencode_sessions(json_str: str):
    return json.loads(json_str)
```

**Step 4: Run test to verify pass**
Run: `pytest tests/adapters/test_opencode.py`
Expected: PASS

**Step 5: Commit**

```bash
git add src/adapters/opencode.py tests/adapters/test_opencode.py
git commit -m "feat: add opencode CLI adapter"
```

---

### Task 4: Hermes Agent Adapter

**Objective:** Read Hermes state directly from its SQLite FTS5 database to extract deep session metrics without subprocess polling.

**Implementation Details:**

- Connect directly to `~/.hermes/state.db` using Python's `sqlite3`.
- Query the `sessions` and `messages` tables to extract `session_id`, `title`, `when`, and message/token counts.
- Fallback/Alternative: Run `hermes sessions export <file>.jsonl` and parse the output.

**Files:**

- Create: `src/adapters/hermes.py`
- Test: `tests/adapters/test_hermes.py`

**Step 1: Write failing test**

```python
def test_hermes_sqlite_query(tmp_path):
    import sqlite3
    db = tmp_path / "state.db"
    conn = sqlite3.connect(db)
    conn.execute("CREATE TABLE sessions (session_id TEXT, title TEXT)")
    conn.execute("INSERT INTO sessions VALUES ('h_123', 'Build DB')")
    conn.commit()

    sessions = get_hermes_sessions(db)
    assert sessions[0]["title"] == "Build DB"
```

**Step 2: Run test to verify failure**
Run: `pytest tests/adapters/test_hermes.py`
Expected: FAIL

**Step 3: Write minimal implementation**

```python
import sqlite3

def get_hermes_sessions(db_path: str):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    cursor = conn.execute("SELECT * FROM sessions")
    return [dict(row) for row in cursor.fetchall()]
```

**Step 4: Run test to verify pass**
Run: `pytest tests/adapters/test_hermes.py`
Expected: PASS

**Step 5: Commit**

```bash
git add src/adapters/hermes.py tests/adapters/test_hermes.py
git commit -m "feat: add hermes SQLite adapter"
```

---

### Task 5: Pi Agent Adapter

**Objective:** Parse Pi's `.jsonl` session files and CLI output.

**Implementation Details:**

- Pi stores sessions globally grouped by workspace paths: `~/.pi/agent/sessions/--path--/*.jsonl`.
- The CLI command `pi -p '<prompt>' --mode json` outputs direct JSONL events on stdout.
- Parse the `usage` block from `assistantMessageEvent` to track `input_tokens`, `output_tokens`, and `cost`.

**Files:**

- Create: `src/adapters/pi.py`
- Test: `tests/adapters/test_pi.py`

**Step 1: Write failing test**

```python
def test_parse_pi_events():
    jsonl = '{"type":"session","id":"pi-123"}\n{"type":"turn_end","usage":{"input": 10, "output": 5}}'
    session_id, usage = parse_pi_jsonl(jsonl.splitlines())
    assert session_id == "pi-123"
    assert usage["input"] == 10
```

**Step 2: Run test to verify failure**
Run: `pytest tests/adapters/test_pi.py`
Expected: FAIL

**Step 3: Write minimal implementation**

```python
import json

def parse_pi_jsonl(lines):
    session_id = None
    usage = {"input": 0, "output": 0}
    for line in lines:
        if not line.strip(): continue
        data = json.loads(line)
        if data.get("type") == "session":
            session_id = data.get("id")
        elif data.get("type") == "turn_end":
            turn_usage = data.get("usage", {})
            usage["input"] += turn_usage.get("input", 0)
            usage["output"] += turn_usage.get("output", 0)
    return session_id, usage
```

**Step 4: Run test to verify pass**
Run: `pytest tests/adapters/test_pi.py`
Expected: PASS

**Step 5: Commit**

```bash
git add src/adapters/pi.py tests/adapters/test_pi.py
git commit -m "feat: add pi JSONL adapter"
```

---

### Risks and Tradeoffs

- **CLI Schema Changes:** The agents (claude, opencode, pi) might change their JSON structure across versions without warning. We should implement schema validation (e.g., Pydantic) to catch this early.
- **SQLite Locking:** Reading `~/.hermes/state.db` while Hermes is actively writing may hit locks. Use WAL mode considerations or fallback to `hermes sessions export`.
- **Live Streams:** Adapters parsing `stream-json` or JSONL stdout in real-time will require asynchronous non-blocking read pipelines.

### Verification Steps

Run `pytest tests/adapters/ -v` to ensure all parsers properly extract metric data from mocked JSON/JSONL/SQLite structures mirroring real agent outputs.

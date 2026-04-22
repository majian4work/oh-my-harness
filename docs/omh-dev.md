# omh-dev — Developer Tools

`omh-dev` is a standalone CLI for framework developers, providing session diagnostics, telemetry analysis, and automated regression evals.
It is fully decoupled from the user-facing `omh` binary and can be compiled and run independently.

## Installation / Running

```bash
# Run directly
cargo run -p omh-dev -- <command>

# Build in release mode (recommended — telemetry/eval can be CPU-intensive)
cargo run -r -p omh-dev -- <command>

# justfile shortcut
just omh-dev <command>
```

## Command Overview

| Command | Purpose |
|---------|---------|
| `diagnose <session_id>` | Analyze session dump files and detect model behavior anomalies |
| `telemetry [session_id]` | View turn-level telemetry stats; omit session_id to summarize recent sessions |
| `eval [path]` | Run TOML-based eval cases and produce a JSON report |

Global option: `--log <level>` (default: `info`).

---

## diagnose — Session Diagnostics

Reads request/response JSON from `.omh/sessions/<session>/dumps/`, analyzes each turn, and reports anomalies.

### Prerequisites

Enable dumps by setting `OMH_LOG=trace` when running omh. The runtime will write request/response/tool_results JSON to the dump directory after each turn.

### Usage

```bash
omh-dev diagnose ses_abc123
# or
just diagnose ses_abc123
```

### Example Output

```
═══ Agent: orchestrator ═══
  Turn 1: model=gpt-4.1 msgs=3 tool_calls=2
    text: Here is the analysis...
  Turn 2: model=gpt-4.1 msgs=5 tool_calls=1
    ⚠ DATA_IGNORED: Model received 4500 bytes of tool_result data across 2 results but concluded negatively
    ⚠ DUPLICATE_CALL: Model re-called read_file with identical args as a previous turn

✓ No anomalies detected.
  — or —
⚠ 2 anomaly(ies) detected.
```

### Detected Anomaly Types

| Tag | Description |
|-----|-------------|
| `DATA_IGNORED` | Model received a large amount of tool_result data but produced a negative conclusion |
| `DUPLICATE_CALL` | Model re-called the same tool with identical arguments across different turns |
| `TOKEN_OVERFLOW` | A sub-agent failed due to token limit exceeded |
| `TOOL_ERROR` | tool_results contain `is_error=true` with "failed" in the message |

---

## telemetry — Telemetry Statistics

Reads `.omh/sessions/<session>/telemetry.jsonl` and displays latency, token usage, tool call frequency, and other metrics.

### Inspect a Single Session

```bash
omh-dev telemetry ses_abc123
```

Output:

```
Session telemetry: ses_abc123
  records: 5
  completed: 4
  failed: 1
  avg latency: 2340 ms
  avg loop turns: 3.20
  avg tool calls: 2.40
  total tokens: 12000 in / 3500 out

Recent records:
  - agent=orchestrator model=gpt-4.1 provider=openai latency=1800ms turns=3 tools=2 completed=true error=-
  - agent=worker model=gpt-4.1 provider=openai latency=900ms turns=1 tools=1 completed=true error=-
```

### Summarize Recent Sessions

```bash
# Default: last 20 sessions
omh-dev telemetry

# Specify a limit
omh-dev telemetry --limit 50
```

Outputs a table with aggregate statistics at the bottom.

---

## eval — Automated Evaluation

Loads TOML test cases from the `tests/evals/` directory (or a specified path), runs each agent, judges PASS/FAIL based on constraints, and writes a JSON report.

### Prerequisites

At least one provider must be configured:

```bash
export OPENAI_API_KEY=sk-...
# or
export ANTHROPIC_API_KEY=sk-ant-...
# or configure credentials via: omh auth login
```

### Usage

```bash
# Run all tests/evals/*.toml
omh-dev eval

# Run a specific eval file
omh-dev eval tests/evals/smoke.toml
```

### TOML Case Format

```toml
[[cases]]
name = "orchestrator_short_hello"
agent = "orchestrator"                          # default: "orchestrator"
prompt = "Reply with one short sentence containing the word hello."
require_completed = true                        # default: true
contains_all = ["hello"]                        # response must contain all of these
contains_any = ["hello", "hi"]                  # response must contain at least one
not_contains = ["error"]                        # response must not contain any of these
min_tool_calls = 0                              # minimum tool call count
max_tool_calls = 0                              # maximum tool call count
max_tool_errors = 0                             # maximum allowed tool errors
disallow_error_categories = ["timeout", "provider"]  # forbidden error categories
```

#### Available Error Categories

| Value | Description |
|-------|-------------|
| `timeout` | Request timed out |
| `rate_limit` | Rate limit triggered |
| `permission` | Insufficient permissions |
| `tool_not_found` | Requested tool does not exist |
| `invalid_input` | Malformed input |
| `model_access` | Model unavailable |
| `context_window` | Context window exceeded |
| `provider` | Provider returned an error |
| `tool_execution` | Tool execution failed |
| `max_turns_reached` | Maximum turn count reached |
| `unknown` | Uncategorized error |

### Output

Each case result is printed to the terminal in real time:

```
[PASS] orchestrator_short_hello (orchestrator)
  session=ses_xxx completed=true tool_calls=0 tool_errors=0 turn_error=-
  response=Hello, this is a short sentence.

[FAIL] repo_model_alias_lookup (orchestrator)
  session=ses_yyy completed=true tool_calls=2 tool_errors=0 turn_error=-
  response=The model id is gpt-4.1
  failure: response missing required text: gpt-5.4

Eval summary: 4/5 passed
Report written to tests/evals/reports/eval_20260422_153012.json
```

### Report Structure (JSON)

```json
{
  "generated_at": 1745330000000,
  "suite_path": "tests/evals",
  "total": 5,
  "passed": 4,
  "failed": 1,
  "aggregates": {
    "by_agent": [
      { "agent": "orchestrator", "cases": 3, "passed": 2, "failed": 1, "total_tool_calls": 5, "total_tool_errors": 0 }
    ],
    "by_tool": [
      { "tool": "read_file", "calls": 3, "successes": 3, "failures": 0 }
    ],
    "by_error_category": [
      { "error_category": "timeout", "count": 1 }
    ]
  },
  "cases": [ ... ]
}
```

The `aggregates` section groups data by agent, tool, and error_category — useful for regression comparisons.

---

## Typical Workflows

### 1. Daily Development: Run Regression After Code Changes

```bash
# After modifying agent prompts or runtime logic
cargo test --workspace --quiet

# Run built-in smoke cases for end-to-end validation
omh-dev eval tests/evals/smoke.toml
```

### 2. Debug a Single Session

```bash
# Run omh with trace-level logging to produce dumps
OMH_LOG=trace cargo run -p omh -- cli "Analyze this project"

# Find the session id from the session list
cargo run -p omh -- sessions

# Diagnose that session
omh-dev diagnose ses_xxx

# View telemetry for that session
omh-dev telemetry ses_xxx
```

### 3. Global Telemetry Review

```bash
# Summarize telemetry from the last 50 sessions
omh-dev telemetry --limit 50
```

Watch for: sudden increases in avg latency, rising failure rates, or abnormal tool_errors for a specific agent.

### 4. Write New Eval Cases

```bash
# Create a new TOML file under tests/evals/
cat > tests/evals/my_feature.toml << 'EOF'
[[cases]]
name = "new_feature_basic"
agent = "orchestrator"
prompt = "Use the new_tool to process input 'test'."
contains_all = ["processed"]
min_tool_calls = 1
max_tool_errors = 0
disallow_error_categories = ["tool_not_found", "tool_execution"]
EOF

# Run it
omh-dev eval tests/evals/my_feature.toml
```

### 5. Cross-Version Regression Comparison

```bash
# Baseline: run eval on main branch
git checkout main
omh-dev eval
# Report written to tests/evals/reports/eval_20260422_100000.json

# Switch to feature branch and run again
git checkout feature/xxx
omh-dev eval
# Report written to tests/evals/reports/eval_20260422_100500.json

# Diff the aggregates section
diff <(jq .aggregates tests/evals/reports/eval_20260422_100000.json) \
     <(jq .aggregates tests/evals/reports/eval_20260422_100500.json)
```

---

## File Layout

```
tests/
└── evals/
    ├── smoke.toml              # Built-in smoke test cases
    └── reports/
        └── eval_YYYYMMDD_HHMMSS.json

.omh/
└── sessions/
    └── ses_xxx/
        ├── telemetry.jsonl      # Turn-level telemetry
        ├── tool_telemetry.jsonl # Tool-level telemetry
        └── dumps/               # Generated when OMH_LOG=trace
            └── orchestrator/
                ├── turn_001_request.json
                ├── turn_001_response.json
                └── turn_001_tool_results.json
```

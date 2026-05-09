---
marp: true
theme: default
paginate: true
size: 16:9
style: |
  section {
    font-family: 'Segoe UI', 'PingFang SC', 'Microsoft YaHei', sans-serif;
    background: linear-gradient(135deg, #0d1117 0%, #161b22 100%);
    color: #e6edf3;
  }
  h1 { color: #58a6ff; font-size: 2.2em; }
  h2 { color: #79c0ff; font-size: 1.6em; }
  h3 { color: #d2a8ff; }
  strong { color: #ffa657; }
  code { background: #30363d; color: #e6edf3; border-radius: 4px; padding: 2px 6px; }
  table { font-size: 0.78em; }
  th { background: #21262d; color: #58a6ff; }
  td { background: #161b22; }
  a { color: #58a6ff; }
  blockquote { border-left: 4px solid #58a6ff; color: #8b949e; }
  ul { font-size: 0.92em; }
  .columns { display: flex; gap: 2em; }
  .col { flex: 1; }
  marp-pre { background: #0d1117 !important; }
  pre { font-size: 0.52em; line-height: 1.3; }
  section.lead h1 { font-size: 2.8em; }
  section.lead h2 { font-size: 1.4em; }
---

<!-- _class: lead -->

# 🚀 Oh-My-Harness

## Multi-Agent Orchestration Framework

**Rust · Agent Orchestration · Multi-Provider · MCP/ACP/A2A**

> 100% AI Generated · Inspired by OpenCode, Codex, Copilot, Goose

---

# Overview

<div class="columns">
<div class="col">

### 🧠 omh — Agent Dispatch Engine

Multi-Agent Collaboration + Fully Extensible

- **7 Built-in Agents** with division of labor (orchestrator routes → worker / oracle / explore / librarian / planner / reviewer execute)
- **Markdown-as-Agent** — Zero-code custom agents (system prompt + routing + permissions)
- **Smart Routing** — avoid_when / triggers / depth-based delegation
- **Hot-pluggable Skills** — 4 activation modes (always / auto / semantic / manual)
- **Multi-Protocol** — MCP (tools) + ACP (agent comms) + A2A (distributed)
- **@file Mentions** — Gitignore-aware autocomplete file injection
- **Per-Session State** — Model/effort persisted in state.toml across restarts
- **Config Resolution** — Project `.omh/config.toml` > Global `~/.config/omh/config.toml`

</div>
<div class="col">

### 🔧 omt — Multi-Task Orchestrator

Task Decomposition → DAG Scheduling → Team Collaboration

- **LLM-driven** automatic task DAG planning
- **Git Worktree** branch-isolated parallel execution
- **A2A Distributed Teams** — multiple omh instances cooperate
- **TUI Dashboard** — team members panel + recent runs panel
- **Team Health** — auto-expire stale members (heartbeat timeout)
- **Resumable Runs** + exponential backoff retry
- **Token Budgeting** with fine-grained control

</div>
</div>

---

# System Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                     omh — Agent Dispatch Engine                   │
│                                                                  │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │                   Harness (Core Orchestration)             │   │
│  │  AgentRuntime: Turn Loop → LLM → Tool Calls → Aggregate   │   │
│  └───────────────────────────────────────────────────────────┘   │
│        ▲             ▲             ▲             ▲               │
│  ┌─────┴─────┐ ┌─────┴─────┐ ┌─────┴─────┐ ┌─────┴─────┐       │
│  │   Agent   │ │  Provider │ │   Tool    │ │   Skill   │       │
│  │  Registry │ │  Registry │ │  Registry │ │  Registry │       │
│  │  7+Custom │ │ OAI/Ant/CP│ │ Built+MCP │ │  4 Modes  │       │
│  └───────────┘ └───────────┘ └───────────┘ └───────────┘       │
│                                                                  │
│  ┌─── Cross-Cutting Concerns ───────────────────────────────┐   │
│  │ Hook │ Permission │ EventBus │ Memory │ Evolution│ Config│   │
│  │ Session (state.toml) │ Trace │ Telemetry               │   │
│  └───────────────────────────────────────────────────────────┘   │
├──────────────────────────────────────────────────────────────────┤
│                      Extension Protocols                          │
│   MCP (Tool Plugin)  │  ACP (Agent Interop)  │  A2A (Team)      │
├──────────────────────────────────────────────────────────────────┤
│                    omt — Task Orchestrator                        │
│    Planner → DAG Scheduler → Executor → Team Dispatcher          │
│    TUI Dashboard: Team Members │ Recent Runs │ Task DAG           │
└──────────────────────────────────────────────────────────────────┘
```

---

# Highlight ①: Agent Dispatch & Custom Extensions

<div class="columns">
<div class="col">

### Smart Routing & Delegation

```
User Input
    │
    ▼
Orchestrator (Primary)
    ├─ Semantic    ──→ Oracle (Deep Analysis)
    ├─ Code Task   ──→ Worker (Execute Changes)
    ├─ Docs Need   ──→ Librarian (MCP Lookup)
    ├─ Planning    ──→ Planner → Reviewer
    └─ Custom      ──→ User Agent (Zero-Code)
```

- **avoid_when** — Scenario-based routing exclusion
- **triggers** — Keyword trigger matching
- **Depth-based delegation** — Automatic delegation chain control
- **Permission isolation** — Per-agent Allow/Deny/Ask

</div>
<div class="col">

### Markdown-as-Agent

```markdown
---
name: security-auditor
description: Security audit specialist
config:
  model: claude-sonnet-4-0
  provider: anthropic
  permission_level: ReadOnly
permissions:
  allow: read_file, glob, grep
  deny: bash, write_file
avoid_when:
  - General coding / implementation tasks
triggers:
  security: Security-related analysis
---
You are a security audit specialist...
```

### Hot-Pluggable Skills

| Mode         | Trigger Condition          |
| ------------ | -------------------------- |
| **always**   | Always injected in context |
| **auto**     | File glob pattern match    |
| **semantic** | Semantic similarity match  |
| **manual**   | Explicit `/skill` invoke   |

</div>
</div>

---

# Highlight ②: omt Team Collaboration Architecture

```
                      ┌───────────────────────────┐
                      │    omt (Orchestrator)     │
                      │    A2A Server :8080       │
                      │                           │
                      │  ┌─────────────────────┐  │
        User Prompt──▶│  │    LLM Planner      │  │
                      │  │   Generate Task DAG  │  │
                      │  └──────────┬──────────┘  │
                      │             ▼             │
                      │  ┌─────────────────────┐  │
                      │  │    DAG Scheduler    │  │
                      │  │  Topo Sort + Conc.  │  │
                      │  └───┬──────┬──────┬───┘  │
                      └──────┼──────┼──────┼──────┘
             ┌───────────────┘      │      └───────────────┐
             ▼                      ▼                      ▼
  ┌───────────────────┐  ┌───────────────────┐  ┌───────────────────┐
  │  omh Agent #1     │  │  omh Agent #2     │  │  omh Agent #3     │
  │  (Local Worktree) │  │  (Remote A2A)     │  │  (Remote A2A)     │
  │                   │  │                   │  │                   │
  │  branch: task-1   │  │  branch: task-2   │  │  branch: task-3   │
  │  Agent: worker    │  │  Agent: worker    │  │  Agent: oracle    │
  │  ┌─────────────┐  │  │  ┌─────────────┐  │  │  ┌─────────────┐  │
  │  │  Turn Loop  │  │  │  │  Turn Loop  │  │  │  │  Turn Loop  │  │
  │  │  LLM→Tools  │  │  │  │  LLM→Tools  │  │  │  │  LLM→Tools  │  │
  │  └─────────────┘  │  │  └─────────────┘  │  │  └─────────────┘  │
  └───────────────────┘  └───────────────────┘  └───────────────────┘
        │ heartbeat            │ heartbeat            │ heartbeat
        └──────────────────────┼──────────────────────┘
                               ▼
                    omt State Aggregation & Retry
                    ├── Success → Trigger downstream deps
                    ├── Failure → Exponential backoff retry
                    └── Timeout → Reschedule task
```

**Key Points:** Git Worktree Isolation · Round-Robin LB · Token Budget · Resumable · Stale Member Expiry · TUI Dashboard

---

# Highlight ③: Multi-Protocol Pluggable Ecosystem

<div class="columns">
<div class="col">

### 🔌 MCP — Unlimited Tool Extension

```toml
# Custom stdio server
[mcp.my-server]
command = "npx"
args = ["-y", "my-mcp-server"]

# Custom HTTP server
[mcp.my-http-server]
url = "https://server.example.com/mcp"
headers = { Authorization = "Bearer ..." }
```

- Built-in **context7** (docs) + **exa** (web search)
- stdio + StreamableHTTP dual transport
- **Auto-discover** → Register as Tool → Agent callable
- Runtime enable/disable support

### 🤝 ACP — Agent Interop

- Structured message passing between agents
- Cross-process Server / Client bidirectional
- Register external agents as delegation targets

</div>
<div class="col">

### 🧬 Memory & Self-Evolution

**Memory System:**
- Markdown knowledge base (Project / Global scope)
- Fuzzy retrieval → auto-inject via confidence + reinforcement filter
- Context window 80% → automatic summarization

**Self-Evolution Engine:**
```
User Correction → Extract Rule → Confidence++
                                    ↓
              High-confidence → Inject into context
              Low-confidence  → Await more validation
```
- **Heuristic gating**: ≥5 tool calls + ≥6 messages before extraction
- **Periodic consolidation** every 50 turns (merge duplicates, prune low-confidence)
- **Injection filter**: confidence ≥ 0.6 AND reinforcement ≥ 2

### 🛡️ Permission & Hook Pipeline

```
Tool Call → PermissionGuard → Execute → AuditTrail
                │
                ├─ Allow → Pass through
                ├─ Deny  → Reject immediately
                └─ Ask   → Await user confirmation
```

- Per-agent permission isolation
- Extensible Hook middleware

</div>
</div>

---

# Highlight ④: Observability & Quality Assurance

<div class="columns">
<div class="col">

### 📊 Turn Telemetry

Auto-collected JSONL metrics per turn:

| Dimension   | Content                        |
| ----------- | ------------------------------ |
| **Latency** | End-to-end / Provider response |
| **Token**   | input / output / per-Agent agg |
| **Tools**   | Call count / error rate / time |
| **Depth**   | Turn Loop iteration count      |

### 🔧 omh-dev Developer Toolkit

| Command     | Function                                    |
| ----------- | ------------------------------------------- |
| `diagnose`  | Anomaly detection (DATA_IGNORED / DUP_CALL) |
| `telemetry` | Latency / Token / tool call stats           |
| `eval`      | TOML-based automated regression tests       |

</div>
<div class="col">

### 🧪 Eval Automation

```toml
[[cases]]
name = "orchestrator_hello"
agent = "orchestrator"
prompt = "Reply with hello"
contains_all = ["hello"]
max_tool_calls = 0
max_tool_errors = 0
disallow_error_categories = ["timeout"]
```

- End-to-end regression verification
- Grouped by Agent / Provider
- JSON report + Pass/Fail statistics

### 📐 Design Philosophy

- **🧩 20-Crate Modular** — Single responsibility, independent builds
- **🔌 Fully Pluggable** — Provider / Tool / Agent / Skill / Hook
- **📝 Markdown-as-Config** — Agent + Skill + Memory, zero code
- **🔄 Progressive Enhancement** — Local → Distributed seamlessly
- **🤖 100% AI Generated** — Self-bootstrapped + self-evolving
- **🛡️ Security First** — Permission isolation + Hook audit

</div>
</div>

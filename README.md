# oh-my-harness (omh)

A multi-agent orchestration framework written in Rust. Ships with an AI coding agent as the first use case.

> **Bootstrapped by** [OpenCode](https://github.com/nicholasgasior/opencode) + [Oh-My-OpenAgent](https://github.com/nicholasgasior/oh-my-openagent).
> Design ideas drawn from [OpenCode](https://github.com/nicholasgasior/opencode), [Oh-My-OpenAgent](https://github.com/nicholasgasior/oh-my-openagent), [Codex](https://github.com/openai/codex), [GitHub Copilot](https://github.com/features/copilot), [Goose](https://github.com/block/goose), [Claw-Code](https://github.com/casualjim/claw-code) and other notable projects.
> **All code is AI-generated.**

## Features

- **Multi-agent system** — Built-in agents (orchestrator, worker, oracle, explore, librarian, planner, reviewer) + user-defined agents via Markdown (YAML front matter + system prompt body)
- **User-defined skills** — `.omh/skills/*.md` with 4 activation modes (always / auto / semantic / manual)
- **Multi-provider LLM** — OpenAI-compatible, Anthropic, GitHub Copilot, custom endpoints
- **Protocol support** — MCP (Model Context Protocol) + ACP (Agent Communication Protocol)
- **Memory & self-evolution** — Markdown knowledge base, agent learns from corrections
- **Multi-frontend** — TUI (default), CLI one-shot
- **Provider auth management** — `omh auth login/logout/list/status`
- **Turn telemetry** — per-session JSONL metrics for latency, loop depth, tool usage, and token consumption

## Workspace layout

```
bins/omh/          # User-facing app package — TUI, CLI, shared app library
bins/omh-dev/      # Developer tooling package — diagnose, telemetry, eval
libs/
  message/         # Message, Role, ContentPart types
  trace/           # Logging (omh-trace)
  bus/             # EventBus (tokio broadcast)
  provider/        # LLM provider trait + OpenAI / Anthropic / Copilot
  tool/            # Tool trait, registry, builtins (bash, read, write, edit, glob, grep)
  session/         # JSONL session persistence
  permission/      # Permission rules & evaluation
  hook/            # Hook trait & pipeline
  agent/           # Agent definition, registry, 7 built-in agents
  memory/          # Markdown memory store
  evolution/       # Self-evolution engine
  mcp/             # MCP JSON-RPC client
  acp/             # ACP client + server trait
  runtime/         # Harness, AgentRuntime, BackgroundTaskManager
  skill/           # Skill definition, registry, SkillTool
```

## Quick start

```bash
# Provider auth
cargo run -p omh -- auth login openai --key sk-...
cargo run -p omh -- auth status

# Oneshot non-interactive run
cargo run -p omh -- run "explain this codebase"

# TUI (default)
cargo run -p omh
```

## Developer Tooling

Developer-only diagnostics and eval commands live in the separate `omh-dev` binary:

```bash
# Telemetry summary for recent sessions
cargo run -p omh-dev -- telemetry

# Telemetry details for one session
cargo run -p omh-dev -- telemetry ses_xxx

# Diagnose one session dump
cargo run -p omh-dev -- diagnose ses_xxx

# Run task evals from tests/evals/*.toml
cargo run -p omh-dev -- eval

# Run a specific eval file
cargo run -p omh-dev -- eval tests/evals/smoke.toml
```

Minimal eval file format:

```toml
[[cases]]
name = "basic greeting"
agent = "orchestrator"
prompt = "Reply with the word hello and nothing else"
contains_all = ["hello"]
not_contains = ["error"]
min_tool_calls = 0
max_tool_calls = 0
max_tool_errors = 0
disallow_error_categories = ["timeout", "provider"]
```

## Configuration

- **Project-level**: `.omh/` directory (agents, skills, rules, memory)
- **Global**: `~/.config/omh/` (agents, skills)
- **Environment**: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`

See **[docs/configuration.md](docs/configuration.md)** for the full configuration reference.

## Agent System

### Built-in Agents

| Agent            | Mode     | Cost      | Role                                                                                  |
| ---------------- | -------- | --------- | ------------------------------------------------------------------------------------- |
| **orchestrator** | primary  | expensive | Central coordinator — receives user input, decomposes tasks, dispatches to sub-agents |
| **worker**       | subagent | cheap     | Focused executor — carries out concrete code changes, never re-delegates              |
| **oracle**       | subagent | expensive | Read-only advisor — architecture guidance, debugging analysis, tradeoff evaluation    |
| **explore**      | subagent | free      | Codebase search — locates files, symbols, and patterns in the local repository        |
| **librarian**    | subagent | cheap     | External research — official docs, API references, web search                         |
| **planner**      | primary  | expensive | Plan-only — turns ambiguous requests into structured, verifiable plans                |
| **reviewer**     | subagent | expensive | Post-implementation QA — answers "can this ship?", defaults to approval               |

### Typical Flow

```
User input
  │
  ▼
orchestrator
  ├─ Code structure?  ──▶ explore   (background, parallel)
  ├─ External docs?   ──▶ librarian (background, parallel)
  ├─ Design decision? ──▶ oracle    (blocking — result drives implementation)
  ├─ Need a plan?     ──▶ planner 
  ├─ Implementation   ──▶ worker    (edits code, verifies, returns result)
  └─ QA               ──▶ reviewer  → [OKAY] or [REJECT]
```

### Design Principles

- **explore/librarian** are cheap grep — always fire in background, never block
- **oracle** is blocking — its advice determines the implementation direction
- **worker** never re-delegates — it receives a task and completes it directly
- **reviewer** biases toward approval — only rejects for concrete, reproducible blockers
- **planner** never writes code — output is always a structured plan

Custom agents can be added as Markdown files in `.omh/agents/` — see [Agent Definitions](docs/configuration.md#agent-definitions-markdown).

## License

MIT

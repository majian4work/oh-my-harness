# oh-my-harness (omh)

A multi-agent orchestration framework written in Rust. Ships with an AI coding agent as the first use case.

> **Bootstrapped by** [OpenCode](https://github.com/nicholasgasior/opencode) + [Oh-My-OpenAgent](https://github.com/nicholasgasior/oh-my-openagent).
> Design ideas drawn from [OpenCode](https://github.com/nicholasgasior/opencode), [Oh-My-OpenAgent](https://github.com/nicholasgasior/oh-my-openagent), [Codex](https://github.com/openai/codex), [GitHub Copilot](https://github.com/features/copilot), [Goose](https://github.com/block/goose), [Claw-Code](https://github.com/casualjim/claw-code) and other notable projects.
> **All code is AI-generated.**

## Features

- **Multi-agent system** — Built-in agents (orchestrator, worker, oracle, explore, librarian, planner, reviewer) + user-defined agents via Markdown
- **User-defined skills** — `.omh/skills/*.md` with 4 activation modes (always / auto / semantic / manual)
- **Multi-provider LLM** — OpenAI-compatible, Anthropic, GitHub Copilot, custom endpoints
- **Protocol support** — MCP (Model Context Protocol) + ACP (Agent Communication Protocol)
- **Memory & self-evolution** — SQLite FTS5 + Markdown knowledge base, agent learns from corrections
- **Crash recovery** — WAL-based mid-turn recovery + git snapshots
- **Multi-frontend** — TUI (default), CLI one-shot
- **Provider auth management** — `omh auth login/logout/list/status`, TUI popup (Ctrl+A)

## Workspace layout

```
bins/omh/          # Binary — TUI, CLI
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
  memory/          # SQLite FTS5 + Markdown memory store
  evolution/       # Self-evolution engine
  snapshot/        # WAL crash recovery + git snapshots
  mcp/             # MCP JSON-RPC client
  acp/             # ACP client + server trait
  runtime/         # Harness, AgentRuntime, BackgroundTaskManager
  skill/           # Skill definition, registry, SkillTool
```

## Quick start

```bash
# TUI (default)
cargo run -p omh

# CLI one-shot
cargo run -p omh -- cli "explain this codebase"

# Provider auth
cargo run -p omh -- auth login openai --key sk-...
cargo run -p omh -- auth status
```

## Configuration

- **Project-level**: `.omh/` directory (agents, skills, rules, sessions, memory)
- **Global**: `~/.config/omh/` (agents, skills, credentials)
- **Environment**: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`

See **[docs/configuration.md](docs/configuration.md)** for the full configuration reference.

## Agent System

### Built-in Agents

| Agent | Mode | Cost | Role |
|-------|------|------|------|
| **orchestrator** | primary | expensive | Central coordinator — receives user input, decomposes tasks, dispatches to sub-agents |
| **worker** | subagent | cheap | Focused executor — carries out concrete code changes, never re-delegates |
| **oracle** | subagent | expensive | Read-only advisor — architecture guidance, debugging analysis, tradeoff evaluation |
| **explore** | subagent | free | Codebase search — locates files, symbols, and patterns in the local repository |
| **librarian** | subagent | cheap | External research — official docs, API references, web search |
| **planner** | primary | expensive | Plan-only — turns ambiguous requests into structured, verifiable plans |
| **reviewer** | subagent | expensive | Post-implementation QA — answers "can this ship?", defaults to approval |

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

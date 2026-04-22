# Configuration Reference

omh uses a layered configuration system. Settings are read in order, with later sources overriding earlier ones:

1. **Built-in defaults** (compiled into the binary)
2. **Global config** `~/.config/omh/config.toml`
3. **Project config** `.omh/config.toml`

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `OPENAI_API_KEY` | OpenAI API key | — |
| `OPENAI_BASE_URL` | OpenAI-compatible endpoint | `https://api.openai.com` |
| `OPENAI_MODEL` | Default model for OpenAI provider | `gpt-4.1` |
| `ANTHROPIC_API_KEY` | Anthropic API key | — |
| `ANTHROPIC_MODEL` | Default model for Anthropic provider | `claude-sonnet-4-0` |
| `COPILOT_MODEL` | Default model for GitHub Copilot | `gpt-4.1` |
| `OMH_LOG` | Log level filter (e.g. `debug`, `trace`, `omh=debug,reqwest=warn`) | `info` |

> GitHub Copilot does not support environment variable auth. Use `omh auth login copilot` instead.

## config.toml

Both `~/.config/omh/config.toml` (global) and `.omh/config.toml` (project) use the same format.

### Agent Overrides

Override model/provider for any built-in or user-defined agent:

```toml
[agents.orchestrator]
model = "claude-sonnet-4-0"
provider = "anthropic"

[agents.worker]
model = "gpt-4.1"

[agents.oracle]
model = "claude-opus-4-0"
provider = "anthropic"
```

**Fields:**

| Field | Type | Description |
|-------|------|-------------|
| `model` | string | Model ID to use for this agent |
| `provider` | string | Provider ID (`openai`, `anthropic`, `copilot`) |

### MCP Servers

Configure external MCP (Model Context Protocol) servers. Built-in servers (`context7`, `exa`) are always available and can be overridden or disabled.

```toml
# Override built-in: add API key to context7
[mcp.context7]
headers = { CONTEXT7_API_KEY = "ctx7sk_..." }

# Disable built-in exa
[mcp.exa]
enabled = false

# Add custom stdio MCP server
[mcp.my-server]
command = "npx"
args = ["-y", "my-mcp-server"]
env = { API_KEY = "..." }

# Add custom HTTP MCP server
[mcp.my-http-server]
url = "https://my-server.example.com/mcp"
headers = { Authorization = "Bearer ..." }
```

**Fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | `""` | Command to start stdio MCP server |
| `args` | string[] | `[]` | Command arguments |
| `env` | map | `{}` | Environment variables for the process |
| `url` | string | — | URL for HTTP (StreamableHttp) transport. If set, `command`/`args`/`env` are ignored |
| `headers` | map | `{}` | HTTP headers (for HTTP transport) |
| `enabled` | bool | `true` | Set `false` to disable a built-in server |

**Built-in MCP servers:**

| Name | URL | Purpose |
|------|-----|---------|
| `context7` | `https://mcp.context7.com/mcp` | Library documentation lookup |
| `exa` | `https://mcp.exa.ai/mcp` | Web search |

## Agent Definitions (Markdown)

Custom agents are defined as Markdown files in `.omh/agents/` (project) or `~/.config/omh/agents/` (global).

```markdown
# my-agent

Short description of the agent.

## Config

- Mode: subagent
- Cost: cheap
- Model: gpt-4.1
- Provider: openai
- MaxTurns: 10
- Temperature: 0.7
- PermissionLevel: workspace_write

## Permissions

- Allow: read, glob, grep
- Deny: bash
- Ask: write, edit

## Use When

- Task requires specialized domain knowledge
- User explicitly requests this agent

## Avoid When

- Simple tasks that don't need specialization

## Triggers

- "analyze performance": Performance analysis requested

## System Prompt

You are a specialized agent that...
```

**Config keys:**

| Key | Values | Default | Description |
|-----|--------|---------|-------------|
| `Mode` | `primary`, `subagent` | `subagent` | Primary agents handle top-level input |
| `Cost` | `free`, `cheap`, `expensive` | `cheap` | Influences model selection when no model specified |
| `Model` | model ID string | — | Specific model to use |
| `Provider` | provider ID string | — | Specific provider to use |
| `MaxTurns` | integer | — | Maximum tool-call loops per turn |
| `Temperature` | float (0.0–2.0) | — | LLM sampling temperature |
| `PermissionLevel` | `read_only`, `workspace_write`, `full_access` | `read_only` | Default tool permission level |

**Permission keys:**

| Key | Description |
|-----|-------------|
| `Allow` | Comma-separated tool names always allowed |
| `Deny` | Comma-separated tool names always denied |
| `Ask` | Comma-separated tool names requiring user approval |

## Skill Definitions (Markdown)

Custom skills are defined as Markdown files in `.omh/skills/` (project) or `~/.config/omh/skills/` (global).

```markdown
---
name: my-skill
description: Does something useful
activation: auto
globs: ["*.rs", "*.toml"]
---

Instructions for the agent when this skill is active...
```

**Frontmatter fields:**

| Field | Values | Default | Description |
|-------|--------|---------|-------------|
| `name` | string | filename stem | Skill name |
| `description` | string | `""` | Short description |
| `activation` | `always`, `auto`, `semantic`, `manual` | `manual` | When this skill is injected into agent context |
| `globs` | string[] | `[]` | File patterns that trigger auto activation |

**Activation modes:**

| Mode | Behavior |
|------|----------|
| `always` | Always injected into every agent context |
| `auto` | Injected when matched file globs are detected |
| `semantic` | Injected when user input semantically matches description |
| `manual` | Only injected when explicitly requested |

## Provider Auth

Credentials are stored in `~/.config/omh/credentials.json` (managed via CLI):

```bash
omh auth login openai --key sk-...
omh auth login anthropic --key sk-ant-...
omh auth login copilot            # OAuth device flow
omh auth status                   # Show configured providers
omh auth list                     # List available models
```

## Directory Structure

```
~/.config/omh/                    # Global config
├── config.toml                   # Global agent overrides + MCP servers
├── credentials.json              # Provider API keys
├── agents/                       # Global custom agents
├── skills/                       # Global custom skills
└── models_cache.json             # Cached model lists

.omh/                             # Project config
├── config.toml                   # Project agent overrides + MCP servers
├── agents/                       # Project custom agents
├── skills/                       # Project custom skills
├── sessions/                     # Session JSONL files
└── memory/                       # Markdown knowledge base
```

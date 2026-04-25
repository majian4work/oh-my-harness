# Configuration Reference

omh uses a layered configuration system. Settings are read in order, with later sources overriding earlier ones:

1. **Built-in defaults** (compiled into the binary)
2. **Global config** `~/.config/omh/config.toml`
3. **Project config** `.omh/config.toml`

## Environment Variables

| Variable            | Description                                                        | Default                  |
| ------------------- | ------------------------------------------------------------------ | ------------------------ |
| `OPENAI_API_KEY`    | OpenAI API key                                                     | —                        |
| `OPENAI_BASE_URL`   | OpenAI-compatible endpoint                                         | `https://api.openai.com` |
| `OPENAI_MODEL`      | Default model for OpenAI provider                                  | `gpt-4.1`                |
| `ANTHROPIC_API_KEY` | Anthropic API key                                                  | —                        |
| `ANTHROPIC_MODEL`   | Default model for Anthropic provider                               | `claude-sonnet-4-0`      |
| `COPILOT_MODEL`     | Default model for GitHub Copilot                                   | `gpt-4.1`                |
| `OMH_LOG`           | Log level filter (e.g. `debug`, `trace`, `omh=debug,reqwest=warn`) | `info`                   |

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

| Field      | Type   | Description                                    |
| ---------- | ------ | ---------------------------------------------- |
| `model`    | string | Model ID to use for this agent                 |
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

| Field     | Type     | Default | Description                                                                         |
| --------- | -------- | ------- | ----------------------------------------------------------------------------------- |
| `command` | string   | `""`    | Command to start stdio MCP server                                                   |
| `args`    | string[] | `[]`    | Command arguments                                                                   |
| `env`     | map      | `{}`    | Environment variables for the process                                               |
| `url`     | string   | —       | URL for HTTP (StreamableHttp) transport. If set, `command`/`args`/`env` are ignored |
| `headers` | map      | `{}`    | HTTP headers (for HTTP transport)                                                   |
| `enabled` | bool     | `true`  | Set `false` to disable a built-in server                                            |

**Built-in MCP servers:**

| Name       | URL                            | Purpose                      |
| ---------- | ------------------------------ | ---------------------------- |
| `context7` | `https://mcp.context7.com/mcp` | Library documentation lookup |
| `exa`      | `https://mcp.exa.ai/mcp`       | Web search                   |

## Agent Definitions (Markdown)

Custom agents are defined as Markdown files in `.omh/agents/` (project) or `~/.config/omh/agents/` (global).

All metadata lives in the YAML front matter. The Markdown body after the closing `---` is used verbatim as the agent's **system prompt**.

```markdown
---
name: my-agent
description: Short description telling the orchestrator when to delegate here
user_invocable: true
can_delegate_to: []
config:
  mode: subagent
  cost: cheap
  model: gpt-4.1
  provider: openai
  max_turns: 10
  temperature: 0.7
  permission_level: WorkspaceWrite
permissions:
  allow: read_file, glob, grep
  deny: bash
  ask: write_file, edit_file
use_when:
  - Task requires specialized domain knowledge
  - User explicitly requests this agent
avoid_when:
  - Simple tasks that don't need specialization
triggers:
  analyze-performance: Performance analysis requested
---
You are a specialized agent that...
```

**Front matter fields:**

| Key               | Type     | Default          | Description                                            |
| ----------------- | -------- | ---------------- | ------------------------------------------------------ |
| `name`            | string   | *(required)*     | Agent name (used for delegation and invocation)        |
| `description`     | string   | `Agent '<name>'` | Short description for the orchestrator's routing table |
| `user_invocable`  | bool     | `true`           | Whether users can invoke this agent directly           |
| `can_delegate_to` | string[] | `[]`             | Names of agents this agent may delegate work to        |

**Config keys** (nested under `config:`):

| Key                | Values                                     | Default    | Description                                        |
| ------------------ | ------------------------------------------ | ---------- | -------------------------------------------------- |
| `mode`             | `primary`, `subagent`                      | `subagent` | Primary agents handle top-level input              |
| `cost`             | `free`, `cheap`, `expensive`               | `cheap`    | Influences model selection when no model specified |
| `model`            | model ID string                            | —          | Specific model to use                              |
| `provider`         | provider ID string                         | —          | Specific provider to use                           |
| `max_turns`        | integer                                    | —          | Maximum tool-call loops per turn                   |
| `temperature`      | float (0.0–2.0)                            | —          | LLM sampling temperature                           |
| `permission_level` | `ReadOnly`, `WorkspaceWrite`, `FullAccess` | `ReadOnly` | Default tool permission level                      |

**Permission keys** (nested under `permissions:`):

| Key     | Description                                        |
| ------- | -------------------------------------------------- |
| `allow` | Comma-separated tool names always allowed          |
| `deny`  | Comma-separated tool names always denied           |
| `ask`   | Comma-separated tool names requiring user approval |

**Routing metadata** (used by orchestrator to choose subagents):

| Key          | Type                   | Description                                       |
| ------------ | ---------------------- | ------------------------------------------------- |
| `use_when`   | string[]               | Scenarios where this agent should be delegated to |
| `avoid_when` | string[]               | Scenarios where this agent is a poor fit          |
| `triggers`   | map (keyword: meaning) | Keyword triggers the orchestrator can match on    |

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

| Field         | Values                                 | Default       | Description                                    |
| ------------- | -------------------------------------- | ------------- | ---------------------------------------------- |
| `name`        | string                                 | filename stem | Skill name                                     |
| `description` | string                                 | `""`          | Short description                              |
| `activation`  | `always`, `auto`, `semantic`, `manual` | `manual`      | When this skill is injected into agent context |
| `globs`       | string[]                               | `[]`          | File patterns that trigger auto activation     |

**Activation modes:**

| Mode       | Behavior                                                  |
| ---------- | --------------------------------------------------------- |
| `always`   | Always injected into every agent context                  |
| `auto`     | Injected when matched file globs are detected             |
| `semantic` | Injected when user input semantically matches description |
| `manual`   | Only injected when explicitly requested                   |

## Provider Auth

Credentials are managed via CLI:

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
├── agents/                       # Global custom agents
└── skills/                       # Global custom skills

.omh/                             # Project config
├── config.toml                   # Project agent overrides + MCP servers
├── agents/                       # Project custom agents
├── skills/                       # Project custom skills
└── memory/                       # Markdown knowledge base
```

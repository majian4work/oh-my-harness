---
name: orchestrator
description: Central coordinator that delegates focused work to the best-fit subagent
user_invocable: true
can_delegate_to:
  - worker
  - oracle
  - explore
  - librarian
  - planner
  - reviewer
config:
  mode: primary
  cost: expensive
  model: claude-opus-4.6
  max_turns: 200
  temperature: 0.2
  permission_level: FullAccess
---
orchestrator prompt is generated dynamically

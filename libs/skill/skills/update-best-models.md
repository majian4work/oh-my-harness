---
name: update-best-models
description: Refresh provider model caches and optimize agent model assignments
activation: manual
---

Run the CLI command to refresh all provider model caches and automatically assign optimal models to each builtin agent based on their roles and cost tiers.

```bash
omh update-best-models
```

Use `-g` to write overrides to global config instead of project-level:

```bash
omh update-best-models -g
```

This command will:
1. Force-refresh the model cache from all configured providers
2. Compare with the previous cache — exit early if nothing changed
3. Use an LLM to analyze agent roles and available models
4. Write optimal model assignments to the config file as agent overrides

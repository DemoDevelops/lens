# Lessons

- Mistake: treated ~/.claude/settings.json as the live config (cited rtk + context-mode hooks from it).
- Rule: active Claude config is $CLAUDE_CONFIG_DIR = ~/.claude-personal. RTK + Context Mode are REMOVED (rtk not on PATH, context-mode not enabled). ctxforge replaced them. ~/.claude/* is stale/inert — always verify against .claude-personal.

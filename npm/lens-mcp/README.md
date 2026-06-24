# lens-mcp

One-line installer for the [lens](https://github.com/DemoDevelops/lens) MCP server: a token-saving MCP tool provider (darkroom code execution, FTS5 search, tree-sitter code graph, reversible compression) that keeps raw data out of an AI agent's context window.

## Install

```sh
claude mcp add lens -- npx -y lens-mcp
```

Restart Claude Code and verify with the `lens_stats` tool. No Rust toolchain required: the prebuilt binary for your platform installs as an npm optional dependency, and nothing touches your hook config.

Supported platforms: macOS (arm64, x64), Linux (x64). For anything else, build from source: https://github.com/DemoDevelops/lens

## License

Elastic License 2.0 (ELv2).

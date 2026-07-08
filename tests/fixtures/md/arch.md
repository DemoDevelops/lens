# Architecture

System architecture and design decisions.

## Overview

The system is organized into modular components:

- **Parser**: Tree-sitter based extraction
- **Graph**: Node and edge representation
- **Skeleton**: Body elision for display
- **Tools**: MCP-driven queries

Each component is independently testable and reusable.

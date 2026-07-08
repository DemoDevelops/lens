<!-- 
Graph spec for T5 integration test:
Nodes:
  - "Index" (heading)
  - "Setup" (heading, contained by Index)
  - "Local" (heading, contained by Setup)
  - "Remote" (heading, contained by Index)
  - "Overview" (heading in arch.md, linked from index)

Containment edges (contains):
  - Index ⊃ Setup
  - Setup ⊃ Local
  - Index ⊃ Remote

Cross-document link edges (imports):
  - index.md → deploy.md (via [deploy](./deploy.md))
  - index.md → arch.md (via [[arch#Overview]])
-->

# Index

This is the main index document linking to other sections.

## Setup

Instructions for setting up the project locally and remotely.

### Local

Local setup steps:

1. Clone the repository
2. Install dependencies
3. Run the build

## Remote

Instructions for deploying to remote environments.

See [deploy](./deploy.md) for deployment details.

Also see [[arch#Overview]] for architecture overview.

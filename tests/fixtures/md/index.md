<!-- 
Full graph spec for markdown fixture corpus:

MODULES:
  - index.md (title: Index)
  - deploy.md
  - arch.md
  - guide.md (title: Implementation Guide, aliases: [impl-guide, how-to], tags: [guide, documentation, reference])
  - plain.md
  - notes.md (Research Notes)
  - sub/util.md (Utilities)
  - sub/index.md (Subdirectory Index)

HEADINGS (contained by their module):
  - Index (in index.md)
    - Setup
      - Local
    - Remote
  - Deployment (in deploy.md)
    - Installation
    - Verification
  - Architecture (in arch.md)
    - Overview
  - Implementation Guide (in guide.md)
    - Link Examples
    - Anchors
    - Setup
    - References
  - Plain CommonMark (in plain.md)
    - Content
    - Structure
    - Section
  - Research Notes (in notes.md)
    - Overview
    - Architecture
    - Observations
    - Transclusion
  - Utilities (in sub/util.md)
    - String Utilities
    - Path Utilities
  - Subdirectory Index (in sub/index.md)
    - Contents
    - Note

EDGES (imports, cross-document links):
  - index.md → deploy.md (via inline [deploy](./deploy.md))
  - index.md → arch.md (via wikilink [[arch#Overview]])
  - deploy.md → index.md (via inline [home](./index.md))
  - guide.md → deploy.md (via reference [deploy-ref])
  - guide.md → arch.md (via reference [arch-ref] and inline [Architecture Overview](./arch.md#overview))
  - plain.md → arch.md (via inline [architecture](./arch.md))
  - notes.md → deploy.md (via wikilink [[deploy]])
  - notes.md → arch.md (via wikilink [[arch]])
  - sub/index.md → sub/util.md (via inline [Utilities](./util.md))
  - sub/index.md → guide.md (via inline [Guide](../guide.md))

ANCHORS (heading-level imports):
  - index.md#Setup (contains Local)
  - arch.md#Overview (linked from index.md via [[arch#Overview]])
  - guide.md#Anchors, guide.md#Setup
  - arch.md#overview (canonical slug, linked from guide.md via [Architecture Overview](./arch.md#overview))

TAGS (from frontmatter):
  - guide tag node (tagged edge: guide.md → guide)
  - documentation tag node (tagged edge: guide.md → documentation)
  - reference tag node (tagged edge: guide.md → reference)

EMBEDS (transclusion):
  - notes.md embeds deploy.md (via ![[deploy]])

TAGS (inline #hashtag in prose, PKM-gated):
  - notes.md → alpha tag (inline #alpha)
  - notes.md → beta tag (inline #beta)
  - notes.md → documentation tag (inline #documentation)

NO-REGRESSION GATE (plain.md must yield no PKM edges):
  - plain.md: zero [[...]] wikilinks, zero ![[ ]] embeds, zero inline #tag hashtags (heading # markers OK)
  - Result: plain.md contributes zero tagged/embeds/wikilink edges; only regular inline links
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

# lens vs the token-savings landscape
### A competitive analysis for the distribution decision

## Bottom line up front

lens's core bet, keep raw data out of the context window and compute over it
instead, is no longer a hypothesis. In the last 12 months Anthropic (98.7% token
reduction, their own engineering blog), Cloudflare (81%, production data), and a
peer-reviewed ICML paper (CodeAct, +20% task success) all independently converged on
exactly this pattern. That is the strongest possible signal that the approach is
correct, and it is the headline to lead with for a technical CTO.

The flip side is the same fact: because the approach is now industry consensus,
Anthropic is shipping pieces of it natively (memory tool, context editing,
"programmatic tool calling"). lens's durable advantage is not the idea, it is the
implementation: local-first, zero-infrastructure, agent-controlled, multi-language,
plus two things nobody else does (transparent output offloading and hook-driven
session-state recovery).

Recommendation: pilot it, do not mass-distribute yet. The engineering is sound and
well-differentiated, but the adoption-steering layer and the benchmark rigor need one
more hardening pass before 100+ engineers depend on it. Details at the end.

## Update (2026-06-22): composition items shipped and benchmarked

The four "borrow / compose" items below were implemented and measured deterministically
(`cargo run --bin bench_changes`, no model, all four gates pass):

- TOON compaction: 48-52% smaller on uniform JSON arrays, lossless round-trip, flat across scale.
- lens_find: natural-language to symbol, hit@1 5/5 on the fixture (lexical ranking, no embeddings).
- lens_symbol proximity weighting: an in-focus file's match moves from rank 5 to rank 1 when recently touched.
- recovery conflict resolution: 74% fewer events into the recovery snapshot; a contradicted (edited-then-deleted) path resolves to its latest state.

What this changes for the decision, and what it does not: it confirms the value ceiling
of these mechanisms on controlled fixtures and partially closes the semantic-query gap
(threat 3 below). It does not change the central recommendation. These are capability and
correctness wins; whether the model actually invokes lens_find / lens_search in real
sessions (adoption) is still unmeasured. Pilot and measure adoption remains the gate.

## The map: four schools of token reduction

Everyone in this space is solving the same problem (tokens are expensive and degrade
reasoning) but they attack it at four different points in the pipeline:

```
                          THE TOKEN-REDUCTION LANDSCAPE

  data -> [ what reaches the context window ] -> model

  +---------------------------------------------------------------------+
  | SCHOOL 1: PACK-IT-ALL          front-load the whole repo, once       |
  |   repomix . code2prompt . gitingest . files-to-prompt               |
  |   mechanism: concatenate everything into one prompt-friendly blob    |
  |   wins: one-shot "understand this repo"  loses: long sessions, cost  |
  +---------------------------------------------------------------------+
  | SCHOOL 2: RANK-AND-MAP         send a smart subset, passively        |
  |   Aider repo-map . Sourcegraph Cody . Continue.dev . Tabby . Roo     |
  |   mechanism: tree-sitter + PageRank or embeddings pick what matters  |
  |   wins: zero agent effort  loses: pays tokens every turn, fuzzy      |
  +---------------------------------------------------------------------+
  | SCHOOL 3: COMPRESS-THE-BYTES   same info, fewer tokens               |
  |   LLMLingua (lossy, model-in-loop) . TOON (lossless) . RTK           |
  |   mechanism: drop low-information tokens, or re-encode structure     |
  |   wins: raw ratio  loses: lossy variants break parsers              |
  +---------------------------------------------------------------------+
  | SCHOOL 4: KEEP-IT-OUT          never let raw data enter context      |
  |   Anthropic code-exec-with-MCP . Cloudflare Code Mode . CodeAct      |
  |   mechanism: agent writes code, darkroom runs it, only answer returns |
  |   wins: biggest savings + correctness  needs: a darkroom             |
  +---------------------------------------------------------------------+

  ORTHOGONAL AXIS - PERSIST-ACROSS-RESETS (memory):
    MemGPT/Letta . Mem0 . Zep . Cognee . Claude memory tool . Cline memory-bank
    mechanism: write state to an external store so it survives compaction
```

## Where lens sits

lens is unusual: it is not a point solution in one school, it is a portfolio
spanning three of them plus the memory axis, delivered as one local MCP server with
hook-driven steering.

| School | lens feature | Posture |
|---|---|---|
| 4 - Keep-it-out | `lens_run` darkroom + large-output offload (`retrieve_ref`) | core thesis, strongest |
| 2 - Rank-and-map | `lens_symbol/neighbors/path` over a tree-sitter symbol graph | on-demand (not passive) |
| 3 - Compress | lossless JSON compaction (~61%) + bounded command wrap | structural, no model in loop |
| Memory | hook-driven session-state recovery across compaction | unique mechanism |

That breadth is the story. No single competitor covers all four. But it also means
lens is compared against the best-in-class tool in each lane, so the head-to-head
below does exactly that.

## Head-to-head by capability

| lens capability | Nearest/best OSS comparable | Verdict | The honest caveat |
|---|---|---|---|
| Darkroom: compute over data | Anthropic code-exec-with-MCP; Cloudflare Code Mode; CodeAct/smolagents | Tie on thesis, win on packaging. lens ships it as a callable multi-language MCP tool, local, zero-setup. Cloudflare is TS-only and needs Workers; smolagents is Python-only; Anthropic's post is a blueprint, not a product. | The 98.7% number is Anthropic's, for one specific workflow. Do not generalize it. lens's own measured darkroom win is 93% and flat across scale, solid and defensible. |
| Code graph | Aider repo-map (tree-sitter + PageRank); a near-identical academic tool, Codebase-Memory (arXiv 2603.27277, Mar 2026): MCP + tree-sitter graph, 83% answer quality at 10x fewer tokens | Win on precision/cost, gap on ranking + semantics. Aider pays ~1k tokens of map every turn; lens costs zero until the agent queries. lens uniquely exposes `lens_path` (how A reaches B). | Aider's PageRank auto-ranks relevance with no agent effort; lens needs the agent to choose to query (the adoption risk, below). Embedding tools (Cody, Continue, Roo) win on "find the auth logic" semantic queries lens cannot do. A published paper now does almost exactly the graph layer: novelty is gone, execution is the moat. |
| JSON/structured compaction | TOON (24k stars, lossless, 30-60% on uniform arrays) | Tie, and TOON validates it. Both lossless, structural, no model in loop. lens's measured 56-61% lands exactly in TOON's published band. | TOON is a cleaner standalone format with momentum. Easiest borrow on the board (below). |
| Output compression generally | LLMLingua (up to 20x) | Win on correctness; they win on raw ratio. LLMLingua is lossy and puts a second model in the request loop; its output is mangled text no parser can read. lens stays lossless and deterministic. | Rebuttal to "20x vs your 1.7x": theirs is lossy NL-prose compression with GPU + latency cost; yours is lossless structured output. Different jobs. |
| Session recovery | Claude native memory tool + context editing (84% reduction, in-product); Mem0/Zep/MemGPT (cross-session memory) | Win on cost + automation for the in-session case. Every competitor except Cline's memory-bank makes LLM calls at write time (Mem0 ~$0.30-0.80/conversation). lens writes structured records via hooks with zero LLM calls, invisibly, exactly when an agent under context pressure would skip housekeeping. | Mem0/Zep/Cognee solve a different, bigger problem (cross-session/user memory over months). lens solves intra-session survival across one compaction. Do not position against Zep's LOCOMO scores, that is not the race. The 25x claim is internal/unverified. |
| Stop web/build flood (redirect/wrap) | RTK (CLI output compression); MCP `resource_link` spec | Win, most aggressive. lens intercepts any large output at the hook layer before it enters context. MCP's `resource_link` needs server-side opt-in; lens needs no cooperation. | Genuinely novel and underexploited in the market. |

## The validation (lead with this)

For a technical CTO, the most persuasive point is that lens's architecture is what
the frontier labs independently arrived at:

- Anthropic, "Code execution with MCP" (Nov 2025): 150k -> 2k tokens, "98.7%" on a
  multi-step workflow. Literally lens's thesis, from the model vendor.
- Cloudflare Code Mode: "converting an MCP server into a TypeScript API can cut token
  usage by 81%"; entire Cloudflare API in ~1,000 tokens.
- CodeAct (ICML 2024, peer-reviewed): code-actions beat JSON tool-calls by up to +20%
  success, -30% actions, across 17 models.
- Codebase-Memory (arXiv, Mar 2026): tree-sitter graph over MCP, 83% quality at 10x
  fewer tokens. External proof the graph layer's design is sound.

Three independent teams + the model vendor + peer review converging in a year is the
strongest competitive endorsement available. lens bet on the right horse early.

## The threats (be honest with the CTO)

1. Anthropic is eating School 4 and the memory axis natively. Memory tool + context
   editing already ship on the Claude API and Claude Code uses them. A skeptical CTO
   will ask "why install a third-party server for what the vendor gives me free?" The
   answer: native context-editing is prose-summary lossy and clears tool results;
   lens keeps typed, lossless session state and adds a darkroom, code graph, and
   offloading the native features do not. True today, but the gap narrows every
   release. This is a roadmap risk, not a correctness risk.
2. The graph layer now has a published academic twin. Codebase-Memory does nearly the
   same thing. Differentiation is execution (Rust, integrated, `lens_path`), not
   concept.
3. Embedding-RAG tools (Cody, Continue, Roo) still beat lens on true semantic
   queries. `lens_find` (shipped 2026-06-22) now maps natural language to a symbol
   lexically (hit@1 5/5 on the fixture), which narrows this for keyword-overlapping
   phrasings, but it is lexical, not embeddings: "where is the rate-limiting logic"
   with no shared term is still their win.

## What we borrowed / composed (shipped + measured 2026-06-22)

All four are now implemented (numbers in the Update near the top); the rationale for each:

- TOON as a compaction strategy (cheap win). Emit TOON for the uniform-array JSON case.
  Lossless, no model, externally benchmarked. Direct drop-in to the existing compactor.
- Aider's personalization weighting for the graph. Aider boosts symbols in
  currently-open files (x50 on edges from chat files). Weighting `lens_symbol` results
  by files touched this session would auto-focus output and partly close the "agent has
  to know what to ask" gap.
- A semantic entry point in front of the graph (medium effort, high value). Natural
  language -> candidate symbol (embedding or even a cheap grep+rank) -> exact graph
  traversal. The one capability the RAG tools have and lens does not, and it
  composes naturally with what is already built.
- Conflict resolution on session records (from Mem0g / Zep). When a record says a file
  was both added and deleted, there is no resolver. Timestamp-validity (Zep) or an
  update-resolver (Mem0g) prevents stale state contaminating recovery. Also addresses
  the known graph-node bouncing / shrink-guard concern.

## Anti-patterns lens correctly avoids

- Pack-it-all front-loading (repomix/gitingest): wastes tokens on irrelevant files
  every turn in a long session. lens's fetch-on-demand is the right call for the
  agentic dev loop. Worth saying explicitly, because it is the most common naive
  approach engineers reach for.
- Lossy compression in the request path (LLMLingua): breaks any downstream parser and
  adds a model + GPU + latency. lens's lossless stance is a correctness guarantee
  worth defending.
- Mandatory external infra (Cody -> Sourcegraph; Continue -> LanceDB; Roo -> Qdrant;
  Mem0 -> pgvector+Neo4j). lens needs none. For a 100-engineer rollout this is a
  major operational advantage: no service to stand up, secure, or scale.
- Memory writes that cost LLM calls (Mem0/Cognee/MemGPT). lens's hook writes are
  free and automatic.

## Distribution-readiness assessment (the actual decision)

What is genuinely strong for a fleet rollout:

- Zero infrastructure. Single local MCP server. No vector DB, no cloud account, no keys
  to rotate across 100 machines. The biggest operational win versus every School-2 and
  memory competitor.
- Architecturally validated by Anthropic/Cloudflare/peer review.
- Lossless and correctness-preserving where competitors trade accuracy for ratio.

What to fix or prove before mass distribution:

1. The search-adoption gap. lens's own data shows `lens_search` fires 0/3 (correct
   on small repos), but the scale-aware nudge meant to fix it is verified to fire while
   its lift is unmeasured. Across 100 engineers, a feature that does not get invoked
   delivers zero value regardless of its ceiling. Measure the lift first.
2. Benchmark rigor for external eyes. The numbers are internal and small-N (N=2-6), and
   the graph byte-savings have a known O(N^2) fixture artifact (correctly excluded). A
   technical CTO will probe this. Either run a published-methodology benchmark (an
   `ANTHROPIC_API_KEY` repro the way Codebase-Memory did on 31 repos) or lead with the
   externally corroborated numbers (the 61% compaction sits in TOON's published band;
   the darkroom thesis matches Anthropic's 98.7% direction).
3. Operational lifecycle at scale. Long-lived per-repo server, graph index staleness,
   and the "server runs the old binary until session restart" behavior are fine for one
   developer but need a documented update/restart story for a fleet.
4. The "why not native" answer, written down. Engineers will ask why this over
   Anthropic's built-ins. The answer is good; make it a one-pager.

Recommendation: run a structured pilot (10-20 engineers, 2-4 weeks) with two metrics
that map directly to the two planes already built: adoption (do the tools actually fire
in real work) and measured token/cost delta (instrument real sessions, not fixtures).
If adoption holds outside the benchmark harness and the cost delta is real on live
repos, the zero-infra + lossless + validated-architecture story makes fleet
distribution an easy yes. The technology is right; the open question is behavioral
adoption at scale, which is exactly the gap the benchmarks already flagged.

## Sources

Code-map / repo intelligence:
- Aider repo map: https://aider.chat/docs/repomap.html , https://aider.chat/2023/10/22/repomap.html
- Sourcegraph Cody context: https://sourcegraph.com/docs/cody/core-concepts/context
- Continue.dev @Codebase: https://docs.continue.dev/customize/context/codebase
- Roo Code codebase indexing: https://docs.roocode.com/
- Tabby repository context: https://www.tabbyml.com/blog/repository-context-for-code-completion
- Codebase-Memory (tree-sitter KG over MCP): https://arxiv.org/html/2603.27277v1

Code execution / keep-it-out:
- Anthropic, Code execution with MCP: https://www.anthropic.com/engineering/code-execution-with-mcp
- Cloudflare Code Mode: https://blog.cloudflare.com/code-mode/ , https://blog.cloudflare.com/code-mode-mcp/
- CodeAct (ICML 2024): https://arxiv.org/abs/2402.01030
- smolagents: https://huggingface.co/blog/smolagents

Compression / packers:
- LLMLingua: https://github.com/microsoft/LLMLingua ; LLMLingua-2: https://arxiv.org/abs/2403.12968
- LongLLMLingua: https://arxiv.org/abs/2310.06839
- TOON: https://github.com/toon-format/toon ; independent benchmark: https://arxiv.org/abs/2603.03306
- repomix: https://github.com/yamadashy/repomix ; code2prompt: https://github.com/mufeedvh/code2prompt
- gitingest: https://github.com/coderamp-labs/gitingest ; files-to-prompt: https://github.com/simonw/files-to-prompt

Agent memory / persistence:
- MemGPT/Letta: https://www.letta.com ; Mem0: https://arxiv.org/abs/2504.19413
- Zep: https://arxiv.org/abs/2501.13956 ; Cognee: https://www.cognee.ai
- Anthropic context management: https://www.anthropic.com/news/context-management
- Cline memory bank: https://docs.cline.bot/best-practices/memory-bank

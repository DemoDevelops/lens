# Supported languages

lens **full-text-searches every UTF-8 file** regardless of language (`lens_index` /
`lens_search` apply no extension filter). The table below is about the **code
graph** (`lens_map` / `lens_symbol` / `lens_links` / `lens_path`) and structural
search (`lens_grep_ast`): which languages get defs / calls / imports nodes.

## Graph languages (16)

| Language | Extensions | Mechanism | Notes |
|---|---|---|---|
| Rust | `rs` | hand-written | full defs/calls/imports |
| Python | `py` | hand-written | full |
| JavaScript | `js` `jsx` `mjs` `cjs` | hand-written | full |
| TypeScript | `ts` `tsx` | hand-written | full |
| Go | `go` | hand-written | full |
| Swift | `swift` | hand-written | full |
| C | `c` | tags adapter | defs only (tags.scm has no call captures) |
| C++ | `cpp` `cc` `cxx` `hpp` `hh` `h` | tags adapter | defs only |
| C# | `cs` | tags adapter | calls via `@reference.send` (member access) |
| Java | `java` | tags adapter | defs + calls |
| Ruby | `rb` | tags adapter | defs + calls |
| PHP | `php` | tags adapter | calls: member/scoped/qualified (not bare) |
| Lua | `lua` | tags adapter | defs + calls |
| Kotlin | `kt` `kts` | vendored tags.scm | crate ships tags.scm but doesn't const-export it |
| Scala | `scala` `sc` | vendored tags.scm | same |
| Bash | `sh` `bash` | hand-authored query | every command is a ref; only defined functions resolve to edges |

"tags adapter" = the grammar's own `TAGS_QUERY` consumed verbatim by
`src/discovery/tags_adapter.rs`. "vendored" = the grammar ships `queries/tags.scm`
but doesn't const-export it, so lens `include_str!`s a committed copy from
`src/discovery/vendored/`. "hand-authored" = no tags.scm exists, so lens writes a
small tags-shaped def/call query.

The tags adapter is intentionally coarser than a hand-written spec (it collapses
kinds, e.g. Rust `struct`/`enum` -> `class`, and carries no imports). `bench_calibration`
freezes the measured adapter-vs-oracle delta on Rust and Python; `bench_acceptance`
gates parse-rate + coverage on SHA-pinned real repos.

## Search-only (no graph; already searchable)

These have no graph today; they are still indexed and searchable.

- **Real languages, no usable grammar / query yet:** Objective-C, Groovy, Perl
  (crates exist but ship no tags.scm; hand-query deferred).
- **Config:** Makefile (flagship file is extensionless, which the extension-based
  dispatch can't reach), HCL/Terraform (block/label mapping is awkward), CSS
  (no real symbol defs), PL/pgSQL, Dockerfile (marginal; crate's TAGS_QUERY is a
  dead cfg).
- **No maintained Rust grammar:** Brightscript, BrighterScript.
- **Containers (T9, deferred):** Vue (`<script>` -> TypeScript) and Jupyter
  `.ipynb` (code cells -> Python) need a preprocessing layer with a line-offset
  map; not yet built.
- **Markup / templates (no symbols to graph):** HTML, EJS, Haml, Jinja, Smarty,
  Go Template, API Blueprint, DataWeave, VCL.

See `dev/language-support-audit.md` (local) for the full per-crate verdict table.

## tree-sitter 0.25

lens runtime tree-sitter is **0.25** (was 0.24). The modern grammar ecosystem
ships **ABI-15** parsers (regenerated with the ts 0.25 CLI); a 0.24 runtime caps at
ABI 14 and rejects them with "Incompatible language version 15". 0.25 supports ABI
13-15, so the new ABI-15 grammars AND the 6 existing ABI-14 grammars all work.

## Adding a language

- **Has a `TAGS_QUERY` const + an ABI <=15 grammar:** add the Cargo dep and one
  `TagsLangSpec` entry to `tags_registry()` in `src/discovery/tags_adapter.rs`, plus
  a fixture test. No query authoring.
- **Ships tags.scm but no const:** vendor the file into `src/discovery/vendored/`
  and `include_str!` it.
- **No tags.scm:** hand-author a small tags-shaped query (`@definition.<kind>` +
  `@name`, `@reference.call`/`@reference.send`) inline in the registry entry.
- **Hand-written precision (rare):** add a `LangSpec` to `all_specs()` in
  `extract.rs` plus its `cached_queries` static + arm and a `fn_scope_kinds` arm.
  Reserved for languages whose coarse tags output is not good enough.

Adding ~25 grammar crates trips `bench_fitness`'s strict `dep_count` gate (G2) by
design; a human refreezes it with `cargo run --bin bench_fitness -- --update` after
confirming savings (G1) and the MCP tool surface (G3) are unchanged.

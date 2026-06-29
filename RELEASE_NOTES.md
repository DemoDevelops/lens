Better `lens_find` ranking and a public-ready README.

### Improved
- `lens_find` blends lexical relevance with a PageRank tie-break, so the most structurally important symbol wins ties. Backed by new findloc accuracy fixtures.

### Docs
- README rewrite: a 10-second pitch with a concrete before/after, an honest benchmarks-and-limits note, a "How it compares" section, and a security note on the darkroom (process isolation and a timeout, not an OS sandbox).

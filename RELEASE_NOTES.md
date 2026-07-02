Faster, multi-threaded index builds via a new full-text search backend.

### Improved
- The content index is now built by a new Tantivy-based full-text search backend instead of SQLite FTS5. Its multi-threaded segment build makes the cold index build near-linear in repository size, up to ~10x faster on large repositories (measured from 77s to 7s at ~161k chunks); the old build grew super-linearly and could take many minutes on a large tree.

### Changed
- The backend swap is automatic. On the first run after upgrading, an existing index rebuilds itself once, the old SQLite FTS5 tables are dropped, and their on-disk space is reclaimed; no reindex command or manual step is needed.
- Search results and ranking are unchanged: symbol-weighted BM25, substring/structural matching, and the same result ordering carry over to the new backend.

Portable search index, tidier data dir, and a more reliable rebuild gate.

### Improved
- The FTS index now stores chunk and manifest paths repo-root-relative instead of absolute, so the index is portable across checkouts and no longer leaks machine layout into search output. Indexes built with absolute paths are migrated automatically on first open.
- On a clean shutdown the SQLite databases are checkpointed and their `-wal`/`-shm` sidecars removed, leaving the data dir tidy.

### Fixed
- The index rebuild now gates on the live chunk count rather than a cached stat, so a schema or path migration that wipes the index can no longer wrongly skip the rebuild.

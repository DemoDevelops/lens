A refreshed savings dashboard and a fix for test runs polluting the savings ledger.

### Fixed
- Test and benchmark runs no longer leak fixture operations into the real `~/.lens/ops.log` ledger. Integration tests previously mirrored their fixture ops into the machine-global ledger (a single concurrency test added tens of millions of bogus "saved" tokens), inflating the dashboard's lifetime totals. Cargo-launched processes now opt out of the global mirror; the installed server runs the compiled binary directly and still records normally.

### Improved
- The web dashboard's header controls are now consistent, self-describing dropdowns. The view (mini/full) and theme (dark/70s) toggles became dropdowns that show the current selection, matching the repo, time-range, and model pickers.
- Added a repo picker: scope the dashboard to a single project or view all repos combined ("global").
- Added a custom time-range picker: alongside the presets, pick an arbitrary start/end window; savings, activity, and charts all honor it.
- The model picker now re-prices both the headline "$ saved" and the applied-value estimate against the selected model's input rate (Opus 4.8 / Fable 5 / Sonnet 5 / Haiku 4.5).

### Changed
- All dashboard dropdowns are custom-rendered so they theme correctly (native menus are drawn by the OS and cannot be styled to match).

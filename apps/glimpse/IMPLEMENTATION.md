# Implementation evidence

Implemented: native GTK launcher and utilities; independent filename index and
watching daemon; one sectioned Settings extension page; reversible window/shortcut block;
Glimpse-owned semantic store, readers, hybrid search, and llama.cpp wrapper; plugin
runtime, permissions, catalog/local/Git/development installs, rollback, SDK, and examples;
suite installer/CLI/uninstaller integration.

The first implementation used Recall. User direction changed to all Glimpse-owned
production code; the Recall backend and vendored production source were removed.
Earlier Recall tests are research evidence only and do not verify the new engine.

Model artifacts were explicitly authorized, downloaded, and SHA-256 verified.
The unmodified pinned llama.cpp library and Glimpse inference helper build on CPU.
GPU testing is unavailable on this host because NVIDIA reports a driver/library
version mismatch; no driver changes were made.

Focused verification:

- 29 core tests passed, including both actual Settings schemas, source exclusions,
  WAL reads, disconnected roots, sandbox restrictions, reader plugins, and rollback.
- Glimpse’s own real multimodal pipeline test passed on CPU in 30.84 seconds, using
  verified weights: text, PDF page 2, images, audio/video locations, and source filtering.
- Own inference-wrapper query/reference cosine: 0.999918343 (required ≥ 0.999).
- Targeted Clippy passed with warnings denied; release build completed.
- Fresh XDG install, retained-data uninstall, reinstall, and explicit purge passed.
- Installed locally through the suite; Glimpse Settings helper reports one page with seven sections.
  Compositor validation returned no errors. Sources remain empty; semantic search and
  shortcut replacement remain disabled until selected in Settings.
- Desktop smoke check passed: calculator result 14, Enter copies it and hides the
  panel, another invocation toggles the same instance. CSS produced no parser errors.
- Fixed the smoke-discovered read-event refresh loop in UI and daemon watchers.

GTK emits existing shared-theme deprecation/frame-timing warnings on this host;
these did not prevent the smoke checks. No compositor shortcut replacements were
applied during testing.
Full 10k/100k benchmarks, broad hardware/accessibility review, and GPU measurements
remain release evaluation tasks; no latency targets are claimed as achieved.


## Usability corrections

The launcher now opens blank and focused at 800×64, expands to 800×520 while typing,
and collapses when cleared. Results have non-selectable category headings and stable
selection during enrichment, with applications first. Glimpse windows have themed
borders and scroll without visible scrollbars. Plugin dialogs have Close and Escape,
and their lists update after registry/catalog changes.

Settings now exposes one Glimpse page under Desktop, alongside Assistant, through an
optional backwards-compatible extension manifest category. Fixed shortcut/device save
handlers, completed exclusions, thermal/startup/currency/theme controls, and made
status information visible using the host protocol's value field. Background operations
release the Settings helper's output pipes and report progress/failures in Diagnostics.
Semantic reset preserves service state and does not touch the live service for scratch
paths. Catalog HTTP failures retain usable saved/bundled entries with a readable notice.

Focused checks: settings persistence/invalid-value cases, both real Settings schemas,
manifest compatibility, corrupt-cache fallback, an installed detached rescan, and the
real catalog HTTP failure. Desktop checks covered initial typing focus, category order,
compact/expanded/cleared sizing, reopening focus, Enter activation, and Plugins Escape.
Release build and Glimpse Clippy passed; compositor validation returned no errors.
Whole-Settings all-target Clippy encounters an existing items-after-test-module warning
in the unchanged privileged helper; no unrelated helper refactor was made.
No model inference tests were repeated for these UI changes.


## Compact Settings and query intent

The launcher is now 640 pixels wide. Calculation and conversion intents bypass every
other provider; valid math displays the expression and answer, with a native interactive
keypad preview. Unit results show the requested target first and related units after it.
Plain search uses conjunctive terms, exact numeric tokens, and name-first application
ranking. Semantic-only suggestions have their own category, a five-file cap, a 0.08
margin from the best qualifying match, and a conservative 0.65 cosine floor.

The initially planned 0.45 floor failed the small real-inference relevance sample:
unrelated spacecraft text produced a contract-document cosine of 0.562. Raising the
floor to 0.65 retained the relevant agreement query at 0.759 and rejected the unrelated
query. This is a focused text calibration, not a full multimodal quality evaluation.

Settings uses native collapsible sections through optional group protocol fields,
with compact per-folder disclosures, nested exclusions, stable source-path keys,
and remembered disclosure state during polling. General contains only common indexing
controls; shortcuts, limits, and currency settings have their own folded sections.
Focused checks passed for intent classification, math, conversion aliases and related
units, exact numeric/multiword/phrase matching, both Settings schemas, literal FTS input,
and Settings protocol compatibility. Desktop checks covered the 640-pixel math view, exclusive conversion results with related mass units, keypad calculation, Copy, continued typing after keypad use, and opening/closing compact folder disclosures. Both updated apps were installed. Additional testing was stopped at the user’s request.

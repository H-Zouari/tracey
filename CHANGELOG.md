# Changelog

## Unreleased

### Added

- **StrictDoc versions come from the `VERSION` field.** A `.sdoc` node at
  `VERSION: 2` is rule `uid+2`, so older code references are stale and
  `@relation` markers are stale against it. `tracey bump` and
  `tracey pre-commit` now work on `.sdoc` files: a change to a node's
  normative fields needs its `VERSION` raised by one, which `tracey bump`
  writes in place. Normative fields default to the Fleet Spec Format's list
  and are configured per element with `format.sdoc.normative_fields`.
- **Retired StrictDoc nodes.** `STATUS: retired` excludes a node from
  coverage, and a code reference to it is a `RetiredRequirement` error.
- **StrictDoc flows are verified through their steps**: a `[[FLOW]]` with no
  `verify` reference of its own counts as verified when every `[STEP]` in it
  has a current one.
- **Stale references to StrictDoc nodes show what changed**: the node's
  normative fields at the referenced version and now.

### Changed

- **A `+N` suffix in a StrictDoc UID is an error**; the version belongs in
  the `VERSION` field. Before, `UID: CH-001+2` was read as version 2.
- **`[TERM]` StrictDoc nodes are no longer requirements.**

- **`exclude` patterns are matched relative to the project root**, as the
  configuration guide documents. Before, they were matched relative to the
  include pattern's directory, so a pattern like
  `crates/tracey/tests/fixtures/**` under `include (crates/**/*.rs)` never
  matched. Patterns written the old way (`tracey/tests/fixtures/**`) still
  match but are **deprecated**: tracey prints a warning with the
  project-relative pattern to use instead. A future release will stop
  matching them.
- **`exclude` now applies to `test_include` files.** An excluded file is no
  longer scanned as a test file, so it no longer causes `ImplInTestFile` or
  unknown-reference errors.
- **`@relation` markers follow StrictDoc's grammar strictly** (via
  strictdoc-parser 0.2):
  - A marker must start a comment line, optionally after the comment
    leader (`//`, `#`, `*`, `--`, …). A mention inside prose such as
    `// see @relation(CH-001) for details` is ignored.
  - Arguments must be separated by a comma and a space.
    `@relation(CH-001,scope=function)` is malformed: it now produces a
    warning and **no reference**, where earlier releases accepted it.
  - Markers that don't parse produce a warning instead of being dropped
    silently.

  If coverage drops after upgrading, check `tracey query validate` for
  malformed-reference warnings.

### Added

- StrictDoc noun roles: `role=Implementation` counts as `impl`;
  `role=Verification` and `role=Test` count as `verify`.
- StrictDoc `scope=class`, `scope=range_start` and `scope=range_end`. A
  `range_start`/`range_end` pair counts once.
- `.sdoc` specs: custom-grammar elements (`[FEATURE]`, …) and composite
  `[[…]]` nodes, together with the requirements nested inside them, are
  loaded as requirements. `[TEXT]` blocks are rendered in the spec view.
- `tracey_status` explains `@relation` markers for StrictDoc specs.

### Fixed

- An include pattern with a partial file name such as `tests/parse_*.rs`
  now scans `tests/` instead of looking for a directory named
  `tests/parse_`.

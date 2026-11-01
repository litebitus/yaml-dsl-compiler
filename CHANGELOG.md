# Changelog

## Unreleased

## v0.1.1 — 2026-11-01

- Compiler
  - Reads `yaml-dsl.yml` up to v1.2, through yaml-dsl-spec 0.1.2.

## v0.1.0 — 2026-11-01

- Compiler
  - Discovers units, folds layers, expands macros, resolves names and validates against the document's schema.
  - Resolves key placeholders before the fold and lists a unit's declarations in document order.
  - Reads a placeholder after `placeholder.escape_prefix` as text.
  - Takes each unit's schema from the caller's schema provider; it finds, fetches and reads none itself.
  - Places a schema error at what is wrong, with the rule it breaks and the closest allowed name.
  - Takes the DSL definition, YAML reading, diagnostics, the function table and schema validation from [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec).
- Writers
  - HCL, JSON and YAML, each in one style.
  - HCL writes conditionals, `for` expressions, operations, splats, expression keys and template directives.
- Example
  - `mock-dsl-compiler` compiles a mock DSL into HCL.

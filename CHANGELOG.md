# Changelog

## Unreleased

- Compiler
  - Discovers units, folds layers, expands macros, resolves names and validates against the document's schema.
  - Resolves key placeholders before the fold and lists a unit's declarations in document order.
  - Reads a placeholder after `placeholder.escape_prefix` as text.
  - Reads a schema URL with a copy kept of it, by the editor's rules.
  - Places a schema error at what is wrong, with the rule it breaks and the closest allowed name.
- Writers
  - HCL, JSON and YAML, each in one style.
  - HCL writes conditionals, `for` expressions, operations, splats, expression keys and template directives.
- Example
  - `mock-dsl-compiler` compiles a mock DSL into HCL.

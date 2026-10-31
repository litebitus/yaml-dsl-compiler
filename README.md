# yaml-dsl-compiler

A Rust library that compiles YAML DSLs into code. A DSL is defined by configuration, the same `yaml-dsl.yml` the [YAML DSL editor](https://github.com/litebitus/vscode-yaml-dsl-editor) reads, so a document compiles as the editor reads it.

## Features

- Layers: overlays fold over their common layer.
- Placeholders, locals and loop variables, resolved by the DSL's scopes.
- Declarations and references, checked and kept as references for the target.
- Macros expanded: `merge`, `loop` and `function_call`; other functions stay calls.
- Validation against the schema the document declares, with errors at file, line and column.
- Writers for HCL, JSON and YAML, each in one readable style; HCL is left unchanged by `terraform fmt`.
- Units compile in parallel.

## Usage

A caller supplies three things and builds its own binary:

| Supplies | How |
|---|---|
| the DSL definition | a DSL block of a `yaml-dsl.yml`, read by `DslDefinition::from_json` |
| a file source | the `FileSource` trait: list and read files |
| a translator | the `Translator` trait: a resolved unit in, output files out |

## Example

[examples/mock-dsl-compiler](examples/mock-dsl-compiler) compiles a mock DSL into HCL. Its [workspace](examples/mock-dsl-compiler/workspace) opens in the editor, and the tool compiles it:

```sh
make example
```

What a DSL compiles to is stated under its block's `code_generation` key, which the example tool reads and the library does not.

## Development

| Target | Does |
|---|---|
| `make test` | format check, clippy with warnings as errors, every test |
| `make build` | `make test`, then a release build |
| `make example` | compiles the example workspace, rewriting its committed output |

The example tool's integration tests run `terraform fmt -check`, so `terraform` must be on the path.

### Releases

Both workflows are dispatched by hand, run the tests, and publish to crates.io with the token in the `CARGO_REGISTRY_TOKEN` secret.

| Workflow | Publishes |
|---|---|
| `Snapshot` | a pre-release such as `0.1.1-snapshot.20261030175512`, tagged on its commit |
| `Release` | the next version: stamps `CHANGELOG.md` and the crate version, tags, and publishes a GitHub release |

Cargo never picks a pre-release on its own, so a consumer pins a snapshot by its exact version:

```toml
yaml-dsl-compiler = "=0.1.1-snapshot.20261030175512"
```

The design is in [docs/design.md](docs/design.md).

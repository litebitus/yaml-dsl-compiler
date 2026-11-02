# YAML DSL compiler

A YAML DSL is a YAML document with a language on top: layers that fold into one document, placeholders, names declared in one place and referenced in another, functions, and macros that expand before the document is used. The [YAML DSL editor](https://github.com/litebitus/vscode-yaml-dsl-editor) makes such a document authorable. This compiler turns it into code.

The compiler is a Rust library. Nothing about a particular DSL is compiled into it: a DSL is a definition the caller hands over, in the shape of a DSL block of the editor's config, and the editor and the compiler read the same definition the same way. A program embeds the library, adds a translator for its DSL, and builds its own binary.

## Pipeline

| Stage | Does | Owner |
|---|---|---|
| Discover | finds the DSL's documents and groups each overlay with its common layer into a unit | library |
| Fold | merges a unit's layers into one document | library |
| Expand | expands macro calls | library |
| Resolve | reads placeholders, locals, loop variables and references by the definition's scopes | library |
| Validate | checks the folded document against its schema | library |
| Translate | turns the resolved document into an output tree | caller |
| Write | renders the output tree in a target language | library |

A unit is compiled whole in one pass, from its documents to its output files. Units are independent and compile in parallel.

- An overlay holding no documents, as the standard's "Common layer and overlays" states, is a unit: it reads no file of its own, and folds to its common layer alone. Its schema is found from its place.
- `Compiler::overlays_holding_no_documents_at` gives the overlays holding no documents whose place is a directory, for a caller that compiles one directory without listing the config root.

### Fold

The overlay is merged over its common layer, key by key, at any depth:

- A map merges into the common layer's map; any other value replaces the common layer's.
- A null deletes the key.
- A `{}` or `[]` at a key the common layer holds is an opt-out and deletes the key. At a key the common layer does not hold, it is content.

Keys are merged as they resolve: a placeholder in a key that reads a builtin the caller gives a value, or a local holding a scalar, is resolved in each layer before the fold. Any other placeholder in a key resolves after it.

## Inputs

| Input | Holds |
|---|---|
| DSL definition | a DSL block of a `yaml-dsl.yml`, read and validated by [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec)'s `read_dsl_definitions` |
| File source | how the library lists and reads files |
| Schema provider | the caller's code that finds each unit's schema |
| Translator | the caller's code for one DSL and one target |

### DSL definition

The library reads the definition's fields with the meaning the editor's design doc gives them, so a document compiles as the editor reads it:

- which files belong to the DSL, and its layers and overlays;
- its placeholders, scopes, declarations and references;
- its locals and loop variables;
- its functions and macros, through the function table the document's schema publishes at `#/x-yaml-dsl-functions`;
- the schema search paths, kept for the caller, whose schema provider finds the schema.

Fields that only shape editing, such as key sort orders, are not read. The library never loads a config file: the caller reads it with [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec), which returns each DSL's definition validated, or every problem at its field.

What the editor reads and the compiler does not yet:

| Feature | When the compiler meets it |
|---|---|
| `common_layer_discovery: overlay_includes` | the DSL is an error |
| a `transform` macro | the call is an error |
| a `merge` from a location | the content is left to the target, as a call |

### Schemas

The library is pure code: it finds, fetches and reads no schema itself. The caller's schema provider hands it each unit's schema: its text, the name reports give it, the fragment of the schema a fragment document's root is, and what finding it warns of. A program finds a schema the way its DSL calls for, such as the editor's rules of a modeline and then `schema_search_paths`; the library keeps the definition's `schema_search_paths` for it to read.

- Each schema is parsed and compiled once per run, by its name and text, however many units read it.
- What finding a schema warns of comes back with the resolved unit, as warnings that fail nothing.

### Builtin values

A logical scope's builtin names are the caller's to give values, through a hook asked once per name a unit reads. A name the hook gives no value stays a reference, for the target to read.

### Fragments

A document the schema provider names a fragment, as the editor reads a modeline's fragment, is never compiled alone: its unit compiles to no files.

### File source

A trait the caller implements: list the files under a directory, read a file. The library reads nothing else from the disk, so it runs the same on a working tree, an editor's open buffers, or an in-memory fixture.

## Resolved document

What Resolve hands the translator: the folded, expanded document as a tree of values, every value carrying the file, line and column it was written at.

| Value | Holds |
|---|---|
| scalar | a string, number, boolean or null |
| list | values in order |
| map | key and value pairs in document order |
| reference | the scope and name it reads, its path into the declaration, and the declaration it resolves to |
| call | a function call left for the target to evaluate: the function and its arguments |

- A macro is expanded, so the resolved document holds what it expands to and never the macro call.
- A function is not evaluated: it belongs to the target, so its call stays a call.
- A placeholder in a scalar resolves to a constant where every name it reads holds one, and otherwise to a template of text and references.
- A placeholder written after the definition's `escape_prefix` is text, with the prefix dropped.
- A reference keeps both what it reads and what that resolves to, so the translator decides whether the target sees a reference or the value.
- A reference to a local whose path reaches into a value known only when the document is used, such as a field of a call's result, resolves to the reference alone.

Beside the document, Resolve lists the names it declares in document order: each with its scope, the key the scope is named by, the declaring key, its path and its value. The value carries the field its key sets by the definition's `child_values_from_parent_key`, first, unless the value writes that field itself.

## Translator

A trait the caller implements for one DSL and one target. It receives a unit's resolved document and returns an output tree: the files to write, each with its content.

| Output value | Holds |
|---|---|
| block | a type, its labels, and a body of attributes and blocks |
| attribute | a name and a value |
| scalar, list, map | as in the resolved document |
| traversal | a reference in the target's syntax: a root, a name or an expression, and the attributes, indexes and splats after it |
| call | a function and its arguments |
| template | text with expressions interpolated, and `if` and `for` directives, each tag with its strip markers |
| conditional, `for` expression, operation, parentheses | the target's expressions, so a translator can write what a hand-written file holds |

A map's key is a name, or an expression the target evaluates for the key.
| comment | text attached to a file, block or attribute, written on the lines above it |

Calls, traversals and templates are structured rather than text, so the writer lays them out. The output tree is the order the file is written in: the writer adds no sorting of its own, and what comes first is the translator's decision.

## Writers

The output is generated code: nobody edits it, but people read and review it. Each writer has one style, the most readable it can write, and no settings.

| Language | Writes |
|---|---|
| HCL | blocks and attributes, in the style below |
| JSON | two-space indentation in tree order; blocks as nested objects keyed by type then labels |
| YAML | block style, two-space indentation; a list of one or two scalars in flow style when the line fits |

- JSON and YAML have no expressions: a traversal, call or template is refused, as is a comment in JSON and a block in YAML.
- A string YAML would read back as another value, such as `yes` or `30`, is double-quoted; a multi-line string is a literal block.

HCL style:

- Two-space indentation, by bracket depth as `terraform fmt` counts it: a line closing as many brackets as it opens, such as `} : name => merge(`, stays at the level of the lines inside, and a heredoc's body is kept as written.
- A blank line on each side of a block and of an attribute holding a multi-line map; none between other attributes.
- `=` aligned within a run of single-line attributes; a multi-line value or a comment ends the run, and a heredoc stays in it.
- A list or map of three or more items is written one item per line with a trailing comma; one or two items stay inline when the line fits.
- No line is wider than 100 columns where the language lets a line break:
  - a call that does not fit takes its arguments one per line, or, where its last argument is a map or a list, wraps it: `merge(local.base, {` to `})`;
  - a list or map that does not fit goes one item per line;
  - a `for` expression that does not fit puts its brackets on lines of their own and breaks its collection and value;
  - a conditional that does not fit keeps its condition on the first line and breaks its branches;
  - an operation and an interpolation stay on one line.
- An operation takes parentheses where precedence needs them, and an expression key takes them unless it is a template.
- A multi-line string is an indented heredoc, `<<-EOT`.
- What the writer writes, `terraform fmt` leaves unchanged.

```hcl
module "queue_mock_orders" {
  source  = "registry.example.com/mock/components//queue"
  version = "1.2.3"

  config = {
    name       = "mock-orders"
    visibility = 30
    kms_key    = module.key_mock_main.o.arn
    consumers = [
      "mock-billing",
      "mock-shipping",
      "mock-audit",
    ]
    tags = { team = "mock", tier = "gold" }
  }
}
```

Writing is deterministic: the same output tree gives the same bytes.

## Errors

- Every error carries the file, line and column of the value it is about.
- A unit reports every error it finds, not only the first.
- A schema error is placed at the value that breaks the rule, or at the key of a field the schema does not allow, and carries what is wrong there, the rule it breaks as its keyword and its JSON Pointer in the schema, and, where one allowed field or value is spelt closest, that name.
- A unit with errors writes no files; the other units are unaffected.

## Rust API

The crate `yaml-dsl-compiler`, beside [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec), which holds the DSL definition's types and reader, the YAML reader with positions, `Diagnostic`, `json_path`, the resolved document's types, the function table, the schema validation and `closest_name`:

| Item | Is |
|---|---|
| `Compiler` | built from a DSL definition, the config root and a file source; lists units, resolves one, compiles one or all |
| `BuiltinValues` | the caller's hook giving builtin names their values |
| `ResolveOptions` | one resolution's builtin values and schema provider, each in place of the compiler's own |
| `SchemaProvider`, `DocumentSchema` | the caller's code that finds a unit's schema, and what it hands over |
| `Translator` | the caller's code turning a resolved unit into output files |
| `OutputFile` | a path relative to the unit's directory, its target language, an optional comment and its body |
| `CompiledUnit` | a unit and either its written files or every error it holds |
| `ResolvedUnit` | a unit's resolved document and the names it declares |

The library renders files and returns them; writing them to disk is the caller's.

## Example tool

The repository's example binary is a caller like any other:

- It reads a `yaml-dsl.yml`, takes its DSL block, and hands it to the library with a file source over the working tree that skips hidden directories and gitignored paths.
- Its translator compiles an example DSL into HCL.
- Its example folder is a workspace the editor opens: the documents are authored in the editor and compiled by the tool, and both read the same `yaml-dsl.yml`.
- What a DSL compiles to is stated in the tool's own config, `mock-dsl-compiler.yml`, by the DSL's id: a code generator keeps its settings in a file of its own, never in the editor's `yaml-dsl.yml`.

Its integration tests run the binary over the example folder, compare the written files, and run `terraform fmt -check` over the HCL it writes.

## Dependencies

| Crate | For |
|---|---|
| a YAML parser that reports positions | reading documents with file, line and column |
| `fancy-regex` | the definition's patterns, written for the editor's JavaScript regular expressions |
| `jsonschema` | validating against the schema the document declares |
| `rayon` | compiling units in parallel |
| `globset` | the definition's file patterns |
| `serde`, `serde_json` | reading the definition and the schema |
| `ignore` | the example tool's file source |

Each is pinned to its latest release when it is added.

# code-trail

A fast, context-aware code navigation engine, initially for Emacs, with a path toward Language Server Protocol (LSP) support and integrated documentation lookup.

> **Status:** Phase 2 implemented: syntax-aware Java definition, reference, and implementation navigation in Emacs, backed by a persistent Rust process, ripgrep, and bundled Tree-sitter. Parsed trees and recent results are cached; semantic resolution and LSP remain planned.

## Getting started

Requirements: Rust with Cargo (edition 2024 support), ripgrep (`rg`) on `PATH`, and Emacs 29.1 or newer.

```sh
cargo build --release
./target/release/code-trail --help
```

Add the Emacs directory to `load-path` and configure the executable:

```elisp
(add-to-list 'load-path "/absolute/path/to/code-trail/emacs")
(require 'code-trail)
(setq code-trail-executable "/absolute/path/to/code-trail/target/release/code-trail")
(add-hook 'java-mode-hook #'code-trail-mode)
(add-hook 'java-ts-mode-hook #'code-trail-mode)
```

### Emacs key bindings

Open a Java file, enable `code-trail-mode`, save the buffer, and place point on a symbol.

| Key | Command | Action |
| --- | --- | --- |
| `M-.` | `xref-find-definitions` | Jump to a definition; select from results when ambiguous |
| `M-?` | `xref-find-references` | List exact-name uses in the project |
| `M-,` | `xref-go-back` | Return to the previous navigation location |
| `M-x` | `code-trail-find-implementation` | Find source type descendants or matching method bodies |

`M-` means Meta, usually the Alt key. `M-?` means Alt+Shift+/ on a US keyboard. The same commands are available through `M-x`.

Results use the standard `xref` UI. Finding a definition while already on its declaration can leave point in the same place. Reference search excludes declarations and is syntax-aware, not semantic: unrelated symbols with the same name can appear.

Evil overrides `M-.` in normal state. Add the following after the setup above to use these navigation bindings in Java buffers without changing other languages:

```elisp
(with-eval-after-load 'evil
  (with-eval-after-load 'cc-mode
    (evil-define-key '(normal motion visual) java-mode-map
      (kbd "M-.") #'xref-find-definitions
      (kbd "M-?") #'xref-find-references
      (kbd "M-,") #'xref-go-back))
  (with-eval-after-load 'java-ts-mode
    (evil-define-key '(normal motion visual) java-ts-mode-map
      (kbd "M-.") #'xref-find-definitions
      (kbd "M-?") #'xref-find-references
      (kbd "M-,") #'xref-go-back)))
```

For an already-open Java buffer, run `M-x code-trail-mode` once if the mode is not enabled. `M-x code-trail-stop` stops the shared engine; the next request starts it again.

Navigation reads saved Java files only. Save modified buffers before navigating. Tree-sitter recognizes declarations and lexical scopes. Source analysis resolves supported receivers and inheritance, but does not provide complete Java semantic resolution: dependency JARs, annotation-generated members, general generic substitution, and some overload conversions remain unresolved. No language server or project index is required; the Java parser is bundled into the executable.

### Development checks and benchmarks

```sh
cargo build
python3 -m unittest discover -s tests -v
CODE_TRAIL_EXECUTABLE="$PWD/target/debug/code-trail" emacs -Q --batch -L emacs \
  -l emacs/code-trail-tests.el -f ert-run-tests-batch-and-exit
cargo build --release
python3 benchmarks/navigation.py
```

The benchmark generates temporary Java projects with ambiguous type names. It reports first-request latency including process startup, repeated-request median/p95 latency, and top-result correctness. Filesystem caches are uncontrolled; these are synthetic baselines, not real-project performance guarantees or a Dumb Jump comparison.

## Vision

code-trail aims to provide the convenience and speed of [Dumb Jump](https://github.com/jacktasia/dumb-jump) while improving result quality through syntax awareness, contextual ranking, and optional semantic integrations. It should remain useful even when a language server is unavailable.

The long-term goal is a reusable Rust engine that can serve Emacs first and, later, other editors through LSP.

## Design principles

- **Fast by default:** minimize work on the critical path; avoid full-project scans or reparsing when unnecessary.
- **Works without an index:** provide immediate, best-effort navigation using fast text search.
- **Progressively smarter:** use syntax trees, project context, and optional language-server answers to improve precision.
- **Editor-independent core:** keep Emacs and LSP protocol details outside the search and analysis engine.
- **Graceful degradation:** navigation must still work when parsers, indexes, or external language servers are missing.
- **Documentation as a first-class future capability:** use the same symbol context to find code documentation.
- **No LLM dependency for navigation:** AI-powered documentation search may be added later, but it must not block ordinary jumps.

## Technology choices

| Component | Preferred technology | Reason |
| --- | --- | --- |
| Core engine | Rust | Efficient native execution, memory safety, strong libraries |
| Initial editor interface | Emacs Lisp | Native `xref` and later `eldoc` integration |
| Fast text search | ripgrep (`rg`) initially | Mature, fast, project-aware search |
| Syntax analysis | Tree-sitter | Incremental parsing and syntax-aware candidates |
| Communication | Persistent process over stdio | Avoid process startup on each navigation request |
| Transport messages | JSON-RPC or a small versioned JSON protocol | Straightforward interoperability; can evolve separately from LSP |
| Optional index | In-memory cache; persistent index if needed | Speed up repeated queries without imposing initial indexing cost |
| Documentation search | Local files first; optional SQLite FTS5 later | Efficient full-text lookup without mandatory remote services |
| Future editor integration | LSP server adapter | Reuse the core with Emacs, Neovim, VS Code, and others |

Rust is the main implementation language. Emacs Lisp should be limited to editor interaction, request handling, result display, and user configuration.

## Architecture

```text
                  +-------------------------+
                  |  Emacs (initial client) |
                  |  xref / eldoc / commands|
                  +------------+------------+
                               |
                         stdio protocol
                               |
                  +------------v------------+
                  |    Emacs protocol adapter|
                  +------------+------------+
                               |
   Future:                  +---v---------------------------+
   +------------------+     |        Shared Rust Core       |
   | LSP clients       |     |                               |
   | Emacs / Neovim /  +---->| symbol resolution             |
   | VS Code           |     | candidate ranking             |
   +--------+---------+     | project context               |
            |               | caches / optional indexing    |
   +--------v---------+     | documentation lookup (later)  |
   | LSP server adapter|---->|                               |
   +------------------+     +---+-------------+-------------+
                               |             |
                         +-----v----+  +-----v--------+
                         | ripgrep  |  | Tree-sitter  |
                         +----------+  +--------------+

           Optional future integration: existing language servers
```

The adapters convert editor/protocol requests into an internal request type. The core should not depend on Emacs, JSON-RPC, or LSP types.

### Internal API (illustrative)

```rust
pub struct NavigationRequest {
    pub file: std::path::PathBuf,
    pub position: Position,
    pub symbol: String,
}

pub struct Location {
    pub file: std::path::PathBuf,
    pub range: Range,
}

pub trait NavigationEngine {
    fn definitions(&self, request: &NavigationRequest) -> Vec<Location>;
}
```

These types are sketches, not a committed public API. Position encoding, errors, cancellation, async behavior, and metadata must be settled before implementation.

## Navigation pipeline

1. Extract the symbol and its editor context (file path, cursor position, surrounding syntax).
2. Check a valid cache entry or optional index, if available.
3. Discover candidate definitions through a fast project search.
4. Parse only relevant files/regions when syntax information improves accuracy.
5. Rank candidates based on syntax role, scope, imports, file/directory proximity, and other project signals.
6. Return usable locations quickly, even if higher-quality analysis is incomplete.
7. Optionally consult an existing language server for authoritative semantic answers when configured.

### Candidate ranking

Initial ranking can be deterministic and inexpensive, using signals such as:

- Definition/declaration versus ordinary reference
- Symbol kind (function, type, variable, method, etc.)
- Same file or nearby directory
- Import and module relationships, when known
- Scope and syntactic context, when available
- Exact-name matches and language-specific conventions

Syntax trees improve recognition of declarations, but **Tree-sitter alone does not provide full semantic resolution** (for example, cross-module type inference, overload resolution, or dynamic dispatch). For those cases, an existing language server can provide better answers.

## Emacs integration

The initial package should provide an `xref` backend for familiar editor navigation, rather than implementing a separate navigation history or locations UI. Later, use `eldoc` for symbol documentation and a dedicated buffer for longer results.

Planned user-facing capabilities:

- Jump to definition
- List/select candidate definitions when ambiguous
- Fall back to text/syntax search when no LSP answer is available
- Find references using syntax-aware exact-name matching (implemented; not semantic resolution)
- Inspect documentation at point (later)

The first version may use a small custom request protocol. Keep it versioned and internal so it can evolve without constraining the future LSP adapter.

### Stdio protocol

The engine accepts one JSON request per line and flushes one response per request. It runs until stdin closes. Version 1 supports `definition`, `references`, and `implementation`, using the same parameters and location response format:

```json
{"version":1,"id":1,"method":"definition","params":{"file":"/project/src/Use.java","line":3,"column":12,"symbol":"Widget"}}
```

`file` must be absolute. Lines are one-based; columns count zero-based Unicode scalar characters, not bytes or UTF-16 code units. Successful responses contain ranked locations:

```json
{"version":1,"id":1,"result":[{"file":"/project/src/Widget.java","line":2,"column":13,"summary":"public class Widget {}","kind":"type","score":100}]}
```

The score above is illustrative; callers must preserve result order rather than depend on score values. No matches returns an empty result array. Errors contain `error.code` and `error.message` instead of `result`; malformed requests do not terminate the process.

This protocol is not LSP. Future adapters must perform their own position conversion and lifecycle handling.

For usages, send `"method":"references"`. Results have `"kind":"reference"` and exclude recognized declarations, imports, package names, comments, and string contents. They include exact-name method calls, method references, and type or variable uses. Requests on recognized local variables, parameters, and lexical type parameters restrict results to that binding, including captured lambda uses and excluding shadowed locals and qualified fields. Source-resolved field requests match the declaring owner, including inherited and qualified uses, while excluding shadowed locals and unrelated same-name fields. Other requests remain project-wide syntax searches: unrelated same-name members and overloads can appear. Reference requests share parsed-tree caching and file-change invalidation with definitions, but keep separate result-cache entries. Field reference requests recompute owner matching because intermediate hierarchy files may not contain the field name. All methods read saved files only.

For implementations, send `"method":"implementation"`, or run `M-x code-trail-find-implementation` in an enabled Java buffer. A prefix argument prompts for an identifier. Results use the standard xref UI and navigation history. At a recognized type declaration or type use, requests return class, enum, record, and anonymous descendants through source-visible interfaces and superclasses. Anonymous results point to the instantiated base type after `new`. Package-qualified names distinguish unrelated types. Generic type arguments alone do not count as supertypes. Requests without a recognized type context retain best-effort direct-supertype name matching.

Method lookup follows the declaring type's source hierarchy, including intermediate interfaces, subclasses, and anonymous class method bodies, and compares erased parameter types with array and varargs normalization. Call-site lookup searches inherited declarations on the receiver before finding implementations. Supported receivers include explicitly typed locals, initializer-inferred `var` locals, parameters, declared and inherited fields, static fields, enum constants, `this`, `this.field`, `super`, source type names, object creation, casts, and source-return-typed call chains. Explicit and on-demand static method imports contribute source receiver owners for unqualified calls and call chains. Generic factories returning `T` from a corresponding `Class<T>` parameter infer `T` from a class literal; this covers `CwRegistry.get(Service.class)` without registry-specific rules. Unambiguous method references also resolve their receivers.

Call lookup uses argument count, primitive widening, exact reference-type matches, and source getter return types to distinguish overloads. Fixed-arity candidates precede expanded varargs candidates. Unknown types, conflicting source return types, and incomparable overloads remain ambiguous. Implementation lookup reports unresolved receivers or overloads instead of returning unrelated methods. Definition lookup returns only the selected declaration when receiver and overload resolution succeeds; otherwise, it retains syntax-ranked fallback results.

Abstract method declarations, constructors, ordinary uses, and unrelated same-name methods are excluded from method implementation results. Interface default methods and the selected concrete declaration can be returned. Results retain `"kind":"type"` or `"kind":"method"`. This is source analysis, not a Java compiler: boxing, general reference-subtype overload conversions, generic inheritance substitution, overloaded method-reference target typing, ambiguous wildcard imports, Lombok-generated members, and dependency JARs are not resolved. Implementation requests share source validation and parsed trees, with separate result-cache entries. Hierarchy discovery validates files mentioning related types, so intermediate-interface edits invalidate cached results.

#### Crosswired Java convention review

The `crosswired-common` review found these navigation-relevant patterns:

| Convention | Navigation handling |
| --- | --- |
| Service Builder API, generated base, concrete implementation, wrapper, and Util classes | Resolve receiver declarations through source inheritance; follow `getService()` chains |
| OSGi `@Reference` fields | Use declared and inherited Java field types; bind field references to their source owner |
| `CwRegistry.get(Service.class)` and similar generic factories | Infer class-token return types from source declarations |
| Qualified/generic supertypes and intermediate interfaces | Find transitive named descendants without unrelated-package matches |
| Static imports of methods and nested types | Resolve explicit and on-demand method owners; rank imported nested declarations |
| Bound method references and `super` calls | Resolve unambiguous source declarations and implementations |
| Anonymous implementations | Keep method owners distinct; return anonymous type implementations at their base type |
| Locals, lambda captures, and switch blocks | Restrict local references and definitions to lexical bindings; infer supported `var` initializers |
| Fixed-arity and varargs overloads | Prefer applicable fixed-arity candidates |
| Enum constants and static service fields | Resolve the field or enum type before looking up methods |

These fixes do not establish complete coverage of all Java in that repository. External Liferay APIs, generated members absent from saved source, and the semantic cases listed above require a classpath-aware Java resolver for authoritative navigation. Method and unresolved member reference results remain exact-name candidates, not a complete semantic reference graph.

Verification included 28 public-protocol tests and eight real-repository requests: definition and implementation on `AnnouncementStatus.PUBLISHED.getKey()`, statically imported `sendRespond(...)`, and `CwRegistry.get(CwMongoInstanceProvider.class).getInstance()`; field references for `PrivacyIdeaIpn.privacyIdeaAdapter`; and anonymous `Accessor` implementations from `Announcement.java`. These sampled cases are not an exhaustive repository-wide semantic audit.

### Project discovery and ranking

The engine searches from the outermost ancestor containing `.git`, `settings.gradle`, or `settings.gradle.kts`. Without those markers, it uses the nearest Maven/Gradle build file, then the source directory as a final fallback.

ripgrep finds Java files containing the requested text, honoring `.gitignore`, `.ignore`, and `.rgignore` even outside a Git repository. The current Java file is always considered, including when ignored. Only candidate files receive Tree-sitter analysis; comments and string contents do not produce symbol candidates.

Syntax-recognized types, constructors, methods, fields, parameters, and local variables rank above uses. Ranking considers the requested syntax role, enclosing type, visible lexical scopes, explicit and wildcard imports (including static and nested-type imports), package, and directory proximity. Local declarations outside the request's scope are excluded; qualified member accesses do not select local variables. Path and position break ties deterministically. If no declaration is recognized, the engine returns best-effort reference locations instead.

The shared Rust `Engine` retains up to 2,048 parsed source snapshots and trees, plus 64 recent query results. Every request rediscovers candidate paths and compares saved source bytes before reusing cached data. Same-size edits, preserved timestamps, atomic replacements, additions, renames, deletions, and ignore-rule changes therefore invalidate affected results without a watcher. Changed files are reparsed; unchanged files reuse their syntax trees. These limits bound entry counts, not total bytes.

Incremental project indexing remains optional and is not enabled. A synthetic comparison using `python3 benchmarks/navigation.py --sizes 10 100 1000 --repeats 5` measured:

| Java files | Phase 1 cold (ms) | Phase 2 cold (ms) | Phase 1 warm median (ms) | Phase 2 warm median (ms) |
| --- | --- | --- | --- | --- |
| 11 | 6.176 | 7.203 | 5.612 | 5.413 |
| 101 | 7.951 | 10.779 | 6.197 | 7.385 |
| 1,001 | 24.792 | 100.800 | 21.960 | 21.959 |

Both versions selected the expected top result on all six requests at each size. Phase 2 adds cold parsing cost; cached requests avoid reparsing but still pay for discovery and source validation. These small, uncontrolled-cache synthetic samples do not establish a real-project need for an index or a speedup. Revisit indexing with representative workloads rather than adding background indexing and watcher complexity now.


## LSP evolution

Two distinct roles are possible:

1. **LSP client / broker:** code-trail requests semantic results from existing servers such as `rust-analyzer`, TypeScript language servers, or Python language servers.
2. **LSP server:** code-trail exposes its own navigation and documentation capabilities to any LSP-compatible editor.

These roles can coexist. The initial design should support neither by hard-coding LSP into the core.

### Future LSP capabilities

| Feature | LSP method |
| --- | --- |
| Definition navigation | `textDocument/definition` |
| Implementation navigation | `textDocument/implementation` |
| References | `textDocument/references` |
| Hover documentation | `textDocument/hover` |
| Document symbols | `textDocument/documentSymbol` |
| Workspace symbols | `workspace/symbol` |

An LSP server must also implement appropriate initialization and lifecycle handling, document synchronization, capability negotiation, errors, and cancellation. Capabilities should be advertised only when implemented.

### LSP-specific engineering considerations

- **Unsaved changes:** maintain per-document in-memory snapshots from `didOpen`, `didChange`, and `didClose` notifications; do not read only from disk.
- **Position conversion:** accurately map UTF-8 byte offsets to negotiated LSP character-position encodings, including UTF-16 where applicable.
- **Incremental updates:** invalidate affected caches and indexes after changes, without rebuilding the whole project.
- **Cancellation and concurrency:** stale searches should not block interactive requests.
- **Multiple language servers:** avoid duplicate or conflicting navigation results; define whether code-trail is a fallback, aggregator, or primary server.
- **Workspace boundaries:** respect ignore rules, root detection, and multi-root workspaces.
- **Protocol separation:** do not leak LSP structs or lifecycle assumptions into the core engine.

For a future LSP adapter, evaluate [`lsp-server`](https://crates.io/crates/lsp-server) for a lean transport or [`tower-lsp-server`](https://crates.io/crates/tower-lsp-server) for a higher-level framework. Make the choice when this phase starts, based on current maintenance and API fit.

## Future documentation support

Documentation lookup should build upon resolved symbol identity and project context rather than relying solely on string matching.

Potential sources, in order of complexity:

1. Nearby source-code doc comments
2. Project `README.md` files and Markdown documentation
3. Local API references and generated documentation
4. Dependency or library documentation (local/offline when available)
5. External documentation sources (opt-in)
6. Optional semantic or AI-assisted documentation search (off the navigation hot path)

Potential surfaces include Emacs ElDoc, a documentation buffer, and eventually LSP hover results. SQLite FTS5 can be introduced if full-text document search needs persistent indexing.

## Performance strategy

Performance is a design constraint, not an assumption based on language choice.

- Keep the engine running as a persistent process.
- Avoid mandatory indexing before the first successful jump.
- Search only relevant project paths and honor ignore rules.
- Cache parsed trees and recent results with explicit invalidation.
- Parse candidate files lazily rather than parsing entire repositories for each query.
- Prioritize and cancel interactive work appropriately.
- Keep optional remote or AI requests outside the latency-critical navigation path.

### Benchmark plan

Compare code-trail to Dumb Jump using identical repositories and representative definition lookups:

- Cold lookup latency (first request, no cache)
- Warm lookup latency (repeated request)
- Memory footprint and CPU usage
- First-result latency versus total completion time
- Correct-result rate, including ambiguous names
- Behavior across small, medium, and large repositories
- Behavior after unsaved edits and file changes

Do not publish target latency numbers until baselines and representative test repositories are established.

## Proposed project layout

```text
code-trail/
├── README.md
├── Cargo.toml
├── crates/
│   ├── code-trail-core/        # Editor-independent models and orchestration
│   ├── code-trail-search/      # Fast candidate discovery
│   ├── code-trail-parser/      # Tree-sitter integration
│   ├── code-trail-ranker/      # Context-aware candidate ranking
│   ├── code-trail-cli/         # Persistent process / initial protocol
│   ├── code-trail-docs/        # Future documentation lookup
│   └── code-trail-lsp/         # Future LSP server adapter
├── emacs/
│   ├── code-trail.el
│   ├── code-trail-xref.el
│   └── code-trail-eldoc.el
└── tests/
```

This is a target structure; create crates as they become necessary rather than scaffolding everything immediately.

## Roadmap

### Phase 1 — Fast Java navigation in Emacs (MVP)

Phase 1 supports Java only. Support for other languages is outside this phase.

- [x] Build Rust executable with persistent stdio communication
- [x] Detect project root and honor ignore rules
- [x] Search Java source files for definition candidates using ripgrep
- [x] Implement lightweight deterministic ranking for Java definitions
- [x] Implement Emacs `xref` backend
- [x] Return useful results without an index
- [x] Set up correctness and latency benchmarks using Java projects

### Phase 2 — Syntax-aware navigation

- [x] Distinguish definitions from uses
- [x] Improve contextual ranking and import awareness
- [x] Cache parsed syntax trees and recent results
- [x] Handle file changes and cache invalidation
- [x] Evaluate optional incremental indexing against benchmarks (deferred; see measurements above)

### Phase 3 — Semantic integrations and LSP

- [ ] Define explicit internal-to-LSP position and location conversions
- [ ] Support unsaved document snapshots
- [ ] Evaluate integration with existing language servers
- [ ] Implement a separate LSP server adapter
- [ ] Expose supported definition, references, and symbol methods
- [ ] Test with Emacs and at least one additional editor

### Phase 4 — Documentation intelligence

- [ ] Extract and display source doc comments
- [ ] Search project Markdown and API documentation
- [ ] Integrate with Emacs ElDoc and LSP hover
- [ ] Evaluate SQLite FTS5 for local documentation indexing
- [ ] Optionally add semantic documentation retrieval

## Key architectural decision

**Build the search, analysis, ranking, and documentation functionality as a reusable Rust core. Keep Emacs and LSP integrations as separate adapters.**

This preserves the ability to launch quickly as a faster, smarter Dumb Jump alternative and grow into an LSP-capable navigation and documentation engine without a major rewrite.

## References

- [Dumb Jump](https://github.com/jacktasia/dumb-jump)
- [Emacs Xref](https://www.gnu.org/software/emacs/manual/html_node/elisp/Xref.html)
- [Emacs ElDoc](https://www.gnu.org/software/emacs/manual/html_node/elisp/Documentation-Functions.html)
- [Language Server Protocol specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [Tree-sitter](https://tree-sitter.github.io/tree-sitter/)
- [ripgrep](https://github.com/BurntSushi/ripgrep)
- [rust-analyzer](https://github.com/rust-lang/rust-analyzer)

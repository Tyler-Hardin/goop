# Excellent Rust Style Guide: Type-Driven, Composable, Nearly Bug-Proof Design

**Target Audience**: AI coding agents (e.g., Goose, Aider, Claude, etc.) and human developers aiming for *great crate* quality.  
**Goal**: Produce code where invalid usage is a compile-time error, APIs are ergonomic and discoverable, architecture is modular and composable, and maintainability is high. Inspired by crates like `tower`, `serde`, `nom`/`chumsky`, `frunk`, Bevy, `std`, and developers with deep functional programming (FP) and practical category theory (CT) experience.  
**Philosophy**: Leverage Rust's type system, ownership, and zero-cost abstractions to *make invalid states unrepresentable*. Draw from FP (algebraic data types, composition, higher-order functions, monadic chaining) and CT concepts (functors via `map`, monads via `and_then`/`flat_map`, morphisms/composition in traits like `Service`) to design systems where *only valid compositions compile*. Balance with ergonomics and performance. Follow the official [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) as a baseline.

**Core Mantra**: "Make illegal states unrepresentable." (Adapted from strongly-typed FP traditions — Elm, Haskell, F#, now Rust.) Use the compiler as your strongest ally against bugs.

---

## 1. Foundational Type Design

The type system is your primary tool for correctness. Great crates use types to encode invariants so misuse is impossible.

### 1.1 Newtypes for Domain Invariants and Distinction
Wrap primitives or foreign types to create semantically distinct, validated types. Prevents mixing (e.g., `UserId` vs `OrderId` both `u64`).

```rust
// Good: Newtype with private field + validated constructor
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(u64);

impl UserId {
    pub fn new(id: u64) -> Result<Self, InvalidIdError> {
        if id == 0 {
            return Err(InvalidIdError::Zero);
        }
        // Additional domain rules...
        Ok(Self(id))
    }

    pub fn get(self) -> u64 { self.0 }
}

// Implement useful traits (Display, From, etc.)
impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user-{}", self.0)
    }
}

// Bad: Raw u64 everywhere — easy to pass wrong ID, no validation, no distinction
fn do_something(user_id: u64, order_id: u64) { ... }
```

**Benefits**: Self-documenting, validated at construction, zero-cost (newtype optimization), prevents whole classes of bugs.

**Also use for**: Units of measure (`struct Meters(f64)`), non-zero (`std::num::NonZeroU64`), etc. Consider `PhantomData` for type-level tags without data.

### 1.2 Enums for Sum Types, States, and Alternatives
Prefer `enum` over `struct` with many `Option`/`bool` fields when values represent alternatives or mutually exclusive/dependent states. Makes invalid combinations *unrepresentable*.

```rust
// Good: Enum encodes possible states cleanly
#[derive(Debug)]
pub enum ConnectionState {
    Disconnected,
    Connecting { attempt: u32 },
    Connected { session_id: SessionId, peer: SocketAddr },
    Failed { reason: String },
}

// Usage: Exhaustive match forces handling all cases
match state {
    ConnectionState::Connected { session_id, .. } => { /* only valid here */ }
    _ => {}
}

// Bad: Struct with dependent fields — invalid combos possible at runtime
pub struct BadConnection {
    is_connected: bool,
    session_id: Option<SessionId>,
    // Easy to have is_connected=true but session_id=None, or vice versa
}
```

**When to use enums**:
- Finite states or variants.
- Error kinds (`thiserror`).
- Configuration modes.
- Results of operations with different outcomes.

Combine with `Option`/`Result` for "absent" or "failure" instead of magic values or sentinel IDs.

### 1.3 Validation at Construction
Encapsulate construction. Make fields private. Return `Result` (or `Option`) from constructors/`TryFrom`.

```rust
pub struct Config {
    port: u16,
    use_tls: bool,
    // ...
}

impl Config {
    pub fn new(port: u16, use_tls: bool) -> Result<Self, ConfigError> {
        if port < 1024 && /* not allowed */ {
            return Err(ConfigError::PrivilegedPort);
        }
        // Cross-field validation if needed
        Ok(Self { port, use_tls })
    }
}
```

Document invariants in rustdoc. Use `TryFrom` for conversions that can fail.

### 1.4 Phantom Types and Type-Level Information
Use `std::marker::PhantomData` for compile-time distinctions or extra type info with zero runtime cost.

```rust
use std::marker::PhantomData;

pub struct Length<Unit>(f64, PhantomData<Unit>);

pub struct Meters;
pub struct Feet;

impl Length<Meters> {
    pub fn to_feet(self) -> Length<Feet> { /* ... */ }
}
```

Common in typestates, units, ID tagging, or "branded" types.

### 1.5 Sealed Traits and Controlled Extensibility
Prevent downstream crates from implementing your traits (avoids coherence/orphan issues and breaking changes).

```rust
mod private {
    pub trait Sealed {}
}

pub trait MyPublicTrait: private::Sealed {
    // ...
}

impl<T> private::Sealed for T where T: SomeBound {}

// Now only types you control (or that meet bounds) can impl MyPublicTrait
```

Use for traits that are "closed" or have specific blanket impls.

### 1.6 Marker Traits and Capability Traits
Use empty traits as markers for capabilities or categories of types.

```rust
pub unsafe trait TrustedLen: Iterator {} // Example from std (use with care)

pub trait ConfigSource {} // Marker for types that can provide config
```

Document safety invariants for `unsafe` ones.

---

## 2. Typestate Pattern: Compile-Time State Machines

**The killer feature for "bugs nearly impossible"**. Encode an object's runtime state in its *compile-time type*. Invalid operations or transitions simply do not exist in the API for that type.

**Core Idea** (from excellent explanations like cliffle.com):
- Represent states as distinct types (marker structs or generics + `PhantomData`).
- Operations that are only valid in a state are `impl` blocks for that state type.
- Transitions consume `self` (or `&mut self` for non-consuming) and return a new typed state.
- Ownership/move semantics + types enforce the protocol.

### Simple Example: File Lifecycle (RAII + Typestate)
`std::fs::File` already does a lightweight version via `drop` consuming it.

```rust
use std::marker::PhantomData;

pub struct Open;
pub struct Closed;

pub struct File<State> {
    inner: std::fs::File,
    _state: PhantomData<State>,
}

impl File<Open> {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let f = std::fs::File::open(path)?;
        Ok(Self { inner: f, _state: PhantomData })
    }

    pub fn read_to_string(&mut self, buf: &mut String) -> std::io::Result<usize> {
        // Only callable on Open
        use std::io::Read;
        self.inner.read_to_string(buf)
    }

    pub fn close(self) -> File<Closed> {
        // Transition by consuming self
        File { inner: self.inner, _state: PhantomData }
    }
}

impl File<Closed> {
    // No read_to_string here — it doesn't even exist for this type!
}
```

**Usage**:
```rust
let file = File::<Open>::open("foo.txt")?;
let mut s = String::new();
file.read_to_string(&mut s)?; // OK
let closed = file.close();
// closed.read_to_string(...); // Compile error: no method
```

### More Advanced: HTTP Response Builder (Ordered Steps)
From typestate literature — status line before headers before body.

Use either separate structs per state or generic + markers.

**Generic + Phantom version (shared code easy)**:

```rust
use std::marker::PhantomData;

pub trait ResponseState {} // Optional bound
pub struct Start; impl ResponseState for Start {}
pub struct HasStatus; impl ResponseState for HasStatus {}
pub struct HasHeaders; impl ResponseState for HasHeaders {}

pub struct HttpResponse<S: ResponseState> {
    // Shared internal state (headers vec, body, etc.)
    headers: Vec<(String, String)>,
    status: Option<(u16, String)>,
    body: Option<String>,
    _state: PhantomData<S>,
}

impl HttpResponse<Start> {
    pub fn new() -> Self { /* ... */ }

    pub fn status_line(mut self, code: u16, reason: &str) -> HttpResponse<HasStatus> {
        self.status = Some((code, reason.to_string()));
        HttpResponse {
            headers: self.headers,
            status: self.status,
            body: self.body,
            _state: PhantomData,
        }
    }
}

impl HttpResponse<HasStatus> {
    pub fn header(mut self, k: &str, v: &str) -> Self { // or &mut self variant
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn body(mut self, b: &str) -> HttpResponse<HasHeaders> {
        self.body = Some(b.to_string());
        HttpResponse { /* move fields */ _state: PhantomData }
    }
}

// Shared methods across states
impl<S: ResponseState> HttpResponse<S> {
    pub fn bytes_so_far(&self) -> usize { /* ... */ }
}
```

**Alternative (separate types per state)**: More explicit, less generic boilerplate for simple cases, but more duplication for shared logic. Use a private inner struct + `Box` or `Arc` if data is large.

**Tips for Typestate**:
- Use `&mut self` for operations that don't change state (avoids move churn).
- For state-specific data: Put it in the marker type itself (e.g., `struct HasStatus { code: u16 }`) or in the generic param.
- Seal the state traits/markers to prevent user-defined states.
- Document each state in its `impl` block — rustdoc groups methods beautifully.
- Great for: Protocol handlers, builders with prerequisites, resource acquisition/release, authentication flows ("must login before send"), parsers, etc.
- Cost: Slightly more complex API surface and potentially more monomorphized code. Use judiciously — simple enums or runtime checks suffice for many cases.
- Exemplars: `serde` internals (serializer state), various protocol libs, custom builders.

**When NOT to use**: Trivial state, or when the state space is huge/complex (then consider a state machine crate or runtime checks + good errors).

---

## 3. API Design, Ergonomics, and Interoperability

Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) religiously for naming, documentation, etc.

### Key Principles
- **Small public surface**: Hide implementation details. Use `pub(crate)` or private modules + `pub use crate::foo::Bar;` in `lib.rs` for clean re-exports.
- **Predictable and discoverable**: Methods do what their name says. Use builders for complex construction.
- **Flexible inputs**: Accept `impl AsRef<str>`, `impl Into<String>`, `impl Iterator<Item = T>`, `&[u8]`, etc. (See "Nine Rules for Elegant Rust Library APIs".)
- **Conversions**: Implement `From`/`Into`/`TryFrom`/`TryInto` generously for ergonomic use.
- **Extension traits**: Add methods to types you don't own (e.g., `trait MyExt for T: ForeignTrait { ... }`).
- **Deref**: Use *only* for smart pointers or transparent newtypes where the target is the primary interface. Overuse hides API and surprises users. Prefer inherent methods or explicit delegation.
- **Object safety vs generics**: Use `dyn Trait` when you need heterogeneous collections or runtime polymorphism. Prefer `impl Trait` or generics for performance and monomorphization. For async object-safe traits, consider the `async-trait` crate or redesign (return `Pin<Box<dyn Future>>` or use associated types carefully).
- **Fluent builders + typestate**: Combine for powerful, safe construction (see Section 2).

**Builder Example (Typestate-enhanced)**:
```rust
pub struct ThingBuilder<State> { /* fields + PhantomData<State> */ }

impl ThingBuilder<Empty> {
    pub fn new() -> Self { ... }
    pub fn with_foo(mut self, v: Foo) -> ThingBuilder<WithFoo> { ... }
}

impl ThingBuilder<WithFoo> {
    pub fn build(self) -> Thing { ... }
}
```

### Naming and Conventions
- `snake_case` for functions/variables/modules.
- `PascalCase` for types/traits.
- `SCREAMING_SNAKE_CASE` for constants/statics.
- Avoid `get_`/`set_` prefixes unless truly getters/setters (prefer `foo()` and `set_foo()` or builders).
- Use `is_`, `has_` for bool queries.
- Error types: `FooError` or specific like `InvalidPort`.

### Documentation (rustdoc)
Every public item **must** have `///` documentation with:
- What it does.
- Panics, errors, invariants, preconditions.
- Examples (that compile and run via `cargo test --doc`).
- `# Safety` sections for unsafe.

Module-level docs (`//!`) explain the crate's purpose, architecture, and how pieces fit (e.g., "This crate provides a typestate-based HTTP client...").

Use intra-doc links (`[`Foo`](struct.Foo.html)`).

---

## 4. Error Handling

**Libraries**:
- Define a dedicated error enum with `#[derive(thiserror::Error)]`.
- Specific variants for different failure modes (better than one big "Other").
- `pub type Result<T> = std::result::Result<T, MyCrateError>;`
- Use `?` everywhere. Add `.context("helpful message")` via `anyhow` or `thiserror` helpers if needed internally.
- Never swallow errors or use `unwrap()` on user-controlled paths.

**Applications / Binaries**:
- Use `anyhow::Result` (or `eyre` + `color-eyre` for beautiful reports) for simplicity and context.
- Convert library errors with `?` or `.map_err(|e| anyhow!(e))`.

**General**:
- Document every error path in rustdoc.
- For truly infallible operations: Return the value directly or use `std::convert::Infallible`.
- Prefer `Result` over `Option` when failure has meaningful information (use `Option` for "not found" or simple absence).

Example `thiserror`:
```rust
#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("privileged port {0} not allowed")]
    PrivilegedPort(u16),
    #[error("invalid host: {0}")]
    InvalidHost(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

---

## 5. Functional Programming and Compositional Design

Great Rust code often feels functional even though it's systems-oriented. Embrace composition over mutation and inheritance (Rust has neither classes nor inheritance anyway).

### 5.1 Iterators and Higher-Order Functions
Use iterator adapters extensively instead of imperative loops with mutable accumulators.

```rust
// Good
let sum: i32 = items.iter().filter(|x| x > 0).map(|x| x * 2).sum();

// Bad (when iterator equivalent exists)
let mut sum = 0;
for x in &items {
    if *x > 0 { sum += x * 2; }
}
```

`itertools` crate adds many useful adapters.

### 5.2 Immutability and Functional Updates
Prefer creating new values over mutating in place for domain logic.

```rust
let new_config = Config {
    port: 8080,
    ..old_config  // Struct update syntax
};
```

Or methods that consume `self` and return modified version.

### 5.3 Monadic / Chaining Patterns (Practical CT)
`Option` and `Result` (and `Future`, `Iterator`) form monad-like structures:

- `map` / `and_then` / `or_else` / `unwrap_or_else` etc. for chaining.
- Use for fallible pipelines without early returns everywhere.

```rust
let user = get_id()
    .and_then(fetch_user)
    .and_then(validate_user)
    .map(|u| enrich(u));
```

Consider the `tap` crate for side-effect inspection in chains without breaking fluency:
```rust
result.tap_ok(|v| log::info!("Got: {:?}", v)).map_err(|e| { log::error!(...); e })
```

### 5.4 Traits as Compositional Abstractions (CT Influence)
Design small, focused traits that compose.

**Tower's `Service` + `Layer` as Exemplar** (highly recommended study):
- `Service<Request>` is like an async function: `Request -> Future<Result<Response, Error>>`.
- `poll_ready` for backpressure (crucial for robustness under load).
- Middleware = `Service` that wraps another `Service` (decorator/composition).
- `Layer` trait turns a service into a wrapped one — enables stacking like `TimeoutLayer.layer(LoggingLayer.layer(Handler))`.
- Result: Extremely modular, testable, reusable components. Third parties can provide middleware easily.
- Why excellent: Type-safe composition of effects, backpressure propagation, separates concerns perfectly. Reflects "morphisms" (services) and "functors"/natural transformations (layers) in a practical systems setting.

Emulate this pattern for your own pipelines, servers, request handlers, data processors, etc.

Other examples:
- Bevy's `Plugin` trait + `App::add_plugin` for composable app extensions.
- Parser combinators (`nom`, `chumsky`): Small parsers compose into larger ones via functions/traits. Very FP/CT inspired (parsers as profunctors or arrows in advanced designs).
- `frunk`: Heterogeneous lists (`HList`), coproducts — generic programming inspired by category theory and Haskell's `HList`/`Either`.

### 5.5 Algebraic Structures (Light CT)
When your domain has combining operations, consider:
- `trait Semigroup { fn combine(self, other: Self) -> Self; }`
- `trait Monoid: Semigroup { fn empty() -> Self; }`
- Implement for your types or use libraries. Useful for config merging, error accumulation, aggregations, etc.

`std` has some (`Add`, `Sum` for iterators).

### 5.6 Purity and Effect Isolation
Keep core logic pure (or as pure as practical). Isolate side effects (IO, randomness, time) at the edges or via dependency injection (traits or generics).

Use `tracing` for structured observability instead of `println!`.

---

## 6. Ownership, Borrowing, Lifetimes, and Safety

- **Default to owned data** in structs and return types unless sharing is semantically required (`Arc<T>` or `Rc<T>` for shared ownership; `&T` for temporary borrows in APIs).
- **Broad input acceptance** in public APIs: `AsRef`, `Borrow`, `Into`, `AsRef<str>` + `&str` etc.
- **Lifetimes**: Elide (`'_`) when obvious. Name them meaningfully in complex signatures (`'input`, `'ctx`, `'a`). Avoid `'static` unless data truly lives forever.
- **Cow<'a, T>**: For "maybe owned" strings/slices when you sometimes need to allocate, sometimes can borrow.
- **Self-referential or complex borrowing**: Redesign the API or use crates like `ouroboros`/`self_cell` sparingly. Often a sign the model needs rethinking.
- **unsafe code**:
  - Minimize. Many excellent crates are 100% safe (`#![forbid(unsafe_code)]`).
  - When necessary: Provide a *safe* wrapper API. Document the safety invariants thoroughly with `// SAFETY: ...` comments explaining why the `unsafe` block is sound (what preconditions the caller/upstream guarantees, what the code upholds).
  - Use `debug_assert!` for runtime checks of invariants in debug builds.
  - Audit with `cargo geiger` or manual review.
- **Performance**: Zero-cost abstractions are a Rust strength — use them (generics, newtypes, iterators). Profile with `criterion`. Avoid allocations in hot loops. Use `const fn` and `const` generics where applicable. `#[inline]` sparingly (compiler is good at it).

---

## 7. Async, Concurrency, and Larger Architecture

- **Structured concurrency**: Use `tokio::spawn` with `JoinHandle` or `tokio::task::JoinSet`. Prefer scopes or `async-scoped` patterns over fire-and-forget.
- **Communication**: Message passing via channels (`tokio::sync::mpsc`, `flume`, `crossbeam`) is often safer and more composable than shared mutable state.
- **Shared state**: `Arc<parking_lot::RwLock<T>>` (faster than std in many cases), `dashmap` for concurrent maps, or lock-free structures/atomics. Minimize contention.
- **Async traits**: `async fn` in traits is object-safe only with workarounds (`async-trait` macro or manual `Pin<Box<dyn Future>>`). For middleware-style, study and emulate **Tower's `Service` trait** (see above) — it elegantly solves many async composition problems with `poll_ready` + associated `Future` type.
- **Backpressure**: Propagate it (like Tower does). Don't let unbounded queues grow.
- **ECS / Data-Oriented** (if applicable): Study Bevy's architecture — components as data, systems as (pure-ish) functions, resources, plugins for modularity. Excellent separation and performance.
- **Overall Architecture**:
  - Hexagonal/Clean/Ports-and-Adapters adapted to Rust: Traits define ports (interfaces to external), concrete impls are adapters. Generics or dependency injection via constructors.
  - Crate boundaries: Logical separation (e.g., `domain`, `application`, `infrastructure` in a workspace or modules). Keep `domain` pure.
  - Composition over inheritance: Traits + generics + wrapping (as in Tower).

---

## 8. Crate Organization, Tooling, and Maintainability

### Module/Crate Layout
```
src/
  lib.rs          # Re-exports, crate docs, top-level types
  error.rs        # Error types
  types/          # Domain newtypes, enums
  client/         # Or feature modules
    mod.rs
    builder.rs    # Typestate builders here
  internal/       # Private implementation details
```

- `lib.rs`: `pub use foo::Bar;`, `pub mod prelude { pub use ...; }` for common imports.
- Keep implementation details unexported.

### Cargo.toml
- Precise dependencies. Use features for optional functionality (e.g., `serde` derive support behind a feature flag).
- `[lints.clippy]` or workspace lints.
- MSRV (Minimum Supported Rust Version) documented and tested in CI.

### Tooling & CI (Non-Negotiable for Excellence)
- `rustfmt` (enforce in CI).
- `clippy`: `cargo clippy -- -D warnings`. Enable useful lints:
  - `clippy::all`, many from `pedantic` and `nursery` (selectively; some are noisy).
  - `missing_docs`, `rust_2018_idioms`, `unsafe_op_in_unsafe_fn`, etc.
  - In code: `#![warn(missing_docs)]` `#![deny(unsafe_code)]` (where appropriate).
- Tests: `cargo test`, including `--doc`. Add `proptest` for property-based testing of invariants (e.g., "roundtrip serialize/deserialize preserves value", "builder always produces valid state").
- `cargo audit`, `cargo udeps`, `cargo outdated`.
- GitHub Actions (or equivalent): Matrix over OS + Rust versions (stable, MSRV), clippy, fmt check, tests, doc build.
- Semver and changelogs. Use `cargo-release` or similar.

### Recommended Crates for Robust Code
- **Errors**: `thiserror` (libs), `anyhow` or `eyre` + `color-eyre` (apps).
- **Async**: `tokio`, `tower` (Service/Layer), `tracing` + `tracing-subscriber`.
- **Testing**: `proptest` / `quickcheck` (properties), `criterion` (benches), `test-case`.
- **Utilities**: `itertools`, `tap`, `derive_more`, `strum`, `once_cell` or `lazy_static`.
- **Advanced typing**: `frunk` (HList/Coproduct), `generic-array` + `typenum`.
- **FP/Combinators**: `nom` or `winnow`/`chumsky` for parsing.
- **Other**: `serde` (with derive), `parking_lot`, `dashmap`, `crossbeam`.

---

## 9. Anti-Patterns to Avoid

- Stringly-typed everything (configs, states, errors) — use enums/newtypes.
- Structs with 5+ `Option<bool>` fields that have implicit dependencies — use enum or typestate.
- `unwrap()` / `expect("todo")` in library code.
- Public mutable fields or constructors that allow invalid states.
- Overuse of `Box<dyn Trait>` when a generic parameter or enum of known types suffices.
- Ignoring `Result` / `Option` (leads to runtime panics later).
- Complex public lifetimes that could be simplified by changing ownership model.
- Mutation-heavy core logic (harder to reason about, test, parallelize).
- "God" traits or modules that do everything.
- Skipping documentation or examples.

---

## 10. Instructions Specifically for AI Coding Agents

When generating or refactoring Rust code, **internalize and apply**:

1. **First question**: "Can I redesign the types (newtype, enum, typestate, PhantomData) so that this class of bug or misuse is impossible at compile time?"
2. **State modeling**: If there's lifecycle, protocol, or ordered steps — strongly consider typestate or a well-designed enum.
3. **Construction**: Always validate in constructors. Return `Result`. Private fields.
4. **Composition**: Look for opportunities to use small traits + generics or Tower-like `Service` + `Layer` / plugin patterns for modularity and extensibility.
5. **Functional style**: Prefer iterators, chaining (`and_then`, `map`), struct updates, and pure(ish) functions. Isolate effects.
6. **Documentation**: Every public item gets rustdoc with runnable examples. Module docs explain architecture.
7. **Errors**: Specific `thiserror` enums for libs. `?` propagation. Context where helpful.
8. **Tooling**: Write code that passes `cargo fmt -- --check`, `cargo clippy -- -D warnings` (reasonable lints), and has tests (incl. proptest for key invariants). Doc tests must pass.
9. **Safety**: If `unsafe` is truly needed, justify it explicitly with invariants. Prefer safe abstractions.
10. **Ergonomics**: Accept flexible input types (`AsRef`, `Into`, iterators). Provide `From`/`TryFrom`. Use builders + typestate for complex objects.
11. **Study these**:
    - `tower` source (Service/Layer design).
    - Typestate articles (e.g., cliffle.com/blog/rust-typestate/).
    - `serde` derive and error handling.
    - Bevy plugin/ECS system for composition.
    - Parser combinators for FP style.
    - Official API Guidelines.
    - "Rust for Rustaceans" (Gjengset) for deeper ownership/trait/unsafe insight.

Following this produces code that *feels* like it was written by someone with category theory intuition and FP experience — robust, elegant, composable, and a joy to use and maintain — even when the domain is systems programming.

---

## Further Reading & Exemplars

- **Official**: [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- **Typestate**: [The Typestate Pattern in Rust](https://cliffle.com/blog/rust-typestate/) + Stanford CS242 lecture notes
- **Tower/Service**: [Inventing the Service trait](https://tokio.rs/blog/2021-05-14-inventing-the-service-trait) (and Tower docs/source)
- **Books**:
  - *Rust for Rustaceans* – Jon Gjengset
  - *Zero To Production in Rust* – Luca Palmieri (excellent app architecture, testing, errors)
  - *Programming Rust* (Blandy, Orendorff, Tindall)
- **Advanced/FP**: `frunk` crate + its docs; `nom`/`chumsky` sources; category theory applied lightly via composition patterns.
- **Unsafe**: *The Rustonomicon*
- **CT Light for Programmers**: Resources on functors/monads in Rust (Option/Result/Iterator/Future) and how traits enable compositional design. See also `rustica` crate or `ctrs` for exploration.

---

*This guide evolves with the ecosystem. Prioritize clarity, safety, and composition. The best Rust code makes the wrong thing hard or impossible, and the right thing obvious and efficient.*

**End of Style Guide**
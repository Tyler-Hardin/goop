# Leptos Style Guide for Reliable, Flicker-Free, Visually Appealing Frontends

**Purpose**: This guide helps AI agents (and humans) write high-quality Leptos code. It emphasizes **reliability** (no invalid states, no hydration mismatches), **performance** (minimal client JS/WASM, targeted updates), **visual polish** (smooth, consistent, layout-stable UIs), and **maintainability** (explicit, testable, Rust-idiomatic patterns).

Leptos (fine-grained reactive Rust web framework) shines when you embrace its reactive signals + declarative views. It has no heavy VDOM; updates are precise and efficient.

## 1. Philosophy & MVP Analogy (Updated for Leptos)

Classic **Model-View-Presenter (MVP)** separated concerns:
- **Model**: Data + business rules.
- **View**: Pure rendering.
- **Presenter**: Wires events → model updates → view refresh; handles side effects.

**In Leptos** (fine-grained reactive system):
- **Model** → Reactive state primitives + domain types:
  - Local: `signal()`, `RwSignal`, `create_memo`, `create_resource`.
  - Shared: Context-provided signals or `reactive_stores::Store` (Leptos 0.7+).
  - Global/App-level: Prefer `leptos_router` URL state (bookmarkable, shareable). Fallback to context + stores.
  - Async/Server data: Server Functions (`#[server]`) + Resources.
  - Complex flows: **Finite State Machines (FSMs)** (manual enum or `leptos-state` crate).
- **View** → Declarative `view! { ... }` macros + small, composable `#[component]` functions. Control flow via native Rust (`if`/`match`/`for`) or helpers like `<Show>`, `<Suspense>`.
- **Presenter / Orchestrator** → 
  - Event handlers (closures that call `set_*` or `send` on machines).
  - `Effect::new(...)` for reactions and client-only side effects.
  - `Action` for async mutations (provides `pending()`, `input()`, error signals automatically).
  - **FSMs as robust Presenters**: Enforce valid states/transitions explicitly. This prevents "impossible state" bugs that cause flickering, wrong UI, or hydration issues. Highly recommended for anything beyond trivial UIs (forms, wizards, async operations, modals, multi-step flows).

**Key Mindset Shift**: Reactivity does the "wiring" automatically. Your job is to keep state **explicit and exhaustive** (enums > boolean flags) and ensure **server-rendered HTML matches what the client hydrates**.

**Core Rules for Agents**:
- Prefer **local state** in components; lift only when truly shared.
- Make state machines or enums the single source of truth for complex UI.
- UI is a pure function of state (declarative `match` / derived signals).
- Side effects live in Effects/Actions/FSM actions — never in render paths.
- Always design for **isomorphic rendering** (SSR + hydration) or **islands**.

## 2. Avoiding Flickering, Hydration Mismatches & Jank (Highest Priority)

Flickering (flash of wrong/default state) and hydration warnings destroy perceived quality. Leptos hydration walks the DOM; mismatches panic or produce warnings.

### 2.1 Server ↔ Client Parity (Non-Negotiable)
The **exact same view tree** must be produced during SSR and client hydration.

**Forbidden**:
- `cfg!(target_arch = "wasm32")` (or similar) that changes rendered HTML structure or text.
- Randomness, `rand::thread_rng()`, `js_sys::Date::now()` etc. in render paths (use seeded or client-only via Effect).
- Client-only crates (gloo-*, wasm-bindgen things) called during SSR.
- Server-only crates compiled into WASM bundle.

**Solutions**:
- Guard browser-only side effects in `Effect::new(move |_| { ... })` (runs after hydration).
- Use `create_local_resource(..., on_client = true)` or similar patterns.
- Mark server-only deps as `optional = true` + `ssr` feature only in `Cargo.toml`.
- Validate HTML output (W3C validator) during refactors. Always include `<tbody>` in tables; never put block elements (`<div>`) inside `<p>`.
- For initial state that differs: Compute a **common structure** (e.g., empty vec on both sides) or use `leptos_hydrated` crate for seamless server→client state transfer.

**Debugging hydration errors**: Inspect the reported DOM element vs. your `view!` tree. Mismatches are almost always parity or invalid HTML.

### 2.2 Loading, Async & Transitions
- **Always** wrap `create_resource` reads in `<Suspense fallback={view!{ <LoadingSkeleton/> }}>` **or better** `<Transition>` (for smooth updates on refetch without full fallback flash).
- Provide **skeleton loaders** that match final layout shape (prevents Cumulative Layout Shift — CLS). Use Tailwind `animate-pulse` + placeholder blocks.
- Server-render data when possible (via server functions inside resources or islands) so first paint has real content.
- For optimistic updates: Update local signal immediately, rollback in error arm of Action/FSM.

### 2.3 Islands Architecture (Strongly Recommended Default)
Full hydration sends a large WASM bundle and hydrates *everything*. Most pages have large static regions.

**Use Islands** (`features = ["islands"]`):
- Mark interactive pieces with `#[island]`.
- Mount via `leptos::mount::hydrate_islands();`.
- Add `islands=true` to `<HydrationScripts options islands=true/>`.
- Benefits:
  - Dramatically smaller WASM (only island code ships).
  - Static HTML stays inert — zero hydration cost or mismatch risk.
  - Server-only code (fs, db, secrets) is safe in non-island `#[component]`s.
  - Faster first paint, lower memory.

**Rule**: Default to islands + server components for content-heavy pages. Use full hydration only for highly interactive SPAs (complex client routing + persistent state across navs).

### 2.4 Visual Stability & Polish Anti-Patterns
- **Layout Shift**: Always reserve space (`min-h-[X]`, `aspect-ratio`, explicit widths/heights on images/containers, or skeleton matching dimensions).
- **Images/Media**: `loading="lazy"`, proper `width`/`height` or `aspect-ratio`, server-rendered placeholders.
- **Animations/Transitions**: Use CSS `transition`, `transform`, `opacity`. Prefer `will-change` sparingly. Respect `@media (prefers-reduced-motion)`. Trigger via state changes (FSM excellent here: `Entering`, `Visible`, `Exiting` states).
- **Modals/Drawers/Popovers**: Use portals if needed, proper focus trap, escape key, backdrop click. Animate with opacity + scale/transform. Manage open state via FSM or dedicated signal + `Show`.
- **Toasts/Notifications**: Dedicated context + queue signal; FSM or simple stack for enter/exit.

## 3. State Management Patterns

### 3.1 Local State (Default)
```rust
let (count, set_count) = signal(0);
// or
let count = RwSignal::new(0);
```

Derived/computed:
```rust
let doubled = move || count.get() * 2; // or create_memo
```

### 3.2 Shared State
- **Context** (simple cross-component):
  ```rust
  provide_context(my_signal);
  // later
  let sig = use_context::<RwSignal<i32>>().expect("...");
  ```
- **Stores** (`reactive_stores` crate, Leptos 0.7+): Structured, fine-grained partial updates on fields. Derive `Store`. Provide via context. Great for complex app state.
- **URL-driven** (via `leptos_router`): Best for filters, pagination, selections, tabs that should be bookmarkable/shareable. Use `use_query` / `use_params` + signals that sync back to URL.

**Global sparingly**: Most UIs compose fine with local + occasional context. Over-use of global leads to tight coupling and harder testing.

### 3.3 Finite State Machines (FSMs) — Strongly Encouraged for Complex UI
Boolean flags (`isLoading && !hasData`) create 4+ implicit states and bugs. **Explicit enums + transitions** are reliable, self-documenting, and prevent flicker/wrong renders.

**Option A: Manual Enum (Simple, Zero-Dependency — Start Here)**
```rust
#[derive(Clone, Debug, PartialEq, Eq)]
enum EditorState {
    Idle,
    Editing { draft: String },
    Saving,
    Saved { timestamp: String },
    Error { message: String, can_retry: bool },
}

let state = RwSignal::new(EditorState::Idle);

// In event handler or Action completion:
match current {
    EditorState::Idle => { /* start editing */ state.set(EditorState::Editing { draft: "...".into() }); }
    EditorState::Editing { .. } => { /* validate & transition to Saving */ ... }
    // ...
}

// In view! (exhaustive match = compiler helps):
match state.get() {
    EditorState::Idle => view! { <button on:click=...>"Start Editing"</button> },
    EditorState::Editing { draft } => view! { <textarea prop:value=draft ... /> ... },
    EditorState::Saving => view! { <Spinner /> "Saving..." },
    EditorState::Saved { timestamp } => view! { <SuccessBanner ts=timestamp /> },
    EditorState::Error { message, can_retry } => view! { <ErrorAlert msg=message retry=can_retry /> },
}
```

Add helper methods on the enum or a `transition` fn for guards (e.g., only save if valid).

**Option B: `leptos-state` Crate (Powerful, XState-inspired)**
Add to `Cargo.toml`:
```toml
leptos-state = "0.2"
```

Example (traffic light or form flow):
```rust
use leptos_state::{MachineBuilder, use_machine};

#[derive(Clone, Debug)]
enum FormEvent { Submit, Success, Failure(String), Retry, Reset }

let machine = MachineBuilder::new()
    .state("idle")
        .on(FormEvent::Submit, "submitting")
    .state("submitting")
        .on(FormEvent::Success, "success")
        .on(FormEvent::Failure(_), "error")
    .state("success")
        .on(FormEvent::Reset, "idle")
    .state("error")
        .on(FormEvent::Retry, "submitting")
        .on(FormEvent::Reset, "idle")
    .initial("idle")
    .build();

let (current_state, send) = use_machine(machine);

// send(FormEvent::Submit) from button
// Render based on current_state.value() or match on it
```

`leptos-state` also gives stores (Zustand-like), middleware (logging, persistence), DevTools, testing helpers, and persistence. Excellent for agent-generated complex UIs. Use it when manual enum transitions become repetitive.

**FSM Benefits** (why agent should default to this pattern):
- Exhaustive states → no "what if loading + error?" flicker.
- Explicit transitions → easy to audit, test, visualize.
- Side effects (analytics, API calls) attached to transitions.
- Easy optimistic states or retry logic.
- Pairs beautifully with `Action` (drive FSM from Action's pending/error signals).

### 3.4 Async Data & Mutations
- **Queries**: `create_resource(move || deps.get(), |deps| fetch_server_fn(deps))` inside `<Suspense>` / `<Transition>`.
- **Mutations**: `create_action(|input| async move { server_fn(input).await })`. Use its `.pending()`, `.input()`, and completed signals to drive FSM or local state.
- Invalidate resources after successful mutation (or use optimistic + rollback).

## 4. Styling & Visual Appeal Guidelines

**Recommended Stack**: **Tailwind CSS** (standalone binary — no persistent Node.js). Fast iteration, consistent design system, responsive, dark mode easy.

Setup (common pattern):
- `input.css` with `@tailwind` directives + custom layers.
- Run `tailwindcss -i ./input.css -o ./style/output.css --watch` (or integrate in build).
- In `index.html` or shell: `<link rel="stylesheet" href="/style/output.css">`.
- leptosfmt + Tailwind IntelliSense in editor.

**Component Primitives** (build a small reusable set):
- `Button(variant: Primary | Secondary | Destructive | Ghost, size: Sm | Md | Lg, disabled, loading?)`
- `Input`, `Select`, `Textarea`, `Checkbox`, `Card`, `Modal`, `Toast`, `Skeleton`, `Badge`, `Alert`.
- Implement variants with Tailwind classes or a `cn` / class helper (or enum → classes fn).
- All interactive primitives should accept `disabled` / `loading` and render appropriately (often driven by FSM state).

**Design Tokens**:
- CSS custom properties for colors, spacing scale, radii, shadows, fonts.
- Dark mode via `.dark` class on `<html>` toggled by signal/context + `prefers-color-scheme`.
- Consistent scale (Tailwind's or custom).

**Polish Rules**:
- Subtle transitions on state changes (opacity, transform, colors). 150-300ms ease.
- Focus-visible styles (never remove outline without replacement).
- Accessible: Proper ARIA, labels, roles, keyboard support. Leptos `view!` passes attributes through.
- Responsive: Mobile-first. Test touch targets (min 44px).
- Loading: Never leave user staring at blank or broken layout. Skeletons > spinners where content shape is known.
- Empty states: Helpful illustrations/messages (not just "No data").
- Error states: Clear, actionable (retry button when appropriate — FSM makes this natural).

**Avoid**:
- Over-animation or janky transforms.
- Layout thrashing (read/write DOM in loops — reactivity prevents most of this).
- Deeply nested components without memoization where props are stable.

## 5. Component & Code Organization Best Practices

- **Small components**: One responsibility. Extract repeated `view!` fragments.
- **Props**: Use structs for complex props. `#[prop(optional)]`, `#[prop(into)]` etc. as needed. Children via `Children` or `ChildrenFn`.
- **Files**: Group by feature (e.g., `components/editor.rs`, `components/editor/state.rs` or FSM in separate module). Or colocate in `pages/` / `components/`.
- **Reactivity hygiene**:
  - Create signals/effects at component root (not inside `if`/`for` that re-runs).
  - Use `create_memo` or derived closures for expensive computations.
  - Effects for "reactions" (e.g., sync to localStorage, URL, analytics) — keep minimal.
- **Routing**: `leptos_router` with nested routes. Load data via resources in page components or loaders pattern.
- **Error Boundaries**: Wrap major sections; provide fallback UI. Handle in FSM where possible.
- **Formatting**: Use `leptosfmt` (config in `leptosfmt.toml` for view! macro style).

## 6. Testing Strategy (for Agent-Generated Code)

- **Pure logic**: Test state transition functions, derived signals, validation — unit tests in Rust.
- **FSMs**: Especially easy to test (enumerate events, assert final state + side effects).
- **Components**: Snapshot or render tests (limited); prefer integration via Playwright/Cypress for critical flows.
- **Hydration/SSR**: `cargo leptos build` + manual or automated checks for warnings in console. W3C HTML validation on rendered output.
- **Visual regression**: Percy/ Chromatic or simple screenshot diffs for key screens (skeletons, states).

## 7. Quick Decision Tree for the Agent

| Situation                        | Recommended Pattern                          | Why |
|----------------------------------|----------------------------------------------|-----|
| Simple counter / toggle          | Local `signal()` + bool/enum                 | Minimal |
| Form with validation + submit    | Enum/FSM (`Idle` → `Validating` → `Submitting` → `Success`/`Error`) + Action | Prevents invalid submits, clear UX |
| Multi-step wizard / onboarding   | FSM with states per step + progress         | Enforces flow, easy back/next |
| Data table (filter/sort/paginate)| Signals for params + Resource + URL sync    | Reactive, bookmarkable |
| Async operation with retry/cancel| FSM + Action integration                    | Explicit states, no flicker on error |
| Shared theme / user prefs        | Context + signal or Store                   | Fine-grained updates |
| Global filters that persist      | leptos_router query params                  | Natural, shareable |
| Mostly static marketing page     | Islands + server components                 | Tiny WASM, fast, safe server code |
| Highly interactive dashboard     | Full hydration + stores/FSM                 | When islands too fragmented |

## 8. Example Snippets (Copy-Paste Ready Patterns)

**Basic Reliable Button (Tailwind + variants)**:
```rust
#[component]
pub fn AppButton(
    #[prop(optional)] variant: Option<ButtonVariant>,
    #[prop(optional)] size: Option<ButtonSize>,
    #[prop(optional)] disabled: Option<bool>,
    #[prop(optional)] loading: Option<RwSignal<bool>>,
    children: Children,
) -> impl IntoView {
    let variant = variant.unwrap_or(ButtonVariant::Primary);
    // class computation or match
    let classes = format!("btn {} {}", variant_class(variant), size_class(size));
    
    view! {
        <button
            class=classes
            disabled=disabled.unwrap_or(false)
            on:click=move |_| { /* ... */ }
        >
            {children()}
            {move || if loading.map(|l| l.get()).unwrap_or(false) { /* spinner */ } else { ().into_view() }}
        </button>
    }
}
```

**Resource + Transition (no flicker)**:
```rust
let todos = create_resource(|| (), |_| fetch_todos());

view! {
    <Transition fallback=move || view! { <TodoSkeletonList /> }>
        {move || match todos.get() {
            None => ().into_view(), // or handled by fallback
            Some(Ok(list)) => view! { <TodoList todos=list /> }.into_view(),
            Some(Err(e)) => view! { <ErrorAlert error=e /> }.into_view(),
        }}
    </Transition>
}
```

**Simple FSM-driven Form State** (see section 3.3).

## 9. Tooling & Ecosystem Recommendations

- **Build**: `cargo-leptos` (SSR + hydration) or Trunk (CSR). Nix-friendly.
- **Formatting**: `leptosfmt`.
- **Styling**: Tailwind standalone.
- **State Machines**: `leptos-state` (or manual enums).
- **Router**: `leptos_router`.
- **Icons**: Inline SVGs or a simple component wrapping heroicons/lucide SVGs.
- **Dev**: leptos devtools / signal inspection in browser console where available; browser React DevTools not applicable — use logging in Effects during dev.
- **Testing**: `wasm-bindgen-test`, Playwright for E2E (critical for hydration/FSM flows).

## 10. Common Pitfalls to Avoid (Agent Checklist)

- Creating signals inside loops or frequently re-rendering conditionals (identity issues, though fine-grained mitigates).
- Reading resources outside Suspense/Transition (hydration warnings + flicker).
- Using `cfg!` that alters HTML.
- Implicit states via multiple bools.
- Ignoring layout stability (skeletons, reserved space).
- Heavy global state when local + props/context suffice.
- Forgetting `#[island]` on interactive bits in mostly-static pages.
- Side effects in render closures (use Effect/Action).
- Non-exhaustive matches on state enums (use `_` only intentionally, or make exhaustive).

Follow this guide and your generated Leptos frontends will be **robust, fast, beautiful, and a joy to maintain**. The combination of Rust's type system (exhaustive enums/FSMs), fine-grained reactivity, and islands/SSR gives you superpowers React/Next.js developers often fight for with extra libraries.

Update this guide as Leptos evolves (islands, stores, leptos-state, new primitives). Questions on specific patterns? Provide the UI flow and state needs — we'll design an explicit FSM + component structure together.
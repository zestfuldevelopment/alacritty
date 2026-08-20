# zestful fork of `alacritty_terminal`

## Base

Upstream `alacritty/alacritty`, tag **`alacritty_terminal_v0.26.0`** (`94e7c88`)
— the released version `zestful-terminal` depends on, not upstream `master`.

**Verified**, not assumed: `diff -rq alacritty_terminal/src` against the
crates.io `alacritty_terminal-0.26.0` source is empty. The fork base is
byte-identical to what the workspace was already building.

Our work is on branch **`zestful/apc-queue`**, pinned **by rev** in
`[patch.crates-io]`, never by branch.

## Delta

One file, `alacritty_terminal/src/term/mod.rs`, plus a `[patch.crates-io]` entry
in the workspace `Cargo.toml` pointing at the zestful `vte` fork.

- `Term::apc_payloads: Vec<Vec<u8>>` — a queue of APC payloads received since
  the last drain.
- `Term::take_apc_payloads()` and `Term::has_apc_payloads()`.
- `Handler::apc_dispatch` on `impl<T: EventListener> Handler for Term<T>`,
  pushing onto that queue.

It deliberately does **no interpretation**. `Term` does not know what the kitty
graphics protocol is; the placement rules need the cursor position at the moment
the payload arrived, which the embedder reads from this same locked `Term`.

## Why this fork exists at all

The plan originally expected `alacritty_terminal` to need **no** changes — the
zestful `vte` fork's `Handler::apc_dispatch` is defaulted, so this compiles
against it untouched. That is true for *compiling* and false for *receiving*.

The alternative was to drive `Processor::advance` against our own `Handler`
wrapper and never fork this crate. **That is not available for real panes.**
`zterm` owns the `advance` call site in only one place — `GridCore`, the
hermetic conformance half, which has no PTY. Real panes go through
`EventLoop::spawn`, and the call site is inside this crate
(`alacritty_terminal/src/event_loop.rs:154`,
`state.parser.advance(&mut **terminal, ...)`) against a `Term` behind a
`FairMutex`. Wrapping would mean replacing that 486-line event loop — mio
polling, the PTY write queue, resize, shutdown, and `MAX_LOCKED_READ` lock
fairness — to save a 5-line delta. Rejected.

Full reasoning and measurements:

> `zestful-internal/docs/terminal/plans/2026-08-20-kitty-graphics-protocol.md`

## Rebase costs

- **This fork is permanent.** Upstream alacritty will not take graphics
  (chrisduerr, 2026-07-07: *"Many of the usecases you've listed are things I
  **never** want to see in my terminal"*), so unlike the `vte` half there is no
  exit path even in principle. The delta is deliberately kept small enough that
  this does not matter much.
- **It is additive only.** One new private field, two new public methods, one
  new trait-method impl. No upstream behaviour is changed and no upstream line
  is deleted, so a rebase conflicts only if upstream edits `Term`'s field list
  or its `Handler` impl block near these lines.
- **It depends on the `vte` fork.** The `[patch.crates-io]` entry must be
  re-pointed whenever the `vte` fork's rev moves.
- All **181** upstream tests pass unmodified (135 + 45 + 1).

## Tests

Three added in `term::tests`, covering the whole path
`Processor::advance` → forked `vte` → `Handler::apc_dispatch` → `Term`'s queue:
payload delivery and drain semantics, text around an APC landing on the grid
undisturbed, and `m=1` chunked payloads queueing in order.

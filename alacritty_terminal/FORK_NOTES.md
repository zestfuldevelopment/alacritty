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

- `pub struct ApcAnchor { point, display_offset, history_size }` — where an APC
  arrived in the grid.
- `Term::apc_payloads: Vec<(ApcAnchor, Vec<u8>)>` — APC payloads received since
  the last drain, each with its anchor.
- `Term::take_apc_payloads()` and `Term::has_apc_payloads()`.
- `Handler::apc_dispatch` on `impl<T: EventListener> Handler for Term<T>`,
  capturing the anchor and pushing onto that queue.

It deliberately does **no interpretation** of the payload: `Term` does not know
what the kitty graphics protocol is.

It does, however, have to capture the **anchor**, and that is not a convenience.
The protocol places an image at the cursor position when the final chunk
arrives, and **that position does not survive the call**: anything following the
escape in the same `read()` is parsed before the embedder regains control, so by
drain time the cursor has moved. A shell prompt printed after an image is the
ordinary case, not a corner one. Capturing at dispatch is the only correct
option available.

`history_size` is captured for the same reason one step out: following bytes can
*scroll* the grid, not merely advance the cursor, which makes `point.line` stale
exactly as the column was. Its delta at drain time is how far the anchored row
has moved. **The correction saturates** — `history_size` is
`total_lines - screen_lines` and stops growing once scrollback is full, and this
crate keeps no monotonic count of lines ever scrolled, so an image anchored
shortly before a scroll burst on a full buffer cannot be located from these
fields alone. A row scrolled out of scrollback is gone regardless; the residual
is narrow, and it is stated rather than hidden.

## Why this fork exists at all

The plan originally expected `alacritty_terminal` to need **no** changes — the
zestful `vte` fork's `Handler::apc_dispatch` is defaulted, so this compiles
against it untouched. That is true for *compiling* and false for *receiving*.

The alternative was to drive `Processor::advance` against our own `Handler`
wrapper and never fork this crate. **That is not available to an embedder using
this crate's event loop.**
An embedder owns the `advance` call site only where it drives a `Processor`
itself; a terminal that uses this crate's `EventLoop` does not. Real panes go
through `EventLoop::spawn`, and the call site is inside this crate
(`alacritty_terminal/src/event_loop.rs:154`,
`state.parser.advance(&mut **terminal, ...)`) against a `Term` behind a
`FairMutex`. Wrapping would mean replacing that 486-line event loop — mio
polling, the PTY write queue, resize, shutdown, and `MAX_LOCKED_READ` lock
fairness — to save a 5-line delta. Rejected.

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
  re-pointed whenever the `vte` fork's rev moves. That fork's `FORK_NOTES.md`
  documents its own delta and deliberate divergences.
- All **181** upstream tests pass unmodified (135 + 45 + 1).

## Tests

Three added in `term::tests`, covering the whole path
`Processor::advance` → forked `vte` → `Handler::apc_dispatch` → `Term`'s queue:
payload delivery and drain semantics, text around an APC landing on the grid
undisturbed, and `m=1` chunked payloads queueing in order.

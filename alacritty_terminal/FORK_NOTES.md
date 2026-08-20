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

- `pub struct ApcAnchor { point, absolute_line, display_offset }` — where an APC
  arrived in the grid.
- `Grid::scrolled_off() -> u64` and a `scrolled_off` field, counting lines ever
  scrolled off the top. Monotonic.
- `pub enum GraphicsEvent { Apc, ClearScreen, Reset, Resize }` — everything the
  graphics layer must know happened, **in stream order**.
- `Term::graphics_events: Vec<GraphicsEvent>`,
  `Term::take_graphics_events()`, `Term::has_graphics_events()`.
- `Handler::apc_dispatch` on `impl<T: EventListener> Handler for Term<T>`,
  capturing the anchor and pushing an `Apc` event; `clear_screen`, `reset_state`
  and `Term::resize` push theirs.

### Why one queue and not several

An image, an erase and a second image arrive in a single `read()` —
`<image> ESC[2J <image>` is ordinary output. Parallel queues drained together
would report "two images, one clear" with no way to say which image the clear
applies to, which is the only question the consumer has. Same shape as the
anchor: information that exists at one moment, discarded before anyone can use
it.

### Why the erase mode is carried

`ClearScreen` carries `ansi::ClearMode` rather than collapsing to "erased". The
graphics protocol clears images on `ED 2` and **not** on `ED 0` or `ED 1`, so a
consumer given only "the screen was erased" cannot implement that rule — and a
test of the rule could not fail. `EL`, `DCH` and `ECH` produce no event at all,
which is pinned by its own test.

It deliberately does **no interpretation** of the payload: `Term` does not know
what the kitty graphics protocol is.

### Why the anchor is captured here, and why it is absolute

The protocol places an image at the cursor position when the final chunk
arrives, and **that position does not survive the call.** Anything following the
escape in the same `read()` is parsed before the embedder regains control, so by
drain time the cursor has moved. A shell prompt printed after an image is the
ordinary case, not a corner one — measured, `"AB<APC>CDEFGH"` leaves the cursor
six columns past where the image belongs.

Following bytes can also *scroll* the grid, which makes the line stale in the
same way the column was. Two candidates were measured and rejected before
`scrolled_off`:

- **`display_offset` does not work.** `Grid::scroll_up` updates it only when the
  viewport is *not* pinned to the active area. At the live edge, where output
  normally arrives, it never moves however much scrolls past — so it would look
  correct in a test that scrolls back first and be wrong in the common case.
- **A `history_size` delta does not work either.** `history_size` is
  `total_lines - screen_lines`, so it stops growing once scrollback is full and
  then under-reports. There is a test pinning exactly this.

`scrolled_off` counts lines leaving the screen whether or not scrollback had
room to keep them, so `scrolled_off + line` is a stable coordinate. It is
exposed as an **absolute anchor rather than a counter to subtract**: the
position can only be answered correctly at dispatch, so it is answered once,
there. To locate the row later, subtract `Grid::scrolled_off()` as it is *then*.

### Three cases where an anchor does not mean what it looks like

Named because a silent limitation here is expensive, and this seam has produced
two already:

- **Alternate screen.** The alt grid keeps no history, so lines scrolling off
  are discarded rather than accumulated. The counter still advances, so anchors
  stay valid *within* a screen — but each screen has its own grid and its own
  counter, so an anchor taken on one is meaningless against the other. Track
  placements per screen.
- **`clear_scrollback`.** Rows still on screen are unaffected, since nothing
  scrolled. Rows that were in scrollback are gone and their anchors name
  nothing.
- **RIS.** Resets the grid but **not** the counter, which is never reset at all.
  Prior anchors are meaningless afterwards and must be discarded by the
  consumer; RIS clears every image anyway. Do not test `scrolled_off() == 0` to
  detect staleness — it never returns to zero. Not resetting is deliberate: a
  counter that rewound would reuse row numbers, so a stale anchor could resolve
  to a *plausible* wrong row. Never reusing a value means it resolves far
  outside the grid instead, where it is obviously invalid.

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
- All **188** upstream tests pass unmodified, including the serialised ref-test
  fixtures: `scrolled_off` carries `#[serde(default)]` precisely so those
  fixtures, which predate it, still deserialise without being regenerated.

## Tests

Ten added in `term::tests`, covering the whole path `Processor::advance` →
forked `vte` → `Handler::apc_dispatch` → `Term`'s queue: payload delivery and
drain semantics, text around an APC landing on the grid undisturbed, `m=1`
chunked payloads queueing in order, per-image anchoring with two images in one
read, and the absolute anchor surviving both ordinary scrolling and a saturated
scrollback. The anchor tests are mutation-checked — pinning the anchor to the
origin, and stopping the counter, each turn them red.

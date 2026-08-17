# How the froklog-watch overlay works on Wayland

*A spec of everything it took to get a click-through, always-on-top DPS meter
rendering over a fullscreen game on Wayland — written for the Slint
conversion so none of it has to be rediscovered. Every rule here was earned
by a hang, a freeze, or an invisible window. Reference implementation:
`src/overlay.rs` in this repo.*

Environment this was proven on: Pop!_OS COSMIC (wlroots-style layer-shell),
NVIDIA proprietary driver + AMD iGPU, game running fullscreen under
XWayland, client stack = layershellev 0.19 (zwlr-layer-shell-v1) +
egui-wgpu 0.29 (Vulkan). The *protocol-level* rules apply to any toolkit;
the wgpu/egui rules apply to anything that owns its own render loop.

---

## 1. A normal window cannot do this — you need layer-shell

A plain Wayland toplevel **cannot** be placed above a fullscreen window,
cannot position itself, and cannot be click-through. There is no
"always on top" request in core Wayland; the compositor owns stacking.

The only sanctioned path is `zwlr_layer_shell_v1` with `Layer::Overlay`,
which stacks above fullscreen surfaces — including an XWayland game.
Consequences:

- **KDE, COSMIC, Hyprland, sway, wlroots-anything: works.**
- **GNOME does not implement layer-shell.** Detect the failed bind and fall
  back — ours falls back to an X11 always-on-top window (an egui viewport
  via XWayland). Don't try to fake it on GNOME; there is no way.
- A layer surface has **no decorations, no toplevel semantics** — you draw
  everything, including your own drag handle.

Slint note: Slint's winit backend creates toplevels. The overlay will need a
custom platform backend (or a separate layer-shell host process) that hands
Slint a layer surface to render into. This document is the contract that
host has to satisfy.

## 2. Click-through = the input region, nothing else

The compositor hit-tests only the surface's **input region**
(`wl_surface.set_input_region`). Alpha does not matter; a fully transparent
pixel still eats clicks if it's inside the region.

- **Unlocked**: set the region to exactly the painted panel rect. The
  transparent rest of the surface never blocks the game.
- **Locked (click-through)**: set an **empty** region — every click reaches
  the game, even through the meter's pixels.
- `wl_region.add()` can be called repeatedly to build a **union of rects** —
  that's how multiple virtual windows on one surface each stay clickable
  (see §9).
- Cache what you last applied (`applied_region` in `apply_input_region`,
  overlay.rs) and skip the request when unchanged — the region is
  double-buffered state, committed with the surface, and re-applying every
  frame is pointless protocol traffic.
- **Keyboard interactivity: `None`, always.** The game must never lose a
  keystroke to the overlay. All text input happens in the main app window.

## 3. NVIDIA deadlock: create the Vulkan instance FIRST (`preflight_instance`)

The single worst bug. Once the process holds **both** a GLX context (the
main egui window runs on eframe/glow, which is GLX under XWayland) **and** a
Wayland connection, a subsequent `vkCreateInstance` on the NVIDIA driver
**deadlocks forever**. No error, no timeout — the thread just never returns.

Rules:

- Create one shared `wgpu::Instance` (**Vulkan backend only** — do not let
  wgpu probe GL; the probe itself can wedge) **at process start, before any
  event loop or Wayland connection exists**. `preflight_instance()` in
  overlay.rs.
- Every overlay spawn reuses that instance. The instance is cheap to share;
  it's devices that cost (see §8).
- If your architecture has no GL anywhere (pure-Vulkan Slint), the trap may
  not fire — but preflighting costs nothing and immunizes you against any
  library that drags GL in later.

## 4. GPU init on its own thread, never in the Wayland event callback

Creating the wgpu surface/adapter/device from inside the layer-shell event
callback deadlocks in Wayland WSI: device creation performs a protocol
round-trip on the same connection whose queue you are currently blocking.

Pattern (overlay.rs `Gpu::new` + the `gpu_rx` channel):

1. Event callback receives the first configure → it has the `wl_surface`
   and real size.
2. Hand those to a **separate thread** that builds surface + adapter +
   device + queue.
3. The result comes back over a channel; the event loop polls `gpu_rx` and
   only starts rendering once the GPU exists.
4. On teardown, **drain the channel first** (a device that arrives after
   the surface died must still be dropped cleanly).

## 5. The renderer must see every texture delta — even for hidden frames

egui ships font-atlas (and image) updates as **texture deltas attached to a
frame's output**. If you skip presenting a frame but drop its deltas, every
later frame panics or renders garbage ("update a texture that has not been
allocated yet") — the atlas the paint jobs reference was never uploaded.

Rules:

- Never run egui before the renderer exists (§4 ordering).
- Apply `textures_delta` **every frame you run egui, including frames you
  don't present** (hidden overlay, empty scene). The `finish_frame` path in
  overlay.rs does this unconditionally.

## 6. Surfaces die on suspend/VT-switch — retry, don't crash, and LOG it

After machine suspend or a compositor hiccup, `surface.get_current_texture()`
starts failing (`Outdated`/`Lost`/`Timeout`). This is routine, not fatal:

- On failure: `surface.configure()` again with the current size, then retry
  the acquire once. Usually recovers immediately.
- If it still fails, skip the frame and try next tick. **Do not tear down.**
- **Always-on counters, not debug-only logs.** We had an overlay frozen for
  9½ hours showing stale data and no way to know why, because the failure
  path only logged under a debug env var. Now `SURFACE_FAILS` (AtomicU64)
  logs the first failure and every 100th, plus a recovery line with the
  total. Whatever the toolkit, keep an unconditional trace of
  acquire-failure streaks.
- `FROKLOG_METER_DEBUG=1` turns on the verbose trace.

## 7. Never respawn a layer surface for resize; resize in place

Layer surfaces resize in place: request the new size, handle the configure.
Tearing down and respawning the surface for a size change causes overlapping
teardown/init cycles that race and can take the process down (and you lose
§4's ordering guarantees). Respawn is reserved for **output change only**
(see §8), as a single controlled teardown — `Quit`, drain `gpu_rx`, then
rebuild.

## 8. Outputs: bound for life, and each Vulkan device costs ~140 MB

- A layer surface is **bound to one output at creation, for life**. Moving
  an overlay to another monitor = controlled respawn on the new output.
  Persist an explicit output name per overlay (`meter_output`,
  `msg_output`); an empty setting means "wherever the pointer was", which
  turns into "wrong monitor and no way to grab it" — always pin.
- Request the adapter with `PowerPreference::LowPower`: on this hybrid box
  that selects the AMD iGPU (RADV) instead of the NVIDIA dGPU — the right
  choice for a UI overlay, and it keeps the overlay working when the NVIDIA
  userspace/kernel mismatch after driver updates breaks Vulkan on the dGPU.
- Measured cost: **each wgpu device ≈ 140 MB GPU-addressable memory (GTT)**,
  almost all per-device driver overhead, not pixels. Two overlays = two
  devices = ~280 MB. This is the number that justifies the single-canvas
  design in §9 — N virtual windows on one device instead of N devices.

## 9. Recommended architecture for the Slint port: ONE canvas per output

What we'd build next (designed, not yet built here — the egui client keeps
two surfaces): one full-screen `Layer::Overlay` surface per output that has
at least one overlay window on it, hosting meter/messages/alerts/history as
**virtual windows** drawn by the toolkit:

- One surface, one device, one ticker — fixes §8's per-device cost.
- Input region = **union of rects** of every visible, unlocked virtual
  window (§2's multi-`add`). Locked windows are simply excluded from the
  union.
- The surface never moves or resizes (full-screen, anchored all four
  edges), which **deletes the drag problem in §10 entirely** — dragging a
  virtual window is plain in-toolkit widget dragging.
- Existing per-overlay x/y positions are already surface coordinates at the
  output origin, so they migrate 1:1.

## 10. If a surface moves itself: pointer coords do NOT follow (COSMIC)

Only relevant if you keep per-window surfaces that drag by moving the
surface (margins). COSMIC does **not** rebase surface-local pointer
coordinates when the surface repositions itself mid-drag — naive
"move by pointer delta" feedback-loops into the corner. The fix in
overlay.rs is absolute-anchor dragging: record the pointer's absolute
position and the surface's margin at press, move by total delta from that
anchor, with a settle window (`drag_settle_until`) and a 4 px
click-vs-drag threshold that replays real clicks. It works, it's ~150 lines
of subtle state, and §9 makes all of it deletable.

**The same bug reaches X11 apps through XWayland, disguised.** During a
button-held grab COSMIC keeps delivering surface-local coordinates in the
frame frozen at press (the non-rebasing behavior above), and XWayland
synthesizes each X11 "root" pointer coordinate as *current window position
+ that frozen local* — so an XWayland client that moves its own window
while polling root coordinates feeds every move back into the next reading
and accelerates away exponentially (observed live: pointer deltas +2, +8,
+14, +23, +36… in lockstep with the window's own motion; the user saw the
overlay "fly across all 3 screens" from a fraction-of-an-inch drag).
Querying the pointer **relative to the dragged window** subtracts the
current position right back out and recovers the stable frozen frame. But a
*real* X server rebases window-relative readings after each move — opposite
regime, opposite math — so a universal drag probes on its first move:
issue it, read again next tick; if the reading dropped by about the move,
you're on a rebasing server (move by remaining delta each tick), otherwise
a frozen frame (total-delta-from-anchor). Implemented in Ryan's client as
`overlay_shell::begin_drag`, verified live on COSMIC (frozen) with the
rebasing path reasoned from real-X11 semantics.

## 11. Render pacing: adaptive tick

No frame callbacks drive this surface — a ticker thread does. Adaptive
rate: ~5 Hz at rest, 60 Hz only while something animates (message fly-ins).
An overlay at a fixed 60 Hz is a space heater; at a fixed 5 Hz the
animations judder. The ticker asks the content "are you animating?" each
tick and adjusts.

## 12. Cheat sheet

| Symptom | Cause | Fix |
|---|---|---|
| Overlay never appears, thread hung | NVIDIA vkCreateInstance after GLX+Wayland | §3 preflight instance at startup |
| Hang on first configure | GPU init inside event callback | §4 init thread + channel |
| Panic "texture not allocated" after hidden frames | dropped texture deltas | §5 apply every frame |
| Frozen overlay after suspend, no errors | surface acquire failing silently | §6 retry + always-on counters |
| Random crash on resize | surface respawn races | §7 resize in place |
| Overlay on wrong monitor, can't drag it | unpinned output | §8 explicit output per overlay |
| Clicks pass through visible pixels / blocked on empty air | input region ≠ painted rect | §2 |
| Game loses keystrokes | keyboard interactivity not None | §2 |
| Drag flies to a corner | compositor doesn't rebase pointer coords | §10 (or §9 and delete it) |
| Works everywhere but GNOME | GNOME has no layer-shell | §1 X11 fallback |

# Yapfast IOSurface frames on macOS

This is softbuffer 0.4.8 from crates.io under its original MIT or Apache-2.0
licence (LICENSE-MIT, LICENSE-APACHE). Slint's Skia renderer draws Yapfast's
window into it when it renders on the CPU, as it does by default.

Its Core Graphics backend allocated a new zeroed buffer for every frame and
handed it to the layer as a `CGImage`, which Core Animation copied once more.
The window's pixels lived three times over: the image the layer held, Core
Animation's copy, and freed frames that macOS's allocator keeps cached.

Changes, marked `Yapfast:` in `src/backends/cg.rs`:

- Skia draws straight into up to three IOSurfaces of the window's size, and
  the layer shows one of them without a copy. A new frame takes a surface
  the window server no longer reads, never the one shown, so a frame is
  never drawn while it is on screen; of those, the one shown most recently,
  so Slint redraws what changed over the fewest presents. Two surfaces
  alternate; the third, needed only when presents come close together, is
  drawn whole once it falls more presents behind than Slint remembers what
  changed. Taking turns instead kept every surface three presents behind,
  and each frame redrew the union of three presents' changes.
- `age()` reports how many presents ago a surface was shown, so Slint
  redraws only what changed since then instead of the whole window.
- Once the window has drawn nothing for a second, the surfaces the layer
  doesn't show are marked purgeable; macOS may take them back under memory
  pressure, and the next frame drawn into one is drawn whole. The footprint
  counts them as reclaimable. Marked at every frame, they were taken back
  while a GIF played, and every few frames drew the whole window. A surface
  that falls more presents behind than Slint remembers is marked purgeable
  at once, even while the window animates: Slint draws it whole anyway.
- A row may be longer than the window is wide, for IOSurface's alignment;
  Slint's renderer takes the row length from the buffer's length.
- The surfaces are tagged sRGB, the space Slint's Skia renders in, where the
  `CGImage` was device RGB.

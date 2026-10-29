# Liquid glass demo

A complete demo of this fork's backdrop layers (`Styled::backdrop`): draggable "Liquid Glass" panels — a navigation bar, a dock, a lens, a control card and a performance card — over an animated scene. Panels squash and stretch while dragged. The control card adjusts blur, refraction, magnification, tint and rim light, switches between light and dark themes, and switches the glass renderer.

```sh
cargo run -p liquid_glass --release
LIQUID_GLASS=cpu cargo run -p liquid_glass --release   # start on the CPU renderer
```

The demo crate is built at `opt-level = 3` even in dev builds (`[package.metadata.profile.dev]` in its `Cargo.toml`), because the CPU renderer shades every glass pixel each frame and drops to a few fps unoptimized. `--release` is optional.

## Two renderers

- **Metal** (default on macOS and iOS): each panel is a div with `.backdrop(blur, BackdropEffect::LiquidGlass(..))`. The GPU blurs and refracts everything painted behind the glass, including text and other panels, at almost no CPU cost.
- **CPU** (every other platform, and for comparison): without backdrop layers GPUI has no backdrop filter, user shaders or render-to-texture, so the glass is rendered on the CPU from the scene description and uploaded as an image every frame. It only refracts the scene shapes, not text or other panels.

Both draw the same optics. If you change them, change `render_glass` in `src/glass.rs` and `liquid_glass` in `crates/gpui_apple/src/metal_renderer/backdrop.metal` together.

## Layout

| File | What it does |
| --- | --- |
| `src/main.rs` | The `Demo` view: panels, drag handling with squash and stretch, the control card, and the window. The empty backdrop doubles as the title bar for moving the window. |
| `src/glass.rs` | `LiquidGlass`, a wrapper around a `Div` that lays out like any element. On Metal it sets `Styled::backdrop`; on the CPU it rasterizes the region behind it, box-blurs it and shades every pixel on a 2-thread rayon pool, then paints the result as an image. |
| `src/backdrop.rs` | The animated scene as data (a gradient and rounded shapes), so the CPU renderer can rasterize just the region under a panel. |
| `src/theme.rs` | Light and dark palettes; the dark mode switch crossfades between them. |
| `src/perf.rs` | The performance card: frame times, glass cost, process CPU, memory and threads. |
| `src/assets.rs` | Icons compiled into the binary (Lucide, ISC license in `assets/icons/LICENSE-lucide`). |

macOS throttles windows that are covered or unfocused, and stops frame ticks while the screen is locked, so bring the window to the front before reading the performance card.

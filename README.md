# GPUI (standalone fork)

> [!NOTE]
> This is an **unofficial fork** of [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), the UI framework built by Zed Industries for the [Zed](https://zed.dev) editor. It is not affiliated with or endorsed by Zed Industries.
>
> The `gpui` / `gpui_*` crates and the internal Zed crates they depend on were extracted from [zed-industries/zed](https://github.com/zed-industries/zed) **with their full git history**, so upstream changes can keep being merged. Code is unchanged from upstream except where noted in the git log; compare any release against its upstream base with `git diff zed-<tag> <tag>`.

## About this fork

### Branches

The channel branches follow Zed's release channels; the fork's own features flow downstream from `nightly`:

| Branch         | Tracks                                    | Use it for                                                    |
| -------------- | ----------------------------------------- | ------------------------------------------------------------- |
| `main`         | newest Zed stable release (`vX.Y.Z`)      | apps — the default                                            |
| `preview`      | newest Zed preview release (`vX.Y.Z-pre`) | trying the next release early; fork patches land here first   |
| `nightly`      | Zed `main`                                | catching upstream breakage early; may be unstable             |
| `experimental` | `nightly` + the fork's features           | developing [liquid glass](#liquid-glass) and other features   |
| `liquid-glass` | `experimental`, promoted by hand          | the stable line of liquid glass — use this to build on it     |

`nightly`, `preview` and `experimental` are synced automatically every day and `main` every three days, each only after it builds and its tests pass.

### Tags

| Tag                            | Meaning                                                                     |
| ------------------------------ | --------------------------------------------------------------------------- |
| `zed-vX.Y.Z`, `zed-vX.Y.Z-pre` | Zed's own release, filtered to the crates in this repo. Pure upstream code. |
| `vX.Y.Z`, `vX.Y.Z-pre`         | Fork release built on that Zed release, checked and tested before tagging.  |
| `vX.Y.Z-fork.N`                | Later fork-only fixes on the same Zed base.                                 |

If two `zed-*` tags point at the same commit, nothing in GPUI changed between those Zed releases.

### Using it

Pin a fork release tag:

```toml
[dependencies]
gpui = { git = "https://github.com/dy0ne/gpui-fork", tag = "v1.24.3" }
gpui_platform = { git = "https://github.com/dy0ne/gpui-fork", tag = "v1.24.3", features = ["font-kit", "wayland", "x11"] }
```

or a local checkout: `gpui = { path = "../gpui-fork/crates/gpui" }`.

Both crates come from the same repository on purpose: Cargo clones a git dependency once per URL + `tag`/`rev`/`branch` and finds each package in it by name, so `gpui_platform` resolves to the same checkout and links against that same `gpui`. **Always use the same `tag` for every crate from this repo.** Mixing references (e.g. `tag = "v1.24.3"` for `gpui` and `branch = "main"` for `gpui_platform`) checks the repo out twice and builds two copies of `gpui`, which fails with errors like ``expected `gpui::App`, found `gpui::App` ``.

In a workspace, declare them once so the tag lives in a single place:

```toml
# your workspace's root Cargo.toml
[workspace.dependencies]
gpui = { git = "https://github.com/dy0ne/gpui-fork", tag = "v1.24.3" }
gpui_platform = { git = "https://github.com/dy0ne/gpui-fork", tag = "v1.24.3", features = ["font-kit", "wayland", "x11"] }

# each member crate's Cargo.toml
[dependencies]
gpui.workspace = true
gpui_platform.workspace = true
```

Cargo applies `[patch.crates-io]` only from the top-level workspace. This repo patches several crates with Zed's forks (`async-task`, `calloop`, `async-process`, `khronos-egl`, …), so **copy the `[patch.crates-io]` section of this repo's `Cargo.toml`, at the tag you depend on, into your own workspace's `Cargo.toml`**. Without it you get the crates.io versions, which may fail to compile or behave differently. To match upstream builds exactly, also copy the `rustflags` and `[env]` from `.cargo/config.toml`.

### Liquid glass

On `experimental` and `liquid-glass`, GPUI has backdrop layers. `Styled::backdrop(blur, effect)` takes everything painted behind an element, blurs it and draws it back inside the element's rounded corners through a `BackdropEffect`, so the glass refracts text, images and other glass too:

```rust
use gpui::{BackdropEffect, LiquidGlass, div, hsla, prelude::*, px};

div()
    .size(px(200.))
    .rounded(px(28.))
    .backdrop(
        px(6.), // Gaussian blur sigma
        BackdropEffect::LiquidGlass(LiquidGlass {
            refraction: px(11.), // how far outside its edge the rim shows the backdrop from
            rim: px(22.),        // width of the curved rim, capped at the corner radius
            magnify: 1.,
            aberration: 0.08,    // color fringes at the rim
            tint: hsla(0., 0., 1., 0.08),
            rim_tone: 0.5,       // rim light: 1 white, 0 none, -1 black
        }),
    )
```

`BackdropEffect::Frost` is plain frosted glass. A layer sees only what was painted before it, so give it a translucent background and put its content in children, which paint on top. Only the Metal renderer (macOS and iOS) draws backdrop layers; on Linux, Windows and the web they are skipped and the element shows just its background.

[`examples/liquid_glass`](examples/liquid_glass) is a complete demo: draggable glass panels (navigation bar, dock, lens, control and stats cards) that squash and stretch while dragged, over an animated scene, with live controls for blur, refraction, magnification, tint and rim light, light and dark themes, and a performance panel. It has two renderers, switchable from the control card: the Metal backdrop layer, and a CPU renderer that shades the glass per pixel and works on every platform.

```sh
cargo run -p liquid_glass --release
LIQUID_GLASS=cpu cargo run -p liquid_glass --release   # start on the CPU renderer
```

The rest of this README is GPUI's own documentation (`crates/gpui/README.md`), with the install snippet and links adjusted for this repo.

---

# Welcome to GPUI!

GPUI is a hybrid immediate and retained mode, GPU accelerated, UI framework
for Rust, designed to support a wide variety of applications.

## Getting Started

GPUI is still in active development as we work on the Zed code editor, and is still pre-1.0. There will often be breaking changes between versions. You'll also need to use the latest version of stable Rust. Add `gpui`, and optionally `gpui_platform`, to your `Cargo.toml` (see [Using it](#using-it) above for this fork):

```toml
gpui = { version = "*" }
gpui_platform = { version = "*", features = ["font-kit", "wayland", "x11"] }
```

Everything in a standalone GPUI app starts with an `Application`. You can create one with `gpui_platform::application()`, which picks the windowing and text backends for the host OS, and kick off your application by passing a callback to `Application::run()`. Inside this callback, you can create a new window with `App::open_window()` and register your first root view.

```rust,no_run
use gpui::*;

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        // ..
    });
}
```

### `gpui_platform`

The features on `gpui_platform` are platform-specific, so the list above is a safe cross-platform default. If you build for a single platform, you can trim it:

- **macOS** — Rendering uses Metal and is always available, but glyph rasterization needs `font-kit`. Without it, GPUI falls back to a placeholder text system that lays text out but renders no glyphs.

  ```toml
  gpui_platform = { version = "*", features = ["font-kit"] }
  ```

- **Linux / FreeBSD** — enable at least one windowing backend for desktop windows: `wayland`, `x11`, or both. These features also compile the renderer and text system, so no separate text feature is needed.

  ```toml
  gpui_platform = { version = "*", features = ["wayland", "x11"] }
  ```

- **Windows** — no features are required. Windowing uses Win32 and text uses DirectWrite. `font-kit` has no effect here.

### Additional Topics

- [Ownership and data flow](crates/gpui/src/_ownership_and_data_flow.rs)
- [Accessibility](crates/gpui/src/_accessibility.rs)
- [Examples](crates/gpui/examples) — run one with `cargo run -p gpui --example hello_world`

### Dependencies

GPUI has various system dependencies that it needs in order to work.

#### macOS

On macOS, GPUI uses Metal for rendering. In order to use Metal, you need to do the following:

- Install [Xcode](https://apps.apple.com/us/app/xcode/id497799835?mt=12) from the macOS App Store, or from the [Apple Developer](https://developer.apple.com/download/all/) website. Note this requires a developer account.

> Ensure you launch Xcode after installing, and install the macOS components, which is the default option.

- Install [Xcode command line tools](https://developer.apple.com/xcode/resources/)

  ```sh
  xcode-select --install
  ```

- Ensure that the Xcode command line tools are using your newly installed copy of Xcode:

  ```sh
  sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
  ```

## The Big Picture

GPUI offers three different [registers](<https://en.wikipedia.org/wiki/Register_(sociolinguistics)>) depending on your needs:

- State management and communication with `Entity`'s. Whenever you need to store application state that communicates between different parts of your application, you'll want to use GPUI's entities. Entities are owned by GPUI and are only accessible through an owned smart pointer similar to an `Rc`. See the `app::context` module for more information.

- High level, declarative UI with views. All UI in GPUI starts with a view. A view is simply an `Entity` that can be rendered, by implementing the `Render` trait. At the start of each frame, GPUI will call this render method on the root view of a given window. Views build a tree of `elements`, lay them out and style them with a tailwind-style API, and then give them to GPUI to turn into pixels. See the `div` element for an all purpose swiss-army knife of rendering.

- Low level, imperative UI with Elements. Elements are the building blocks of UI in GPUI, and they provide a nice wrapper around an imperative API that provides as much flexibility and control as you need. Elements have total control over how they and their child elements are rendered and can be used for making efficient views into large lists, implement custom layouting for a code editor, and anything else you can think of. See the `element` module for more information.

Each of these registers has one or more corresponding contexts that can be accessed from all GPUI services. This context is your main interface to GPUI, and is used extensively throughout the framework.

## Other Resources

In addition to the systems above, GPUI provides a range of smaller services that are useful for building complex applications:

- Actions are user-defined structs that are used for converting keystrokes into logical operations in your UI. Use this for implementing keyboard shortcuts, such as cmd-q. See the `action` module for more information.

- Platform services, such as `quit the app` or `open a URL` are available as methods on the `app::App`.

- An async executor that is integrated with the platform's event loop. See the `executor` module for more information.

- The `[gpui::test]` macro provides a convenient way to write tests for your GPUI applications. Tests also have their own kind of context, a `TestAppContext` which provides ways of simulating common platform input. See `app::test_context` and `test` modules for more details.

Currently, the best way to learn about these APIs is to read the Zed source code or drop a question in the [Zed Discord](https://zed.dev/community-links). Please report issues specific to this fork here, not to the Zed project.

## License

Apache-2.0, same as upstream — see [LICENSE-APACHE](LICENSE-APACHE). Copyright Zed Industries, Inc. and contributors; fork modifications are noted in the git history. Bundled fonts in `assets/fonts/` are under the SIL Open Font License (see the license file in each font directory).

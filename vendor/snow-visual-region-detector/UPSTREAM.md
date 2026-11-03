# Snow Shot visual region detector

Source: https://github.com/mg-chao/snow-apps

Pinned revision: `fc8f669b583cb889b8b20057013a02e8d41aa07f`

Original directory: `snow-crates/crates/visual-region-detector/src/`

Copyright belongs to the Snow Apps contributors. Distributed under
GPL-3.0-or-later; the upstream license is preserved in `LICENSE`.

ClipFerry integration changes (2026-10-23): standalone Cargo manifest,
default pure Rust backend with file codecs disabled, and removal of three
`snow_core::qos::apply_current_thread()` calls to avoid importing Snow Apps'
unrelated runtime. Detection algorithms otherwise remain upstream source.
ClipFerry runs detection off the selection/UI thread on bounded image crops.

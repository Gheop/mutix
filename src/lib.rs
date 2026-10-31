//! mutix: silently mouthed words recognized from the motion sensors of
//! AirPods. The `mutix` command line and the `mutix-gtk` training app share
//! this engine.

pub mod aap;
pub mod bluez;
pub mod config;
pub mod control;
pub mod dataset;
pub mod dtw;
pub mod eval;
pub mod features;
pub mod frame;
pub mod mic;
pub mod model;
pub mod output;
pub mod segmenter;
pub mod sensor;
pub mod tui;
pub mod vocab;
pub mod voice;

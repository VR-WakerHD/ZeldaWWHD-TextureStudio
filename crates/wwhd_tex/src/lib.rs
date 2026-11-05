//! Core library of ZeldaWWHD Texture Studio: Wii U resource formats, texture
//! extraction, classification, upscaling, re-encoding and archive rebuilding.

pub mod bflim;
pub mod bfres;
pub mod bytes;
pub mod classify;
pub mod container;
pub mod enhance;
pub mod error;
pub mod gx2;
pub mod inventory;
pub mod jobs;
pub mod mods_dir;
pub mod mips;
pub mod pipeline;
pub mod policy;
pub mod project;
pub mod quality;
pub mod resample;
pub mod runtime_pack;
pub mod sarc;
pub mod texture;
pub mod tools_install;
pub mod upscale;
pub mod yaz0;

pub use error::{Error, Result};

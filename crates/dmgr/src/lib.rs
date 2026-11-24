//! # dmgr
//!
//! Create, inspect, verify and extract Apple disk images (`.dmg`) on any
//! platform, in pure Rust.
//!
//! `dmgr` writes UDIF images (`UDZO`, `ULFO`, `ULMO`, `UDBZ`, `UDRO`, `UDRW`)
//! containing an HFS+ (or case-sensitive HFSX) volume, optionally inside a
//! GUID partition table, with Finder window customization (`.DS_Store`
//! layout, background pictures, icon positions and volume icons). It does
//! not need macOS, `hdiutil` or any C libraries.
//!
//! ## Creating an image
//!
//! ```no_run
//! use dmgr::{DmgBuilder, Format};
//!
//! DmgBuilder::new("My App")
//!     .format(Format::Ulfo)
//!     .add_path("build/My App.app")
//!     .add_applications_link()
//!     .write("My App.dmg")?;
//! # Ok::<(), dmgr::Error>(())
//! ```
//!
//! ## Reading an image
//!
//! ```no_run
//! use dmgr::DiskImage;
//!
//! let image = DiskImage::open("My App.dmg")?;
//! let mut volume = image.into_volume()?;
//! for entry in volume.list(volume.root().cnid) {
//!     println!("{}", entry.name);
//! }
//! # Ok::<(), dmgr::Error>(())
//! ```
//!
//! ## Layers
//!
//! * [`DmgBuilder`] — the high-level builder.
//! * [`hfs`] — HFS+ volume builder, reader and checker.
//! * [`udif`] — the UDIF container: writer, reader and checksum verification.
//! * [`partition`] — GPT layout and partition map parsing.
//! * [`finder`] — `.DS_Store` and alias records for Finder windows.
//! * [`disk`] — the sparse virtual disk that ties them together.

#![cfg_attr(docsrs, feature(doc_auto_cfg))]

mod bytes;
pub mod disk;
mod dmg;
mod error;
pub mod finder;
pub mod hfs;
pub mod image;
pub mod io;
pub mod partition;
mod source;
pub mod udif;

pub use dmg::{DmgBuilder, Item, ProgressFn};
pub use error::{Error, Result};
pub use image::{DiskImage, ImageSource, convert};
pub use partition::PartitionScheme;
pub use source::{ImportOptions, wildcard_match};
pub use udif::Format;

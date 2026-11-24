//! Finder window customization: `.DS_Store` view settings, icon positions
//! and background pictures.

pub mod alias;
pub mod dsstore;

use std::path::PathBuf;

use plist::{Dictionary, Value as P};

use crate::error::Result;
use dsstore::{Record, Value};

/// Window background.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Background {
    /// Finder's default (white/system) background.
    #[default]
    Default,
    /// A solid color; components in `0.0..=1.0`.
    Color { red: f64, green: f64, blue: f64 },
    /// A picture (PNG, JPEG, TIFF, ...), copied into a hidden `.background` folder.
    Image(PathBuf),
}

impl Background {
    /// Parses `#RRGGBB` / `RRGGBB` into a color background.
    pub fn from_hex(s: &str) -> Option<Self> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if h.len() != 6 || !h.is_ascii() {
            return None;
        }
        let c = |i: usize| {
            u8::from_str_radix(&h[i..i + 2], 16)
                .ok()
                .map(|v| f64::from(v) / 255.0)
        };
        Some(Self::Color {
            red: c(0)?,
            green: c(2)?,
            blue: c(4)?,
        })
    }
}

/// Position of one item's icon in the window (icon center, in points).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IconPosition {
    /// Name of the item in the volume root.
    pub name: String,
    pub x: u32,
    pub y: u32,
}

/// How the Finder window of the mounted volume looks.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct WindowLayout {
    /// Window origin on screen.
    pub x: i32,
    pub y: i32,
    /// Window content size.
    pub width: u32,
    pub height: u32,
    pub icon_size: u16,
    pub text_size: u16,
    /// Put labels below icons (otherwise to the right).
    pub label_on_bottom: bool,
    pub show_item_info: bool,
    pub show_icon_preview: bool,
    pub show_status_bar: bool,
    pub show_toolbar: bool,
    pub show_sidebar: bool,
    pub show_pathbar: bool,
    pub show_tab_view: bool,
    pub sidebar_width: u32,
    pub grid_spacing: f64,
    /// `none`, `name`, `kind`, `dateModified`, `dateCreated`, `size`, `label`...
    pub arrange_by: String,
    pub background: Background,
    pub icons: Vec<IconPosition>,
}

impl Default for WindowLayout {
    fn default() -> Self {
        Self {
            x: 200,
            y: 120,
            width: 640,
            height: 400,
            icon_size: 128,
            text_size: 13,
            label_on_bottom: true,
            show_item_info: false,
            show_icon_preview: true,
            show_status_bar: false,
            show_toolbar: false,
            show_sidebar: false,
            show_pathbar: false,
            show_tab_view: false,
            sidebar_width: 0,
            grid_spacing: 100.0,
            arrange_by: "none".into(),
            background: Background::Default,
            icons: Vec::new(),
        }
    }
}

fn bplist(d: Dictionary) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    P::Dictionary(d).to_writer_binary(&mut out)?;
    Ok(out)
}

impl WindowLayout {
    /// Builds the `.DS_Store` records for the root folder. `background_alias`
    /// is the alias record of the background picture, when there is one.
    pub fn ds_store_records(&self, background_alias: Option<Vec<u8>>) -> Result<Vec<Record>> {
        let mut bwsp = Dictionary::new();
        bwsp.insert("ContainerShowSidebar".into(), P::Boolean(self.show_sidebar));
        bwsp.insert("PreviewPaneVisibility".into(), P::Boolean(false));
        bwsp.insert("ShowPathbar".into(), P::Boolean(self.show_pathbar));
        bwsp.insert("ShowSidebar".into(), P::Boolean(self.show_sidebar));
        bwsp.insert("ShowStatusBar".into(), P::Boolean(self.show_status_bar));
        bwsp.insert("ShowTabView".into(), P::Boolean(self.show_tab_view));
        bwsp.insert("ShowToolbar".into(), P::Boolean(self.show_toolbar));
        bwsp.insert(
            "SidebarWidth".into(),
            P::Integer(u64::from(self.sidebar_width).into()),
        );
        bwsp.insert(
            "WindowBounds".into(),
            P::String(format!(
                "{{{{{}, {}}}, {{{}, {}}}}}",
                self.x, self.y, self.width, self.height
            )),
        );

        let mut icvp = Dictionary::new();
        icvp.insert("arrangeBy".into(), P::String(self.arrange_by.clone()));
        let (ty, (r, g, b)) = match &self.background {
            Background::Default => (0u64, (1.0, 1.0, 1.0)),
            Background::Color { red, green, blue } => (1, (*red, *green, *blue)),
            Background::Image(_) => (2, (1.0, 1.0, 1.0)),
        };
        icvp.insert("backgroundColorBlue".into(), P::Real(b));
        icvp.insert("backgroundColorGreen".into(), P::Real(g));
        icvp.insert("backgroundColorRed".into(), P::Real(r));
        if let Some(alias) = background_alias {
            icvp.insert("backgroundImageAlias".into(), P::Data(alias));
        }
        icvp.insert("backgroundType".into(), P::Integer(ty.into()));
        icvp.insert("gridOffsetX".into(), P::Real(0.0));
        icvp.insert("gridOffsetY".into(), P::Real(0.0));
        icvp.insert("gridSpacing".into(), P::Real(self.grid_spacing));
        icvp.insert("iconSize".into(), P::Real(f64::from(self.icon_size)));
        icvp.insert("labelOnBottom".into(), P::Boolean(self.label_on_bottom));
        icvp.insert("showIconPreview".into(), P::Boolean(self.show_icon_preview));
        icvp.insert("showItemInfo".into(), P::Boolean(self.show_item_info));
        icvp.insert("textSize".into(), P::Real(f64::from(self.text_size)));
        icvp.insert("viewOptionsVersion".into(), P::Integer(1u64.into()));

        let mut recs = vec![
            Record::new(".", b"bwsp", Value::Blob(bplist(bwsp)?)),
            Record::new(".", b"icvp", Value::Blob(bplist(icvp)?)),
            Record::new(".", b"vSrn", Value::Long(1)),
            Record::new(".", b"vstl", Value::Type(*b"icnv")),
        ];
        for icon in &self.icons {
            let mut blob = Vec::with_capacity(16);
            blob.extend_from_slice(&icon.x.to_be_bytes());
            blob.extend_from_slice(&icon.y.to_be_bytes());
            blob.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00]);
            let name = crate::hfs::unicode::decompose(&icon.name);
            recs.push(Record::new(name, b"Iloc", Value::Blob(blob)));
        }
        Ok(recs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let l = WindowLayout {
            background: Background::from_hex("#336699").unwrap(),
            icons: vec![IconPosition {
                name: "App.app".into(),
                x: 140,
                y: 120,
            }],
            ..Default::default()
        };
        let recs = l.ds_store_records(None).unwrap();
        assert_eq!(recs.len(), 5);
        let data = dsstore::write(recs);
        let back = dsstore::read(&data).unwrap();
        let bwsp = back.iter().find(|r| &r.code == b"bwsp").unwrap();
        let Value::Blob(b) = &bwsp.value else {
            panic!()
        };
        let v = P::from_reader(std::io::Cursor::new(b)).unwrap();
        assert_eq!(
            v.as_dictionary().unwrap()["WindowBounds"]
                .as_string()
                .unwrap(),
            "{{200, 120}, {640, 400}}"
        );
        let iloc = back.iter().find(|r| &r.code == b"Iloc").unwrap();
        assert_eq!(iloc.filename, "App.app");
        assert!(Background::from_hex("zz0000").is_none());
    }
}

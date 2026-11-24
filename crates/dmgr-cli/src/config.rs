//! `dmgr.toml` configuration files and size parsing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A byte size given as an integer or a string like `"200m"`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SizeValue {
    Bytes(u64),
    Text(String),
}

impl SizeValue {
    pub fn bytes(&self) -> Result<u64> {
        match self {
            Self::Bytes(b) => Ok(*b),
            Self::Text(s) => parse_size(s),
        }
    }
}

/// Parses sizes such as `4096`, `512k`, `100m`, `1.5g`, `2GiB`, `10MB`.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("invalid size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        "s" | "sectors" => 512,
        other => bail!("unknown size unit {other:?} in {s:?}"),
    };
    if n < 0.0 || !n.is_finite() {
        bail!("invalid size {s:?}");
    }
    Ok((n * mult as f64).round() as u64)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SymlinkConfig {
    pub name: String,
    pub target: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WindowConfig {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub icon_size: Option<u16>,
    pub text_size: Option<u16>,
    pub label_position: Option<String>,
    pub show_item_info: Option<bool>,
    pub show_icon_preview: Option<bool>,
    pub show_status_bar: Option<bool>,
    pub show_toolbar: Option<bool>,
    pub show_sidebar: Option<bool>,
    pub show_pathbar: Option<bool>,
    pub grid_spacing: Option<f64>,
    pub arrange_by: Option<String>,
    pub background: Option<PathBuf>,
    pub background_color: Option<String>,
    /// Item name -> `[x, y]`.
    #[serde(default)]
    pub icons: BTreeMap<String, [u32; 2]>,
}

/// Contents of a `dmgr.toml` file. Relative paths are resolved against the
/// directory containing the file.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub output: Option<PathBuf>,
    pub volume_name: Option<String>,
    pub format: Option<String>,
    pub level: Option<u32>,
    pub filesystem: Option<String>,
    pub layout: Option<String>,
    pub size: Option<SizeValue>,
    pub free_space: Option<SizeValue>,
    /// Folders whose *contents* go into the volume root.
    #[serde(default)]
    pub contents: Vec<PathBuf>,
    /// Files or folders copied into the volume root.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Destination path in the volume -> source path.
    #[serde(default)]
    pub copy: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub symlinks: Vec<SymlinkConfig>,
    pub applications_link: Option<bool>,
    pub icon: Option<PathBuf>,
    pub open_folder: Option<bool>,
    #[serde(default)]
    pub hide: Vec<String>,
    #[serde(default)]
    pub hide_extension: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub owner: Option<String>,
    pub xattrs: Option<bool>,
    pub reproducible: Option<bool>,
    pub window: Option<WindowConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        if let Some(p) = &mut cfg.output {
            fix(p);
        }
        cfg.contents.iter_mut().for_each(fix);
        cfg.files.iter_mut().for_each(fix);
        cfg.copy.values_mut().for_each(fix);
        if let Some(p) = &mut cfg.icon {
            fix(p);
        }
        if let Some(w) = &mut cfg.window {
            if let Some(p) = &mut w.background {
                fix(p);
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("512k").unwrap(), 512 << 10);
        assert_eq!(parse_size("100m").unwrap(), 100 << 20);
        assert_eq!(parse_size("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_size("10 MB").unwrap(), 10 << 20);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5q").is_err());
    }

    #[test]
    fn parse_config() {
        let cfg: Config = toml::from_str(
            r##"
            volume-name = "My App"
            format = "ULFO"
            size = "200m"
            files = ["dist/My App.app"]
            applications-link = true
            [[symlinks]]
            name = "Docs"
            target = "/Library/Docs"
            [window]
            width = 600
            background-color = "#ffffff"
            [window.icons]
            "My App.app" = [160, 180]
            "##,
        )
        .unwrap();
        assert_eq!(cfg.size.unwrap().bytes().unwrap(), 200 << 20);
        assert_eq!(cfg.window.unwrap().icons["My App.app"], [160, 180]);
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
    }
}

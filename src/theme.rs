use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Default,
    Catppuccin,
    AyuDark,
    Ayuppuccin,
    Custom(String),
}

impl FromStr for Theme {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "default" | "kamui" => Ok(Self::Default),
            "catppuccin" | "catppuccin-mocha" | "mocha" => Ok(Self::Catppuccin),
            "ayu" | "ayu-dark" | "ayu_dark" => Ok(Self::AyuDark),
            "ayuppuccin" | "ayu-catppuccin" | "ayuppuccin-dark" => Ok(Self::Ayuppuccin),
            other => {
                // allow custom theme names (a-z,0-9,-,_)
                if custom_exists(other) && load_custom_palette(other).is_ok() {
                    Ok(Self::Custom(other.to_string()))
                } else {
                    Err(format!(
                        "unknown theme '{other}' (expected: default, catppuccin, ayu-dark, ayuppuccin or a file in ~/.config/kamui/themes/<name>.json)"
                    ))
                }
            }
        }
    }
}
impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Catppuccin => f.write_str("catppuccin"),
            Self::AyuDark => f.write_str("ayu-dark"),
            Self::Ayuppuccin => f.write_str("ayuppuccin"),
            Self::Custom(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Palette {
    pub bg: String,
    pub fg: String,
    pub muted: String,
    pub mauve: String,
    pub blue: String,
    pub green: String,
    pub red: String,
    pub amber: String,
    pub teal: String,
    pub cyan: String,
}

impl Theme {
    pub fn palette(&self) -> Option<Palette> {
        match self {
            Self::Default => None,
            Self::Catppuccin => Some(Palette {
                bg: "#1e1e2e".into(),
                fg: "#cdd6f4".into(),
                muted: "#6c7086".into(),
                mauve: "#cba6f7".into(),
                blue: "#89b4fa".into(),
                green: "#a6e3a1".into(),
                red: "#f38ba8".into(),
                amber: "#fab387".into(),
                teal: "#94e2d5".into(),
                cyan: "#89dceb".into(),
            }),
            Self::AyuDark => Some(Palette {
                bg: "#0a0e14".into(),
                fg: "#bfbdb6".into(),
                muted: "#5c6773".into(),
                mauve: "#d4bfff".into(),
                blue: "#59c2ff".into(),
                green: "#aad94c".into(),
                red: "#f07178".into(),
                amber: "#ffb454".into(),
                teal: "#95e6cb".into(),
                cyan: "#95e6cb".into(),
            }),
            Self::Ayuppuccin => Some(Palette {
                bg: "#2c2c2e".into(),
                fg: "#bfbdb6".into(),
                muted: "#8a8986".into(),
                mauve: "#cba6f7".into(),
                blue: "#5ac1fe".into(),
                green: "#a9d94b".into(),
                red: "#ef7177".into(),
                amber: "#feb454".into(),
                teal: "#94e2d5".into(),
                cyan: "#95e6cb".into(),
            }),
            Self::Custom(name) => load_custom_palette(name).ok(),
        }
    }
    pub fn all() -> Vec<Theme> {
        let mut v = vec![
            Self::Default,
            Self::Catppuccin,
            Self::AyuDark,
            Self::Ayuppuccin,
        ];
        for n in list_custom_names() {
            if load_custom_palette(&n).is_ok() {
                v.push(Self::Custom(n));
            }
        }
        v
    }
}

pub fn themes_dir() -> PathBuf {
    crate::config::global_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("themes")
}
fn custom_exists(name: &str) -> bool {
    themes_dir().join(format!("{name}.json")).is_file()
}
fn list_custom_names() -> Vec<String> {
    let dir = themes_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
        {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}
fn load_custom_palette(name: &str) -> Result<Palette, String> {
    let path = themes_dir().join(format!("{name}.json"));
    load_custom_palette_path(&path)
}

fn load_custom_palette_path(path: &std::path::Path) -> Result<Palette, String> {
    let fail = |cause: String| format!("theme: {}: {cause}", path.display());
    let data = std::fs::read_to_string(path).map_err(|e| fail(format!("cannot read: {e}")))?;
    let v: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| fail(format!("invalid JSON: {e}")))?;
    if !v.is_object() {
        return Err(fail("root must be a JSON object".into()));
    }
    let color = |field: &str, value: Option<&str>| -> Result<String, String> {
        let value = value.ok_or_else(|| fail(format!("missing string field '{field}'")))?;
        hex_to_rgb(value).map_err(|_| fail(format!("field '{field}' must be exact #RRGGBB")))?;
        Ok(value.to_string())
    };
    // support either flat {bg,fg,...} or {defs:{}, theme:{}} like ayuppuccin.json
    let get = |k: &str| v.get(k).and_then(|x| x.as_str());
    // if has defs, resolve refs
    if let Some(defs) = v.get("defs") {
        let resolve = |key: &str| -> Result<String, String> {
            // theme.<key> may be {dark:"mauve"} -> resolve via defs
            let theme = v
                .get("theme")
                .and_then(|x| x.get(key))
                .ok_or_else(|| fail(format!("missing theme field '{key}'")))?;
            let ref_name = theme
                .get("dark")
                .and_then(|x| x.as_str())
                .or_else(|| theme.as_str())
                .ok_or_else(|| fail(format!("theme field '{key}' must be a string or dark ref")))?;
            let resolved = defs
                .get(ref_name)
                .and_then(|x| x.as_str())
                .unwrap_or(ref_name);
            color(&format!("theme.{key} -> {ref_name}"), Some(resolved))
        };
        return Ok(Palette {
            bg: resolve("background")?,
            fg: resolve("text")?,
            muted: if v.get("theme").and_then(|x| x.get("textMuted")).is_some() {
                resolve("textMuted")?
            } else {
                color("fg_muted", get("fg_muted"))?
            },
            mauve: color(
                "defs.mauve",
                defs.get("mauve")
                    .and_then(|x| x.as_str())
                    .or(Some("#cba6f7")),
            )?,
            blue: color(
                "defs.blue",
                defs.get("ayu_blue")
                    .or_else(|| defs.get("blue"))
                    .and_then(|x| x.as_str())
                    .or(Some("#89b4fa")),
            )?,
            green: color(
                "defs.green",
                defs.get("ayu_green")
                    .or_else(|| defs.get("green"))
                    .and_then(|x| x.as_str())
                    .or(Some("#a6e3a1")),
            )?,
            red: color(
                "defs.red",
                defs.get("ayu_red")
                    .or_else(|| defs.get("red"))
                    .and_then(|x| x.as_str())
                    .or(Some("#f38ba8")),
            )?,
            amber: color(
                "defs.amber",
                defs.get("ayu_amber")
                    .or_else(|| defs.get("amber"))
                    .and_then(|x| x.as_str())
                    .or(Some("#fab387")),
            )?,
            teal: color(
                "defs.teal",
                defs.get("teal")
                    .and_then(|x| x.as_str())
                    .or(Some("#94e2d5")),
            )?,
            cyan: color(
                "defs.cyan",
                defs.get("cyan")
                    .or_else(|| defs.get("teal"))
                    .and_then(|x| x.as_str())
                    .or(Some("#89dceb")),
            )?,
        });
    }
    Ok(Palette {
        bg: color("bg", get("bg"))?,
        fg: color("fg", get("fg"))?,
        muted: color("muted", get("muted").or_else(|| get("fg_muted")))?,
        mauve: color("mauve", get("mauve").or(Some("#cba6f7")))?,
        blue: color("blue", get("blue").or(Some("#89b4fa")))?,
        green: color("green", get("green").or(Some("#a6e3a1")))?,
        red: color("red", get("red").or(Some("#f38ba8")))?,
        amber: color("amber", get("amber").or(Some("#fab387")))?,
        teal: color("teal", get("teal").or(Some("#94e2d5")))?,
        cyan: color("cyan", get("cyan").or(Some("#89dceb")))?,
    })
}

#[allow(dead_code)]
pub fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), String> {
    let bytes = hex.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' || !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        return Err("expected exact #RRGGBB".into());
    }
    let parse = |range| u8::from_str_radix(&hex[range], 16).map_err(|e| e.to_string());
    Ok((parse(1..3)?, parse(3..5)?, parse(5..7)?))
}
#[allow(dead_code)]
pub fn fg_true(hex: &str) -> String {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or_default();
    format!("\x1b[38;2;{r};{g};{b}m")
}
#[allow(dead_code)]
pub fn bg_true(hex: &str) -> String {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or_default();
    format!("\x1b[48;2;{r};{g};{b}m")
}
#[allow(dead_code)]
pub fn ratatui_fg(hex: &str) -> ratatui::style::Color {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or_default();
    ratatui::style::Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn theme_file(data: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-theme-{}.json", Uuid::new_v4()));
        fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn hex_parser_rejects_short_and_invalid_values() {
        assert!(hex_to_rgb("#fff").is_err());
        assert!(hex_to_rgb("112233").is_err());
        assert!(hex_to_rgb("#gg2233").is_err());
        assert_eq!(hex_to_rgb("#Aa10ff").unwrap(), (0xaa, 0x10, 0xff));
    }

    #[test]
    fn loads_valid_flat_theme_and_rejects_invalid_field() {
        let valid = theme_file(r##"{"bg":"#112233","fg":"#abcdef","muted":"#010203"}"##);
        assert_eq!(load_custom_palette_path(&valid).unwrap().bg, "#112233");
        fs::remove_file(valid).unwrap();

        let invalid = theme_file(r##"{"bg":"#123","fg":"#abcdef","muted":"#010203"}"##);
        let error = load_custom_palette_path(&invalid).unwrap_err();
        assert!(error.contains("field 'bg'"));
        assert!(error.contains(&invalid.display().to_string()));
        fs::remove_file(invalid).unwrap();
    }

    #[test]
    fn loads_defs_theme_and_rejects_bad_reference() {
        let valid = theme_file(
            r##"{"defs":{"base":"#112233","text":"#abcdef","dim":"#010203"},"theme":{"background":{"dark":"base"},"text":{"dark":"text"},"textMuted":{"dark":"dim"}}}"##,
        );
        assert_eq!(load_custom_palette_path(&valid).unwrap().fg, "#abcdef");
        fs::remove_file(valid).unwrap();

        let invalid = theme_file(
            r##"{"defs":{"base":"#112233","text":"#abcdef"},"theme":{"background":{"dark":"base"},"text":{"dark":"text"},"textMuted":{"dark":"missing"}}}"##,
        );
        let error = load_custom_palette_path(&invalid).unwrap_err();
        assert!(error.contains("theme.textMuted -> missing"));
        fs::remove_file(invalid).unwrap();
    }
}

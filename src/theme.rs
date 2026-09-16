use std::{fs, path::Path};

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub background: Rgb,
    pub surface: Rgb,
    pub primary: Rgb,
    pub accent: Rgb,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl From<Rgb> for Color {
    fn from(value: Rgb) -> Self {
        Color::Rgb(value.0, value.1, value.2)
    }
}

impl Theme {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.is_file() {
            let theme = presets().remove(0);
            theme.save(path)?;
            return Ok(theme);
        }
        toml::from_str(&fs::read_to_string(path)?).context("parse theme configuration")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn gradient(&self, step: u8, total: u8) -> Color {
        let total = total.max(1) as u16;
        let step = step.min(total as u8) as u16;
        let mix = |a: u8, b: u8| -> u8 {
            (((a as u16) * (total - step) + (b as u16) * step) / total) as u8
        };
        Color::Rgb(
            mix(self.primary.0, self.accent.0),
            mix(self.primary.1, self.accent.1),
            mix(self.primary.2, self.accent.2),
        )
    }
}

pub fn presets() -> Vec<Theme> {
    vec![
        Theme {
            name: "Night City".into(),
            background: Rgb(10, 7, 18),
            surface: Rgb(35, 14, 48),
            primary: Rgb(238, 28, 76),
            accent: Rgb(247, 239, 0),
        },
        Theme {
            name: "Arasaka".into(),
            background: Rgb(5, 5, 7),
            surface: Rgb(28, 28, 32),
            primary: Rgb(210, 13, 35),
            accent: Rgb(235, 235, 235),
        },
        Theme {
            name: "Mox".into(),
            background: Rgb(17, 5, 31),
            surface: Rgb(50, 16, 70),
            primary: Rgb(246, 55, 178),
            accent: Rgb(44, 225, 230),
        },
        Theme {
            name: "Samurai".into(),
            background: Rgb(8, 4, 5),
            surface: Rgb(45, 8, 14),
            primary: Rgb(236, 22, 45),
            accent: Rgb(255, 198, 0),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_uses_endpoints() {
        let theme = presets().remove(0);
        assert_eq!(theme.gradient(0, 10), Color::from(theme.primary));
        assert_eq!(theme.gradient(10, 10), Color::from(theme.accent));
    }
}

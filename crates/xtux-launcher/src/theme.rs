//! Colours of the launcher themes. They live here, not in the .slint file, so a test can check every theme and
//! accent against WCAG contrast rules, also over the worst backdrop image.

/// Straight (not premultiplied) sRGB colour with alpha 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

const fn rgb(hex: u32) -> Rgba {
    Rgba {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 1.0,
    }
}

const fn rgba(hex: u32, a: f32) -> Rgba {
    Rgba {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a,
    }
}

impl Rgba {
    pub fn to_slint(self) -> slint::Color {
        slint::Color::from_argb_u8((self.a * 255.0).round() as u8, self.r, self.g, self.b)
    }

    #[cfg(test)]
    /// `self` drawn over the opaque `below`.
    fn over(self, below: Rgba) -> Rgba {
        let mix = |top: u8, bottom: u8| {
            (top as f32 * self.a + bottom as f32 * (1.0 - self.a)).round() as u8
        };
        Rgba {
            r: mix(self.r, below.r),
            g: mix(self.g, below.g),
            b: mix(self.b, below.b),
            a: 1.0,
        }
    }

    #[cfg(test)]
    fn with_alpha(self, a: f32) -> Rgba {
        Rgba { a, ..self }
    }

    /// WCAG relative luminance.
    fn luminance(self) -> f32 {
        let channel = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }
}

/// WCAG contrast ratio of two opaque colours.
pub fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (a.luminance(), b.luminance());
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Brand,
    Anthracite,
    Light,
}

#[cfg(test)]
pub const ACCENTS: [&str; 5] = ["lime", "emerald", "cyan", "violet", "amber"];
#[cfg(test)]
pub const VARIANTS: [Variant; 3] = [Variant::Brand, Variant::Anthracite, Variant::Light];

/// Which variant a theme choice means.
pub fn variant(choice: &str, dark_variant: &str, system_dark: bool) -> Variant {
    let dark = |name: &str| {
        if name == "anthracite" {
            Variant::Anthracite
        } else {
            Variant::Brand
        }
    };
    match choice {
        "light" => Variant::Light,
        "brand" | "anthracite" => dark(choice),
        _ if system_dark => dark(dark_variant),
        _ => Variant::Light,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub base: Rgba,
    pub base_2: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub glass: Rgba,
    pub glass_strong: Rgba,
    pub glass_border: Rgba,
    pub shine: Rgba,
    pub shadow: Rgba,
    pub field: Rgba,
    /// Fill of buttons and markers.
    pub accent: Rgba,
    /// Accent coloured text and glyphs.
    pub accent_text: Rgba,
    /// Text on the accent fill.
    pub on_accent: Rgba,
    /// Fill of destructive buttons (Stop, Delete) and the text on it.
    pub danger: Rgba,
    pub on_danger: Rgba,
    /// Fill of warnings (anti-cheat that may block) and the text on it.
    pub warning: Rgba,
    pub on_warning: Rgba,
    /// Opacity of the blurred game art behind everything.
    pub backdrop_opacity: f32,
    /// Opacity of the veil over the backdrop at the top of the window (the thinnest part).
    pub veil_min: f32,
}

const DARK_INK: Rgba = rgb(0x06140F);
const WHITE: Rgba = rgb(0xFFFFFF);

pub fn palette(variant: Variant, accent: &str) -> Palette {
    let light = variant == Variant::Light;
    // Dark themes use the bright brand accents; the light theme needs deeper tones to stay readable.
    let (accent, accent_text) = match (accent, light) {
        ("emerald", false) => (rgb(0x34D399), rgb(0x34D399)),
        ("cyan", false) => (rgb(0x22D3EE), rgb(0x22D3EE)),
        ("violet", false) => (rgb(0xA78BFA), rgb(0xB9A4FB)),
        ("amber", false) => (rgb(0xFBBF24), rgb(0xFBBF24)),
        (_, false) => (rgb(0xA3E635), rgb(0xA3E635)),
        ("emerald", true) => (rgb(0x059669), rgb(0x046C4E)),
        ("cyan", true) => (rgb(0x0891B2), rgb(0x0B6A82)),
        ("violet", true) => (rgb(0x7C3AED), rgb(0x6D28D9)),
        ("amber", true) => (rgb(0xD97706), rgb(0x92400E)),
        (_, true) => (rgb(0x65A30D), rgb(0x3F6212)),
    };
    // Whichever of dark ink and white reads better on the accent.
    let on_accent = if contrast(DARK_INK, accent) >= contrast(WHITE, accent) {
        DARK_INK
    } else {
        WHITE
    };
    let common = |base, base_2, text, muted, glass, glass_strong| Palette {
        base,
        base_2,
        text,
        muted,
        glass,
        glass_strong,
        glass_border: rgba(0xFFFFFF, if light { 0.85 } else { 0.14 }),
        shine: rgba(0xFFFFFF, if light { 0.55 } else { 0.10 }),
        shadow: if light {
            rgba(0x0B1A14, 0.19)
        } else {
            rgba(0x000000, 0.66)
        },
        field: if light {
            rgba(0xFFFFFF, 0.72)
        } else {
            rgba(0x000000, 0.34)
        },
        accent,
        accent_text,
        on_accent,
        danger: rgb(0xDC2626),
        on_danger: WHITE,
        warning: rgb(0xF59E0B),
        on_warning: DARK_INK,
        backdrop_opacity: if light { 0.40 } else { 0.55 },
        veil_min: if light { 0.45 } else { 0.40 },
    };
    match variant {
        Variant::Brand => common(
            rgb(0x04100C),
            rgb(0x0F3B2E),
            rgb(0xF4F7FB),
            rgb(0xA9C9BA),
            rgba(0x0B2E23, 0.62),
            rgba(0x082219, 0.88),
        ),
        Variant::Anthracite => common(
            rgb(0x0A0B0D),
            rgb(0x1A1C21),
            rgb(0xF4F7FB),
            rgb(0xA9AFB7),
            rgba(0x1D1F24, 0.66),
            rgba(0x1A1C21, 0.9),
        ),
        Variant::Light => common(
            rgb(0xDCE7E1),
            rgb(0xF4F8F6),
            rgb(0x0B1A14),
            rgb(0x3D5648),
            rgba(0xFFFFFF, 0.66),
            rgba(0xFFFFFF, 0.86),
        ),
    }
}

#[cfg(test)]
impl Palette {
    /// The window background at its weakest: the darkest or brightest backdrop pixel under the thinnest veil.
    fn worst_backgrounds(&self) -> [Rgba; 2] {
        [rgb(0x000000), WHITE].map(|pixel| {
            let art = pixel.with_alpha(self.backdrop_opacity).over(self.base);
            self.base.with_alpha(self.veil_min).over(art)
        })
    }

    /// Every text colour on every surface it is drawn on, with the WCAG minimum it must reach.
    pub fn checks(&self) -> Vec<(&'static str, f32, f32)> {
        let mut checks = Vec::new();
        for background in self.worst_backgrounds() {
            let glass = self.glass.over(background);
            let strong = self.glass_strong.over(background);
            let field = self.field.over(glass);
            let active = self.accent.with_alpha(0.25).over(glass);
            for (surface, name) in [(glass, "glass"), (strong, "strong glass")] {
                checks.push((name_of("text", name), contrast(self.text, surface), 4.5));
                checks.push((name_of("muted", name), contrast(self.muted, surface), 4.5));
                checks.push((
                    name_of("accent text", name),
                    contrast(self.accent_text, surface),
                    4.5,
                ));
            }
            checks.push(("text on fields", contrast(self.text, field), 4.5));
            checks.push(("text on active buttons", contrast(self.text, active), 4.5));
            // Glyphs (navigation icons) are graphics: 3:1 on the active navigation item.
            let nav = self.accent.with_alpha(0.2).over(strong);
            checks.push((
                "accent glyph on active navigation",
                contrast(self.accent_text, nav),
                3.0,
            ));
            // Page titles (30 px, bold) sit straight on the veiled backdrop: large text, 3:1.
            checks.push((
                "titles on the backdrop",
                contrast(self.text, background),
                3.0,
            ));
            checks.push((
                "muted on the backdrop",
                contrast(self.muted, background),
                3.0,
            ));
        }
        checks.push((
            "text on the accent",
            contrast(self.on_accent, self.accent),
            4.5,
        ));
        checks.push(("text on danger", contrast(self.on_danger, self.danger), 4.5));
        checks.push((
            "text on warning",
            contrast(self.on_warning, self.warning),
            4.5,
        ));
        checks
    }
}

#[cfg(test)]
fn name_of(what: &'static str, surface: &'static str) -> &'static str {
    // Static names keep the check list cheap; the combinations are few.
    match (what, surface) {
        ("text", "glass") => "text on glass",
        ("text", _) => "text on strong glass",
        ("muted", "glass") => "muted on glass",
        ("muted", _) => "muted on strong glass",
        ("accent text", "glass") => "accent text on glass",
        _ => "accent text on strong glass",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_and_accent_is_readable() {
        let mut failures = Vec::new();
        for variant in VARIANTS {
            for accent in ACCENTS {
                for (what, ratio, min) in palette(variant, accent).checks() {
                    if ratio < min {
                        failures.push(format!("{variant:?}/{accent}: {what} {ratio:.2} < {min}"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "contrast too low:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn choices_map_to_variants() {
        assert_eq!(variant("system", "anthracite", true), Variant::Anthracite);
        assert_eq!(variant("system", "brand", false), Variant::Light);
        assert_eq!(variant("brand", "anthracite", false), Variant::Brand);
        assert_eq!(variant("light", "brand", true), Variant::Light);
    }

    #[test]
    fn contrast_matches_wcag_examples() {
        assert!((contrast(rgb(0x000000), WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(WHITE, WHITE) - 1.0).abs() < 0.01);
    }
}

use eframe::egui::Color32;

pub struct Palette {
    /// Tab strip background.
    pub chrome: Color32,
    /// Active tab, sidebar and page canvas.
    pub content: Color32,
    pub hover: Color32,
    pub separator: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub accent: Color32,
    pub page_border: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            chrome: Color32::from_gray(24),
            content: Color32::from_gray(38),
            hover: Color32::from_gray(32),
            separator: Color32::from_gray(52),
            text: Color32::from_gray(225),
            text_weak: Color32::from_gray(140),
            accent: Color32::from_rgb(10, 132, 255),
            page_border: Color32::from_black_alpha(90),
        }
    } else {
        Palette {
            chrome: Color32::from_gray(218),
            content: Color32::from_gray(240),
            hover: Color32::from_gray(228),
            separator: Color32::from_gray(205),
            text: Color32::from_gray(30),
            text_weak: Color32::from_gray(110),
            accent: Color32::from_rgb(0, 122, 255),
            page_border: Color32::from_black_alpha(40),
        }
    }
}

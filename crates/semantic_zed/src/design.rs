use gpui::{App, BoxShadow, Hsla, hsla, px};
use ui::prelude::*;

/// Shared visual tokens for the scientific workspace surfaces.
///
/// These are deliberately opaque. The editor and PDF remain quiet reading
/// canvases, while the sidebar gets only a very small cool-violet cast. Dark
/// themes continue to inherit their authored Zed colors instead of being
/// force-fit into the light product palette.
#[derive(Clone)]
pub(crate) struct ScientificPalette {
    pub sidebar: Hsla,
    pub canvas: Hsla,
    pub card: Hsla,
    pub pdf_surround: Hsla,
    pub divider: Hsla,
    pub accent_tint: Hsla,
    pub card_shadow: Vec<BoxShadow>,
}

impl ScientificPalette {
    pub fn resolve(cx: &App) -> Self {
        let colors = cx.theme().colors();
        let is_light = colors.editor_background.l > 0.55;

        if is_light {
            Self {
                // Near-white with just enough violet to distinguish the dock
                // from the document canvas at a glance.
                sidebar: hsla(246.0 / 360.0, 0.20, 0.976, 1.0),
                canvas: hsla(240.0 / 360.0, 0.08, 0.995, 1.0),
                card: hsla(0.0, 0.0, 1.0, 1.0),
                pdf_surround: hsla(244.0 / 360.0, 0.12, 0.965, 1.0),
                divider: hsla(242.0 / 360.0, 0.11, 0.89, 0.72),
                accent_tint: hsla(246.0 / 360.0, 0.72, 0.965, 1.0),
                card_shadow: vec![
                    BoxShadow::new(px(0.0), px(1.0), hsla(242.0 / 360.0, 0.16, 0.16, 0.045))
                        .blur_radius(px(2.0)),
                    BoxShadow::new(px(0.0), px(4.0), hsla(242.0 / 360.0, 0.16, 0.16, 0.035))
                        .blur_radius(px(12.0)),
                ],
            }
        } else {
            Self {
                sidebar: colors.panel_background,
                canvas: colors.editor_background,
                card: colors.surface_background,
                pdf_surround: colors.background,
                divider: colors.border_variant,
                accent_tint: colors.ghost_element_selected,
                card_shadow: vec![
                    BoxShadow::new(px(0.0), px(2.0), hsla(0.0, 0.0, 0.0, 0.16))
                        .blur_radius(px(8.0)),
                ],
            }
        }
    }
}

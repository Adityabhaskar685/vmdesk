//! The few icons the app draws, as vector paths on a 16-unit grid. Paths, not font glyphs:
//! Noto Sans has no search or ellipsis glyph.

use gpui::{IntoElement, PathBuilder, Pixels, Point, Styled, canvas, point, px, rgb};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    More,
    Plus,
    Terminal,
    Close,
    Expand,
    Collapse,
    Sidebar,
    SplitRow,
    SplitColumn,
    Check,
    Palette,
    WinMinimize,
    WinMaximize,
    WinRestore,
}

enum Stroke {
    Line(&'static [(f32, f32)]),
    Circle { x: f32, y: f32, r: f32 },
    Dot { x: f32, y: f32, r: f32 },
}

impl Icon {
    fn strokes(self) -> &'static [Stroke] {
        match self {
            Self::Search => &[
                Stroke::Circle {
                    x: 7.,
                    y: 7.,
                    r: 4.5,
                },
                Stroke::Line(&[(10.4, 10.4), (13.5, 13.5)]),
            ],
            Self::More => &[
                Stroke::Dot {
                    x: 3.5,
                    y: 8.,
                    r: 1.4,
                },
                Stroke::Dot {
                    x: 8.,
                    y: 8.,
                    r: 1.4,
                },
                Stroke::Dot {
                    x: 12.5,
                    y: 8.,
                    r: 1.4,
                },
            ],
            Self::Plus => &[
                Stroke::Line(&[(8., 3.5), (8., 12.5)]),
                Stroke::Line(&[(3.5, 8.), (12.5, 8.)]),
            ],
            Self::Terminal => &[
                Stroke::Line(&[(3., 4.5), (6.5, 8.), (3., 11.5)]),
                Stroke::Line(&[(8.5, 11.5), (13., 11.5)]),
            ],
            Self::Close => &[
                Stroke::Line(&[(4.5, 4.5), (11.5, 11.5)]),
                Stroke::Line(&[(11.5, 4.5), (4.5, 11.5)]),
            ],
            // Corners pointing out: the pane fills the window.
            Self::Expand => &[
                Stroke::Line(&[(3., 6.), (3., 3.), (6., 3.)]),
                Stroke::Line(&[(10., 3.), (13., 3.), (13., 6.)]),
                Stroke::Line(&[(13., 10.), (13., 13.), (10., 13.)]),
                Stroke::Line(&[(6., 13.), (3., 13.), (3., 10.)]),
            ],
            // The same corners turned in: back to the normal layout.
            Self::Collapse => &[
                Stroke::Line(&[(3., 6.), (6., 6.), (6., 3.)]),
                Stroke::Line(&[(10., 3.), (10., 6.), (13., 6.)]),
                Stroke::Line(&[(13., 10.), (10., 10.), (10., 13.)]),
                Stroke::Line(&[(6., 13.), (6., 10.), (3., 10.)]),
            ],
            Self::Sidebar => &[
                Stroke::Line(&[(3., 3.5), (13., 3.5), (13., 12.5), (3., 12.5), (3., 3.5)]),
                Stroke::Line(&[(6.5, 3.5), (6.5, 12.5)]),
            ],
            // Window controls, drawn like GNOME's: a bar, a square, two stacked squares.
            Self::WinMinimize => &[Stroke::Line(&[(4., 8.5), (12., 8.5)])],
            Self::WinMaximize => &[Stroke::Line(&[
                (4., 4.),
                (12., 4.),
                (12., 12.),
                (4., 12.),
                (4., 4.),
            ])],
            Self::WinRestore => &[
                Stroke::Line(&[(4., 6.), (10., 6.), (10., 12.), (4., 12.), (4., 6.)]),
                Stroke::Line(&[(6., 6.), (6., 4.), (12., 4.), (12., 10.), (10., 10.)]),
            ],
            Self::Check => &[Stroke::Line(&[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)])],
            // A painter's palette: an outline with three paint dots.
            Self::Palette => &[
                Stroke::Circle {
                    x: 8.,
                    y: 8.,
                    r: 5.5,
                },
                Stroke::Dot {
                    x: 5.5,
                    y: 6.5,
                    r: 1.1,
                },
                Stroke::Dot {
                    x: 8.5,
                    y: 5.,
                    r: 1.1,
                },
                Stroke::Dot {
                    x: 10.8,
                    y: 7.5,
                    r: 1.1,
                },
            ],
            // A pane split side by side (a vertical divider).
            Self::SplitRow => &[
                Stroke::Line(&[(3., 3.5), (13., 3.5), (13., 12.5), (3., 12.5), (3., 3.5)]),
                Stroke::Line(&[(8., 3.5), (8., 12.5)]),
            ],
            // A pane split one above the other (a horizontal divider).
            Self::SplitColumn => &[
                Stroke::Line(&[(3., 3.5), (13., 3.5), (13., 12.5), (3., 12.5), (3., 3.5)]),
                Stroke::Line(&[(3., 8.), (13., 8.)]),
            ],
        }
    }
}

pub fn icon(icon: Icon, size: Pixels, color: u32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let scale = size / 16.;
            let at = |x: f32, y: f32| -> Point<Pixels> {
                point(bounds.origin.x + scale * x, bounds.origin.y + scale * y)
            };
            for stroke in icon.strokes() {
                let mut path = match stroke {
                    Stroke::Dot { .. } => PathBuilder::fill(),
                    _ => PathBuilder::stroke(px(1.5) * (size / px(16.)).max(1.)),
                };
                match *stroke {
                    Stroke::Line(points) => {
                        let mut points = points.iter();
                        if let Some(&(x, y)) = points.next() {
                            path.move_to(at(x, y));
                        }
                        for &(x, y) in points {
                            path.line_to(at(x, y));
                        }
                    }
                    Stroke::Circle { x, y, r } | Stroke::Dot { x, y, r } => {
                        let radii = point(scale * r, scale * r);
                        path.move_to(at(x - r, y));
                        path.arc_to(radii, px(0.), false, true, at(x + r, y));
                        path.arc_to(radii, px(0.), false, true, at(x - r, y));
                        path.close();
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(color));
                }
            }
        },
    )
    .size(size)
    .flex_shrink_0()
}

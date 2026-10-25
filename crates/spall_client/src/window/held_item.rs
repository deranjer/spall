//! Screen-space overlays for items used from the pack (ENG-262): the compass
//! held in the lower right, and a readable paper note.
//!
//! North is world -Z: yaw 0 looks along -Z (see `view_dir_from`) and a
//! positive yaw turns toward +X, so a compass heading in degrees is the yaw
//! in degrees, clockwise seen from above (north 0, east 90, south 180).

/// Dial radius, logical pixels.
const DIAL_RADIUS: f32 = 56.0;
/// Margin from the screen's lower right corner.
const DIAL_MARGIN: f32 = 28.0;
/// Dots along the needle, tip to tail, on each side of the pivot.
const NEEDLE_DOTS: usize = 9;

/// Where world north points on the screen for a camera with this yaw, as a
/// unit vector with x to the right and y down. Facing north it points up;
/// facing east it points left.
pub(super) fn north_on_screen(yaw: f32) -> [f32; 2] {
    [-yaw.sin(), -yaw.cos()]
}

const DIAL_FACE: yakui::Color = yakui::Color::rgb(70, 52, 34);
const DIAL_RIM: yakui::Color = yakui::Color::rgb(150, 118, 70);
const NEEDLE_NORTH: yakui::Color = yakui::Color::rgb(210, 50, 40);
const NEEDLE_SOUTH: yakui::Color = yakui::Color::rgb(235, 228, 210);
const PAPER: yakui::Color = yakui::Color::rgb(236, 224, 190);
const PAPER_INK: yakui::Color = yakui::Color::rgb(48, 34, 22);

fn at(x: f32, y: f32, children: &mut dyn FnMut()) {
    yakui::align(yakui::Alignment::TOP_LEFT, || {
        yakui::offset(yakui::Vec2::new(x, y), children);
    });
}

/// The held compass: a round dial lower right with a needle on world north.
pub(super) fn draw_compass(yaw: f32, screen: [f32; 2]) {
    let [dx, dy] = north_on_screen(yaw);
    let centre = [
        screen[0] - DIAL_MARGIN - DIAL_RADIUS,
        screen[1] - DIAL_MARGIN - DIAL_RADIUS,
    ];
    let square = |c: [f32; 2], half: f32, color: yakui::Color| {
        at(c[0] - half, c[1] - half, &mut || {
            yakui::colored_box(color, yakui::Vec2::splat(half * 2.0));
        });
    };
    // A disc from stacked rows.
    let disc = |radius: f32, color: yakui::Color| {
        let rows = (radius * 2.0 / 4.0) as i32;
        for row in 0..rows {
            let y = -radius + (row as f32 + 0.5) * 4.0;
            let half = (radius * radius - y * y).max(0.0).sqrt();
            at(centre[0] - half, centre[1] + y - 2.0, &mut || {
                yakui::colored_box(color, yakui::Vec2::new(half * 2.0, 4.0));
            });
        }
    };
    disc(DIAL_RADIUS, DIAL_RIM);
    disc(DIAL_RADIUS - 5.0, DIAL_FACE);
    // Cardinal ticks and the N label.
    for (i, label) in ["N", "E", "S", "W"].into_iter().enumerate() {
        let a = i as f32 * std::f32::consts::FRAC_PI_2;
        let (sa, ca) = a.sin_cos();
        let p = [
            centre[0] + sa * (DIAL_RADIUS - 14.0),
            centre[1] - ca * (DIAL_RADIUS - 14.0),
        ];
        at(p[0] - 5.0, p[1] - 9.0, &mut || {
            let mut text = yakui::widgets::Text::new(14.0, label);
            text.style.color = if i == 0 { NEEDLE_NORTH } else { DIAL_RIM };
            text.show();
        });
    }
    // The needle: dots along the line through the pivot, red toward north.
    let reach = DIAL_RADIUS - 18.0;
    for i in 1..=NEEDLE_DOTS {
        let t = i as f32 / NEEDLE_DOTS as f32 * reach;
        let size = 3.0 + (1.0 - i as f32 / NEEDLE_DOTS as f32) * 2.0;
        square([centre[0] + dx * t, centre[1] + dy * t], size, NEEDLE_NORTH);
        square([centre[0] - dx * t, centre[1] - dy * t], size, NEEDLE_SOUTH);
    }
    square(centre, 4.0, DIAL_RIM);
}

/// A paper note, centred over the world, with its text and a hint.
pub(super) fn draw_note(text: &str, screen: [f32; 2]) {
    let w = (screen[0] * 0.5).clamp(280.0, 520.0);
    let lines: Vec<&str> = text.lines().collect();
    let h = 90.0 + lines.len() as f32 * 30.0;
    let (x, y) = ((screen[0] - w) * 0.5, (screen[1] - h) * 0.5);
    yakui::colored_box(
        yakui::Color::BLACK.with_alpha(0.4),
        yakui::Vec2::from_array(screen),
    );
    at(x, y, &mut || {
        yakui::colored_box(PAPER, yakui::Vec2::new(w, h));
    });
    for (i, line) in lines.iter().enumerate() {
        at(x + 28.0, y + 28.0 + i as f32 * 30.0, &mut || {
            let mut t = yakui::widgets::Text::new(22.0, (*line).to_owned());
            t.style.color = PAPER_INK;
            t.show();
        });
    }
    at(x + 28.0, y + h - 30.0, &mut || {
        let mut t = yakui::widgets::Text::new(13.0, "Esc or click to put it away");
        t.style.color = PAPER_INK.with_alpha(0.6);
        t.show();
    });
}

#[cfg(test)]
mod tests {
    use super::super::view_dir_from;
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5
    }

    #[test]
    fn the_needle_points_to_world_north_whatever_the_yaw() {
        // Facing north (yaw 0, along -Z): north is straight ahead, up.
        assert!(near(north_on_screen(0.0), [0.0, -1.0]));
        // Facing east: north is to the left. South: behind (down). West: right.
        assert!(near(north_on_screen(FRAC_PI_2), [-1.0, 0.0]));
        assert!(near(north_on_screen(2.0 * FRAC_PI_2), [0.0, 1.0]));
        assert!(near(north_on_screen(-FRAC_PI_2), [1.0, 0.0]));
        // Agrees with the camera: the view direction's heading is the yaw.
        // Agrees with the camera basis: right = (-fwd.z, 0, fwd.x), north = -Z.
        for yaw in [0.7_f32, -2.1, 4.0] {
            let fwd = view_dir_from(yaw, 0.0);
            assert!(near(north_on_screen(yaw), [-fwd[0], fwd[2]]), "yaw {yaw}");
        }
    }
}

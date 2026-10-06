//! Characters the console draws geometrically instead of from the font:
//! box drawing (U+2500-257F), block elements (U+2580-259F), geometric
//! shapes, arrows and the VT100 scan lines. Drawn for the exact cell size,
//! lines run to the cell edges, so they join seamlessly with their
//! neighbors (which a font glyph, smaller than the cell, would not), the
//! way modern terminals render them.

/// Line weight of one arm of a box-drawing character.
#[derive(Clone, Copy, PartialEq)]
enum Line {
    None,
    Light,
    Heavy,
    Double,
}

use Line::{Double as D, Heavy as H, Light as L, None as N};

/// Arms (left, right, up, down) of a box-drawing character.
fn arms(ch: char) -> Option<[Line; 4]> {
    Some(match ch {
        '─' | '┄' | '┈' | '╌' => [L, L, N, N],
        '━' | '┅' | '┉' | '╍' => [H, H, N, N],
        '│' | '┆' | '┊' | '╎' => [N, N, L, L],
        '┃' | '┇' | '┋' | '╏' => [N, N, H, H],
        '┌' | '╭' => [N, L, N, L],
        '┏' => [N, H, N, H],
        '┐' | '╮' => [L, N, N, L],
        '┓' => [H, N, N, H],
        '└' | '╰' => [N, L, L, N],
        '┗' => [N, H, H, N],
        '┘' | '╯' => [L, N, L, N],
        '┛' => [H, N, H, N],
        '├' => [N, L, L, L],
        '┣' => [N, H, H, H],
        '┤' => [L, N, L, L],
        '┫' => [H, N, H, H],
        '┬' => [L, L, N, L],
        '┳' => [H, H, N, H],
        '┴' => [L, L, L, N],
        '┻' => [H, H, H, N],
        '┼' => [L, L, L, L],
        '╋' => [H, H, H, H],
        '═' => [D, D, N, N],
        '║' => [N, N, D, D],
        '╔' => [N, D, N, D],
        '╗' => [D, N, N, D],
        '╚' => [N, D, D, N],
        '╝' => [D, N, D, N],
        '╠' => [N, D, D, D],
        '╣' => [D, N, D, D],
        '╦' => [D, D, N, D],
        '╩' => [D, D, D, N],
        '╬' => [D, D, D, D],
        '╴' => [L, N, N, N],
        '╵' => [N, N, L, N],
        '╶' => [N, L, N, N],
        '╷' => [N, N, N, L],
        '╸' => [H, N, N, N],
        '╹' => [N, N, H, N],
        '╺' => [N, H, N, N],
        '╻' => [N, N, N, H],
        _ => return None,
    })
}

/// Whether the console draws `ch` itself.
pub fn is_special(ch: char) -> bool {
    matches!(ch as u32, 0x2190..=0x2193 | 0x23ba..=0x23bd | 0x2500..=0x259f | 0x25a0..=0x25ff)
}

/// Coverage (0-255) of pixel (x, y) in a w×h cell for a special character,
/// or None if the character is not one (then the font draws it).
pub fn coverage(ch: char, x: usize, y: usize, w: usize, h: usize) -> Option<u8> {
    let (xi, yi, wi, hi) = (x as i32, y as i32, w as i32, h as i32);
    let t = (wi / 12).max(1); // light line half width
    let on = |b: bool| if b { 255 } else { 0 };
    if let Some(a) = arms(ch) {
        return Some(on(box_pixel(a, xi, yi, wi, hi, t)));
    }
    // Fractions of the cell measured from the top or left.
    let rows = |num: i32, den: i32| yi * den >= hi * num;
    let cols = |num: i32, den: i32| xi * den < wi * num;
    let c = match ch {
        '▀' => on(!rows(1, 2)),
        '▁'..='▇' => on(rows(8 - (ch as i32 - '▀' as i32), 8)),
        '█' => 255,
        '▉'..='▏' => on(cols(8 - (ch as i32 - '█' as i32), 8)),
        '▐' => on(!cols(1, 2)),
        '░' => 64,
        '▒' => 128,
        '▓' => 192,
        '▔' => on(!rows(1, 8)),
        '▕' => on(!cols(7, 8)),
        '▖'..='▟' => {
            // Quadrants: bits for upper-left, upper-right, lower-left, lower-right.
            const Q: [u8; 10] = [0b0010, 0b0001, 0b1000, 0b1011, 0b1001, 0b1110, 0b1101, 0b0100, 0b0110, 0b0111];
            let bits = Q[(ch as u32 - '▖' as u32) as usize];
            let quadrant = match (cols(1, 2), rows(1, 2)) {
                (true, false) => 0b1000,
                (false, false) => 0b0100,
                (true, true) => 0b0010,
                (false, true) => 0b0001,
            };
            on(bits & quadrant != 0)
        }
        // VT100 scan lines 1, 3, 7 and 9.
        '⎺' => on((yi - hi / 10).abs() < t),
        '⎻' => on((yi - hi * 3 / 10).abs() < t),
        '⎼' => on((yi - hi * 7 / 10).abs() < t),
        '⎽' => on((yi - hi * 9 / 10).abs() < t),
        _ => return shape(ch, xi, yi, wi, hi, t),
    };
    Some(c)
}

fn box_pixel(a: [Line; 4], x: i32, y: i32, w: i32, h: i32, t: i32) -> bool {
    let (cx, cy) = (w / 2, h / 2);
    // Half widths: a band |d| < half is 2*half-1 pixels wide, so light
    // lines are t*2-1 pixels and heavy ones two pixels more.
    let half = |l: Line| match l {
        H => t + 1,
        _ => t,
    };
    // A double line is two light lines this far from the center line.
    let gap = t + 1;
    let band = |pos: i32, center: i32, l: Line| match l {
        N => false,
        D => (pos - (center - gap)).abs() < half(L) || (pos - (center + gap)).abs() < half(L),
        _ => (pos - center).abs() < half(l),
    };
    let [left, right, up, down] = a;
    // How far a horizontal arm reaches past the center: to the far side of
    // the vertical lines, so corners and junctions close.
    let reach_h = if up == D || down == D { gap + half(L) } else { half(up.max_w(down)) };
    let reach_v = if left == D || right == D { gap + half(L) } else { half(left.max_w(right)) };
    (band(y, cy, left) && x <= cx + reach_h)
        || (band(y, cy, right) && x >= cx - reach_h)
        || (band(x, cx, up) && y <= cy + reach_v)
        || (band(x, cx, down) && y >= cy - reach_v)
}

impl Line {
    /// The heavier of two arms (for how far crossing arms extend).
    fn max_w(self, other: Line) -> Line {
        let rank = |l: Line| match l {
            N => 0,
            L => 1,
            D => 2,
            H => 3,
        };
        if rank(self) >= rank(other) { self } else { other }
    }
}

/// Triangles, diamonds, circles, squares and arrows.
fn shape(ch: char, x: i32, y: i32, w: i32, h: i32, t: i32) -> Option<u8> {
    let on = |b: bool| if b { 255 } else { 0 };
    // Shapes sit in a square centered in the cell.
    let size = w.min(h) * 3 / 4;
    let (x0, y0) = ((w - size) / 2, (h - size) / 2);
    let (u, v) = (x - x0, y - y0); // coordinates in the square
    let inside_square = (0..size).contains(&u) && (0..size).contains(&v);
    // Point-in-triangle by edge functions; `outline` keeps only a border.
    let triangle = |p: [(i32, i32); 3], outline: bool| -> bool {
        let edge = |a: (i32, i32), b: (i32, i32)| ((b.0 - a.0) * (v - a.1) - (b.1 - a.1) * (u - a.0)) as i64;
        let len = |a: (i32, i32), b: (i32, i32)| (((b.0 - a.0) * (b.0 - a.0) + (b.1 - a.1) * (b.1 - a.1)) as i64).isqrt();
        let e = [edge(p[0], p[1]), edge(p[1], p[2]), edge(p[2], p[0])];
        let inside = e.iter().all(|&d| d >= 0) || e.iter().all(|&d| d <= 0);
        if !inside || !outline {
            return inside;
        }
        let l = [len(p[0], p[1]), len(p[1], p[2]), len(p[2], p[0])];
        (0..3).any(|i| e[i].abs() <= l[i] * t as i64)
    };
    let s = size - 1;
    let m = s / 2;
    let up = [(m, 0), (s, s), (0, s)];
    let down = [(0, 0), (s, 0), (m, s)];
    let right = [(0, 0), (s, m), (0, s)];
    let left = [(s, 0), (s, s), (0, m)];
    let small = |p: [(i32, i32); 3]| p.map(|(a, b)| (m + (a - m) / 2, m + (b - m) / 2));
    let (du, dv) = ((u - m).abs(), (v - m).abs());
    let r2 = |r: i32| (u - m) * (u - m) + (v - m) * (v - m) <= r * r;
    let c = match ch {
        '▲' => on(triangle(up, false)),
        '△' => on(triangle(up, true)),
        '▴' => on(triangle(small(up), false)),
        '▵' => on(triangle(small(up), true)),
        '▼' => on(triangle(down, false)),
        '▽' => on(triangle(down, true)),
        '▾' => on(triangle(small(down), false)),
        '▿' => on(triangle(small(down), true)),
        '▶' | '►' => on(triangle(right, false)),
        '▷' | '▻' => on(triangle(right, true)),
        '▸' => on(triangle(small(right), false)),
        '▹' => on(triangle(small(right), true)),
        '◀' | '◄' => on(triangle(left, false)),
        '◁' | '◅' => on(triangle(left, true)),
        '◂' => on(triangle(small(left), false)),
        '◃' => on(triangle(small(left), true)),
        '◆' => on(du + dv <= m),
        '◇' => on(du + dv <= m && du + dv > m - t * 2),
        '●' => on(r2(m)),
        '○' | '◯' => on(r2(m) && !r2(m - t)),
        '■' => on(inside_square),
        '□' => on(inside_square && (u < t || v < t || u > s - t || v > s - t)),
        '▪' => on(du <= m / 2 && dv <= m / 2),
        '▫' => on(du <= m / 2 && dv <= m / 2 && (du > m / 2 - t || dv > m / 2 - t)),
        // Arrows: a shaft through the cell and a head at the tip.
        '↑' | '↓' | '←' | '→' => {
            let (cx, cy) = (w / 2, h / 2);
            let head = w / 3;
            let vertical = matches!(ch, '↑' | '↓');
            let shaft = if vertical { (x - cx).abs() < (t + 1) / 2 && (h / 6..h - h / 6).contains(&y) } else {
                (y - cy).abs() < (t + 1) / 2 && (1..w - 1).contains(&x)
            };
            let tip = match ch {
                '↑' => (y - h / 6) >= 0 && (y - h / 6) <= head && (x - cx).abs() <= y - h / 6,
                '↓' => (h - h / 6 - y) >= 0 && (h - h / 6 - y) <= head && (x - cx).abs() <= h - h / 6 - y,
                '←' => x <= head && (y - cy).abs() <= x,
                _ => (w - 1 - x) <= head && (y - cy).abs() <= w - 1 - x,
            };
            on(shaft || tip)
        }
        _ => {
            if is_special(ch) {
                // Unknown character in a range we claim: a hollow box, so it
                // is visibly a placeholder.
                on(inside_square && (u < t || v < t || u > s - t || v > s - t))
            } else {
                return None;
            }
        }
    };
    Some(c)
}

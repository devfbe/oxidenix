//! The default keymap (German, as kbd's `de-latin1-nodeadkeys`), Linux's default function
//! strings (defkeymap.c's `func_buf`) and accent table (`accent_table`).

use crate::keysym::*;

/// The keymaps there are: plain, Shift, AltGr, Control, Shift+Control, Alt, Shift+Alt,
/// Control+Alt (indices: the modifiers' `KG_*` bits).
const TABLES: [usize; 8] = [0, 1, 2, 4, 5, 8, 9, 12];

const H: u16 = K_HOLE;

/// A Latin-1 character.
const fn l(c: u8) -> u16 {
    k(KT_LATIN, c)
}

/// A letter (Caps Lock takes the other case's keymap).
const fn let_(c: u8) -> u16 {
    k(KT_LETTER, c)
}

const fn meta(c: u8) -> u16 {
    k(KT_META, c)
}

/// A Unicode keysym (as KDGKBENT shows it).
const fn uni(c: u16) -> u16 {
    u(c)
}

/// A letter key: its control character, Meta with Alt.
const fn letter(c: u8, altgr: u16) -> [u16; 8] {
    let upper = c - 32;
    [let_(c), let_(upper), altgr, l(c & 0x1f), l(c & 0x1f), meta(c), meta(upper), meta(c & 0x1f)]
}

/// A key that is the same in every keymap.
const fn all(sym: u16) -> [u16; 8] {
    [sym; 8]
}

/// Function key `n` (0: F1): Shift adds ten, AltGr is console n + 13, Alt and
/// Control+Alt console n + 1.
const fn fkey(n: u8, shifted: u8) -> [u16; 8] {
    let f = k(KT_FN, n);
    [f, k(KT_FN, shifted), k(KT_CONS, n + 12), f, f, k(KT_CONS, n), k(KT_CONS, n), k(KT_CONS, n)]
}

/// A keypad key: Alt enters a decimal code with the digits.
const fn pad(v: u8, alt: u16) -> [u16; 8] {
    let p = k(KT_PAD, v);
    [p, p, p, p, p, alt, alt, p]
}

/// Columns: plain, Shift, AltGr, Control, Shift+Control, Alt, Shift+Alt, Control+Alt.
const ROWS: &[(u8, [u16; 8])] = &[
    (1, [l(0x1b), l(0x1b), H, l(0x1b), l(0x1b), meta(0x1b), meta(0x1b), H]),
    (2, [l(b'1'), l(b'!'), l(0xb9), H, H, meta(b'1'), meta(b'!'), H]),
    (3, [l(b'2'), l(b'"'), l(0xb2), l(0), l(0), meta(b'2'), meta(b'"'), H]),
    (4, [l(b'3'), l(0xa7), l(0xb3), l(0x1b), l(0x1b), meta(b'3'), H, H]),
    (5, [l(b'4'), l(b'$'), l(0xbc), l(0x1c), l(0x1c), meta(b'4'), meta(b'$'), H]),
    (6, [l(b'5'), l(b'%'), l(0xbd), l(0x1d), l(0x1d), meta(b'5'), meta(b'%'), H]),
    (7, [l(b'6'), l(b'&'), l(0xac), l(0x1e), l(0x1e), meta(b'6'), meta(b'&'), H]),
    (8, [l(b'7'), l(b'/'), l(b'{'), l(0x1f), l(0x1f), meta(b'7'), meta(b'/'), H]),
    (9, [l(b'8'), l(b'('), l(b'['), l(0x7f), l(0x7f), meta(b'8'), meta(b'('), H]),
    (10, [l(b'9'), l(b')'), l(b']'), H, H, meta(b'9'), meta(b')'), H]),
    (11, [l(b'0'), l(b'='), l(b'}'), H, H, meta(b'0'), meta(b'='), H]),
    (12, [l(0xdf), l(b'?'), l(b'\\'), l(0x1c), l(0x1c), H, meta(b'?'), H]),
    (13, [l(0xb4), l(b'`'), l(0xb8), H, H, H, meta(b'`'), H]),
    (14, [l(0x7f), l(0x7f), l(0x7f), l(0x08), l(0x08), meta(0x7f), meta(0x7f), H]),
    (15, [l(b'\t'), l(b'\t'), l(b'\t'), l(b'\t'), l(b'\t'), meta(b'\t'), meta(b'\t'), H]),
    (16, letter(b'q', l(b'@'))),
    (17, letter(b'w', H)),
    (18, letter(b'e', uni(0x20ac))),
    (19, letter(b'r', H)),
    (20, letter(b't', H)),
    (21, letter(b'z', H)),
    (22, letter(b'u', H)),
    (23, letter(b'i', H)),
    (24, letter(b'o', H)),
    (25, letter(b'p', H)),
    (26, [let_(0xfc), let_(0xdc), H, l(0x1b), l(0x1b), H, H, H]),
    (27, [l(b'+'), l(b'*'), l(b'~'), l(0x1d), l(0x1d), meta(b'+'), meta(b'*'), H]),
    (28, [K_ENTER, K_ENTER, K_ENTER, K_ENTER, K_ENTER, meta(0x0d), meta(0x0d), H]),
    (29, all(k(KT_SHIFT, KG_CTRL))),
    (30, letter(b'a', H)),
    (31, letter(b's', H)),
    (32, letter(b'd', H)),
    (33, letter(b'f', H)),
    (34, letter(b'g', H)),
    (35, letter(b'h', H)),
    (36, letter(b'j', H)),
    (37, letter(b'k', H)),
    (38, letter(b'l', H)),
    (39, [let_(0xf6), let_(0xd6), H, H, H, H, H, H]),
    (40, [let_(0xe4), let_(0xc4), H, H, H, H, H, H]),
    (41, [l(b'^'), l(0xb0), H, l(0x1e), l(0x1e), meta(b'^'), H, H]),
    (42, all(k(KT_SHIFT, KG_SHIFT))),
    (43, [l(b'#'), l(b'\''), H, l(0x1c), l(0x1c), meta(b'#'), meta(b'\''), H]),
    (44, letter(b'y', H)),
    (45, letter(b'x', H)),
    (46, letter(b'c', H)),
    (47, letter(b'v', H)),
    (48, letter(b'b', H)),
    (49, letter(b'n', H)),
    (50, letter(b'm', l(0xb5))),
    (51, [l(b','), l(b';'), H, H, H, meta(b','), meta(b';'), H]),
    (52, [l(b'.'), l(b':'), H, H, H, meta(b'.'), meta(b':'), H]),
    (53, [l(b'-'), l(b'_'), H, l(0x1f), l(0x1f), meta(b'-'), meta(b'_'), H]),
    (54, all(k(KT_SHIFT, KG_SHIFT))),
    (55, all(k(KT_PAD, K_PSTAR))),
    (56, all(k(KT_SHIFT, KG_ALT))),
    (57, [l(b' '), l(b' '), l(b' '), l(0), l(0), meta(b' '), meta(b' '), H]),
    (58, all(K_CAPS)),
    (59, fkey(0, 10)),
    (60, fkey(1, 11)),
    (61, fkey(2, 12)),
    (62, fkey(3, 13)),
    (63, fkey(4, 14)),
    (64, fkey(5, 15)),
    (65, fkey(6, 16)),
    (66, fkey(7, 17)),
    (67, fkey(8, 18)),
    (68, fkey(9, 19)),
    (69, [K_NUM, K_BARENUMLOCK, K_NUM, K_NUM, K_NUM, K_NUM, K_NUM, K_NUM]),
    (70, [K_HOLD, K_SH_MEM, K_SH_REGS, K_SH_STAT, K_HOLD, K_HOLD, K_HOLD, K_HOLD]),
    (71, pad(7, k(KT_ASCII, 7))),
    (72, pad(8, k(KT_ASCII, 8))),
    (73, pad(9, k(KT_ASCII, 9))),
    (74, all(k(KT_PAD, K_PMINUS))),
    (75, pad(4, k(KT_ASCII, 4))),
    (76, pad(5, k(KT_ASCII, 5))),
    (77, pad(6, k(KT_ASCII, 6))),
    (78, all(k(KT_PAD, K_PPLUS))),
    (79, pad(1, k(KT_ASCII, 1))),
    (80, pad(2, k(KT_ASCII, 2))),
    (81, pad(3, k(KT_ASCII, 3))),
    (82, pad(0, k(KT_ASCII, 0))),
    (83, {
        let p = k(KT_PAD, K_PCOMMA);
        [p, p, p, p, p, p, p, K_BOOT]
    }),
    (86, [l(b'<'), l(b'>'), l(b'|'), H, H, meta(b'<'), meta(b'>'), H]),
    (87, fkey(10, 30)),
    (88, fkey(11, 31)),
    (96, all(k(KT_PAD, K_PENTER))),
    (97, all(k(KT_SHIFT, KG_CTRL))),
    (98, all(k(KT_PAD, K_PSLASH))),
    (99, [l(0x1c), l(0x1c), l(0x1c), l(0x1c), l(0x1c), H, H, H]),
    (100, all(k(KT_SHIFT, KG_ALTGR))),
    (102, all(K_FIND)),
    (103, {
        let up = k(KT_CUR, K_UP);
        [up, up, up, up, up, K_SPAWNCONSOLE, up, up]
    }),
    (104, [K_PGUP, K_SCROLLBACK, K_PGUP, K_PGUP, K_PGUP, K_PGUP, K_PGUP, K_PGUP]),
    (105, {
        let left = k(KT_CUR, K_LEFT);
        [left, left, left, left, left, K_DECRCONSOLE, left, left]
    }),
    (106, {
        let right = k(KT_CUR, K_RIGHT);
        [right, right, right, right, right, K_INCRCONSOLE, right, right]
    }),
    (107, all(K_SELECT)),
    (108, all(k(KT_CUR, K_DOWN))),
    (109, [K_PGDN, K_SCROLLFORW, K_PGDN, K_PGDN, K_PGDN, K_PGDN, K_PGDN, K_PGDN]),
    (110, all(K_INSERT)),
    (111, [K_REMOVE, K_REMOVE, K_REMOVE, K_REMOVE, K_REMOVE, K_REMOVE, K_REMOVE, K_BOOT]),
    (119, all(K_PAUSE)),
    (127, all(K_COMPOSE)),
];

/// The default keymaps: for each table, its (keycode, keysym) entries.
pub fn maps() -> impl Iterator<Item = (usize, impl Iterator<Item = (u8, u16)>)> {
    TABLES.iter().enumerate().map(|(column, &table)| (table, ROWS.iter().map(move |(keycode, syms)| (*keycode, syms[column]))))
}

/// The function keys' strings: F1-F20, Find, Insert, Remove, Select, Prior, Next, Macro,
/// Help (none), Do (none), Pause.
pub const FUNCS: [&[u8]; 30] = [
    b"\x1b[[A", b"\x1b[[B", b"\x1b[[C", b"\x1b[[D", b"\x1b[[E", b"\x1b[17~", b"\x1b[18~", b"\x1b[19~", b"\x1b[20~",
    b"\x1b[21~", b"\x1b[23~", b"\x1b[24~", b"\x1b[25~", b"\x1b[26~", b"\x1b[28~", b"\x1b[29~", b"\x1b[31~", b"\x1b[32~",
    b"\x1b[33~", b"\x1b[34~", b"\x1b[1~", b"\x1b[2~", b"\x1b[3~", b"\x1b[4~", b"\x1b[5~", b"\x1b[6~", b"\x1b[M", b"",
    b"", b"\x1b[P",
];

/// Dead key, base, result (Latin-1).
pub const ACCENTS: [(u8, u8, u8); 68] = [
    (b'`', b'A', 0xc0), (b'`', b'a', 0xe0), (b'\'', b'A', 0xc1), (b'\'', b'a', 0xe1),
    (b'^', b'A', 0xc2), (b'^', b'a', 0xe2), (b'~', b'A', 0xc3), (b'~', b'a', 0xe3),
    (b'"', b'A', 0xc4), (b'"', b'a', 0xe4), (b'O', b'A', 0xc5), (b'o', b'a', 0xe5),
    (b'0', b'A', 0xc5), (b'0', b'a', 0xe5), (b'A', b'A', 0xc5), (b'a', b'a', 0xe5),
    (b'A', b'E', 0xc6), (b'a', b'e', 0xe6), (b',', b'C', 0xc7), (b',', b'c', 0xe7),
    (b'`', b'E', 0xc8), (b'`', b'e', 0xe8), (b'\'', b'E', 0xc9), (b'\'', b'e', 0xe9),
    (b'^', b'E', 0xca), (b'^', b'e', 0xea), (b'"', b'E', 0xcb), (b'"', b'e', 0xeb),
    (b'`', b'I', 0xcc), (b'`', b'i', 0xec), (b'\'', b'I', 0xcd), (b'\'', b'i', 0xed),
    (b'^', b'I', 0xce), (b'^', b'i', 0xee), (b'"', b'I', 0xcf), (b'"', b'i', 0xef),
    (b'-', b'D', 0xd0), (b'-', b'd', 0xf0), (b'~', b'N', 0xd1), (b'~', b'n', 0xf1),
    (b'`', b'O', 0xd2), (b'`', b'o', 0xf2), (b'\'', b'O', 0xd3), (b'\'', b'o', 0xf3),
    (b'^', b'O', 0xd4), (b'^', b'o', 0xf4), (b'~', b'O', 0xd5), (b'~', b'o', 0xf5),
    (b'"', b'O', 0xd6), (b'"', b'o', 0xf6), (b'/', b'O', 0xd8), (b'/', b'o', 0xf8),
    (b'`', b'U', 0xd9), (b'`', b'u', 0xf9), (b'\'', b'U', 0xda), (b'\'', b'u', 0xfa),
    (b'^', b'U', 0xdb), (b'^', b'u', 0xfb), (b'"', b'U', 0xdc), (b'"', b'u', 0xfc),
    (b'\'', b'Y', 0xdd), (b'\'', b'y', 0xfd), (b'T', b'H', 0xde), (b't', b'h', 0xfe),
    (b's', b's', 0xdf), (b'"', b'y', 0xff), (b's', b'z', 0xdf), (b'i', b'j', 0xff),
];

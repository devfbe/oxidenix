//! Linux's keysyms (linux/keyboard.h): a keysym is a type and a value, `K(t, v)`, as
//! KDGKBENT and KDSKBENT show it; a keymap holds `U(K(t, v))` (bit pattern 0xf000 set)
//! for typed keysyms and a Unicode code point below 0xf000 (`U+xxxx` in loadkeys) as it is.

/// `K(t, v)`.
pub const fn k(t: u8, v: u8) -> u16 {
    (t as u16) << 8 | v as u16
}

/// `U(x)`: between the keysym user space sees and a keymap's entry (both ways).
pub const fn u(x: u16) -> u16 {
    x ^ 0xf000
}

/// The keysym's type (`KTYP`) and value (`KVAL`).
pub const fn ktyp(x: u16) -> u8 {
    (x >> 8) as u8
}

pub const fn kval(x: u16) -> u8 {
    x as u8
}

pub const KT_LATIN: u8 = 0;
pub const KT_FN: u8 = 1;
pub const KT_SPEC: u8 = 2;
pub const KT_PAD: u8 = 3;
pub const KT_DEAD: u8 = 4;
pub const KT_CONS: u8 = 5;
pub const KT_CUR: u8 = 6;
pub const KT_SHIFT: u8 = 7;
pub const KT_META: u8 = 8;
pub const KT_ASCII: u8 = 9;
pub const KT_LOCK: u8 = 10;
pub const KT_LETTER: u8 = 11;
pub const KT_SLOCK: u8 = 12;
pub const KT_DEAD2: u8 = 13;
pub const KT_BRL: u8 = 14;
pub const NR_TYPES: u8 = 15;

/// The largest value of each type (Linux's `max_vals`).
pub const MAX_VALS: [u8; NR_TYPES as usize] = [255, 255, 19, 19, 5, 255, 3, 8, 255, 25, 7, 255, 7, 255, 10];

// KT_SPEC
pub const K_HOLE: u16 = k(KT_SPEC, 0);
pub const K_ENTER: u16 = k(KT_SPEC, 1);
pub const K_SH_REGS: u16 = k(KT_SPEC, 2);
pub const K_SH_MEM: u16 = k(KT_SPEC, 3);
pub const K_SH_STAT: u16 = k(KT_SPEC, 4);
pub const K_BREAK: u16 = k(KT_SPEC, 5);
pub const K_CONS: u16 = k(KT_SPEC, 6);
pub const K_CAPS: u16 = k(KT_SPEC, 7);
pub const K_NUM: u16 = k(KT_SPEC, 8);
pub const K_HOLD: u16 = k(KT_SPEC, 9);
pub const K_SCROLLFORW: u16 = k(KT_SPEC, 10);
pub const K_SCROLLBACK: u16 = k(KT_SPEC, 11);
pub const K_BOOT: u16 = k(KT_SPEC, 12);
pub const K_CAPSON: u16 = k(KT_SPEC, 13);
pub const K_COMPOSE: u16 = k(KT_SPEC, 14);
pub const K_SAK: u16 = k(KT_SPEC, 15);
pub const K_DECRCONSOLE: u16 = k(KT_SPEC, 16);
pub const K_INCRCONSOLE: u16 = k(KT_SPEC, 17);
pub const K_SPAWNCONSOLE: u16 = k(KT_SPEC, 18);
pub const K_BARENUMLOCK: u16 = k(KT_SPEC, 19);
/// The first entry of a map loaded by KDSKBENT (Linux's marker).
pub const K_ALLOCATED: u16 = k(KT_SPEC, 126);
/// KDGKBENT of entry 0 of a map that does not exist.
pub const K_NOSUCHMAP: u16 = k(KT_SPEC, 127);

// KT_FN: the function keys and the editing keys, indices of the function strings.
pub const K_F1: u16 = k(KT_FN, 0);
pub const K_FIND: u16 = k(KT_FN, 20);
pub const K_INSERT: u16 = k(KT_FN, 21);
pub const K_REMOVE: u16 = k(KT_FN, 22);
pub const K_SELECT: u16 = k(KT_FN, 23);
pub const K_PGUP: u16 = k(KT_FN, 24);
pub const K_PGDN: u16 = k(KT_FN, 25);
pub const K_MACRO: u16 = k(KT_FN, 26);
pub const K_HELP: u16 = k(KT_FN, 27);
pub const K_DO: u16 = k(KT_FN, 28);
pub const K_PAUSE: u16 = k(KT_FN, 29);

// KT_PAD values
pub const K_P0: u8 = 0;
pub const K_PPLUS: u8 = 10;
pub const K_PMINUS: u8 = 11;
pub const K_PSTAR: u8 = 12;
pub const K_PSLASH: u8 = 13;
pub const K_PENTER: u8 = 14;
pub const K_PCOMMA: u8 = 15;
pub const K_PDOT: u8 = 16;
pub const K_PPLUSMINUS: u8 = 17;
pub const K_PPARENL: u8 = 18;
pub const K_PPARENR: u8 = 19;

// KT_CUR values
pub const K_DOWN: u8 = 0;
pub const K_LEFT: u8 = 1;
pub const K_RIGHT: u8 = 2;
pub const K_UP: u8 = 3;

// KT_SHIFT values: the modifiers, bits of the shift state and of a keymap's index.
pub const KG_SHIFT: u8 = 0;
pub const KG_ALTGR: u8 = 1;
pub const KG_CTRL: u8 = 2;
pub const KG_ALT: u8 = 3;
pub const KG_SHIFTL: u8 = 4;
pub const KG_SHIFTR: u8 = 5;
pub const KG_CTRLL: u8 = 6;
pub const KG_CTRLR: u8 = 7;
pub const KG_CAPSSHIFT: u8 = 8;
pub const NR_SHIFT: usize = 9;

/// The diacritics of KT_DEAD's values.
pub const DEAD_CHARS: [u8; 6] = [b'`', b'\'', b'^', b'~', b'"', b','];

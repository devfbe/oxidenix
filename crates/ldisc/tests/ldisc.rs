//! The line discipline against Linux's N_TTY behavior: canonical editing and its echo,
//! end of file, literal next, signals and flushing, input mapping, noncanonical reads,
//! mode switches, flow control, output processing and the buffer's limits.

use ldisc::*;

fn feed(l: &mut Ldisc, input: &[u8]) -> (Vec<u8>, Vec<Received>) {
    let mut echo = Vec::new();
    let mut rs = Vec::new();
    for &b in input {
        rs.push(l.receive(b, &mut echo));
    }
    (echo, rs)
}

/// Reads like one read(2) of `max` bytes in canonical mode (one line at most), or
/// everything there in noncanonical mode.
fn read(l: &mut Ldisc, max: usize) -> Option<Vec<u8>> {
    let take = l.take(max)?;
    let mut out = vec![0u8; take.copy];
    l.peek(&mut out);
    l.consume(take.consume);
    Some(out)
}

fn std() -> Ldisc {
    Ldisc::new(Termios::default())
}

fn with(f: impl FnOnce(&mut Termios)) -> Ldisc {
    let mut t = Termios::default();
    f(&mut t);
    Ldisc::new(t)
}

#[test]
fn defaults_are_linux_tty_std_termios() {
    let t = Termios::default();
    assert_eq!(t.iflag, ICRNL | IXON);
    assert_eq!(t.oflag, OPOST | ONLCR);
    assert_eq!(t.lflag, ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN);
    assert_eq!(&t.cc[..17], b"\x03\x1c\x7f\x15\x04\x00\x01\x00\x11\x13\x1a\x00\x12\x0f\x17\x16\x00");
}

#[test]
fn termios_bytes_round_trip() {
    let mut t = Termios::default();
    t.lflag &= !ECHO;
    t.cc[VMIN] = 5;
    t.ospeed = 9600;
    let b = t.to_bytes();
    assert_eq!(Termios::default().with_bytes(&b), t);
    // struct termios keeps the speeds it had.
    let short = Termios::default().with_bytes(&b[..TERMIOS_SIZE]);
    assert_eq!(short.ospeed, 38400);
    assert_eq!(short.cc[VMIN], 5);
}

#[test]
fn canonical_reads_a_line_at_a_time() {
    let mut l = std();
    let (echo, _) = feed(&mut l, b"ab\rcd");
    assert_eq!(echo, b"ab\r\ncd");
    assert!(l.readable(false));
    assert_eq!(l.available(), 3);
    assert_eq!(read(&mut l, 100).unwrap(), b"ab\n");
    // The half-typed line is not readable.
    assert!(!l.readable(false));
    assert!(read(&mut l, 100).is_none());
    feed(&mut l, b"\n");
    // A short read takes part of the line; the rest stays.
    assert_eq!(read(&mut l, 1).unwrap(), b"c");
    assert_eq!(read(&mut l, 100).unwrap(), b"d\n");
}

#[test]
fn eof_ends_a_line_and_alone_reads_zero() {
    let mut l = std();
    let (echo, _) = feed(&mut l, b"x\x04\x04");
    assert_eq!(echo, b"x");
    assert_eq!(l.available(), 1);
    assert_eq!(read(&mut l, 100).unwrap(), b"x");
    assert_eq!(read(&mut l, 100).unwrap(), b"");
    assert!(read(&mut l, 100).is_none());
}

#[test]
fn erase_kill_and_werase_echo_like_linux() {
    let mut l = std();
    let (echo, _) = feed(&mut l, b"abc\x7f");
    assert_eq!(echo, b"abc\x08 \x08");
    let (echo, _) = feed(&mut l, b"\x15");
    // ECHOKE: the line is erased visually.
    assert_eq!(echo, b"\x08 \x08\x08 \x08");
    let (echo, _) = feed(&mut l, b"one two_3  \x17");
    assert_eq!(&echo[11..], b"\x08 \x08\x08 \x08\x08 \x08\x08 \x08\x08 \x08\x08 \x08\x08 \x08");
    feed(&mut l, b"\n");
    assert_eq!(read(&mut l, 100).unwrap(), b"one \n");
    // Control characters echo as ^X and erase two columns.
    let (echo, _) = feed(&mut l, b"\x01\x7f");
    assert_eq!(echo, b"^A\x08 \x08\x08 \x08");
    // Without ECHOKE: the kill character and a newline.
    let mut l = with(|t| t.lflag &= !ECHOKE);
    let (echo, _) = feed(&mut l, b"ab\x15");
    assert_eq!(echo, b"ab^U\r\n");
    // Without ECHOE: erase echoes the erase character.
    let mut l = with(|t| t.lflag &= !(ECHOE | ECHOCTL));
    let (echo, _) = feed(&mut l, b"ab\x7f\n");
    assert_eq!(echo, b"ab\x7f\r\n");
    assert_eq!(read(&mut l, 100).unwrap(), b"a\n");
}

#[test]
fn erase_takes_whole_utf8_characters_with_iutf8() {
    let mut l = with(|t| t.iflag |= IUTF8);
    let (echo, _) = feed(&mut l, "aä€\x7f\x7f\n".as_bytes());
    assert_eq!(&echo[6..], b"\x08 \x08\x08 \x08\r\n");
    assert_eq!(read(&mut l, 100).unwrap(), b"a\n");
    // Without IUTF8 one byte goes.
    let mut l = std();
    feed(&mut l, "ä\x7f\n".as_bytes());
    assert_eq!(read(&mut l, 100).unwrap(), b"\xc3\n");
}

#[test]
fn tabs_are_erased_back_to_their_column() {
    let mut l = std();
    let (echo, _) = feed(&mut l, b"ab\t\x7f");
    // The tab took columns 2..8: six back.
    assert_eq!(&echo[3..], b"\x08\x08\x08\x08\x08\x08");
    assert_eq!(l.column(), 2);
    // The line started at column 5 (a prompt).
    let mut l = std();
    let mut out = Vec::new();
    l.output(b"$ >> ", &mut out);
    let (echo, _) = feed(&mut l, b"x\t\x7f");
    assert_eq!(&echo[2..], b"\x08\x08");
}

#[test]
fn literal_next_and_reprint() {
    let mut l = std();
    let (echo, _) = feed(&mut l, b"\x16\x03\x16\x7fz");
    assert_eq!(echo, b"^\x08^C^\x08^?z");
    let (echo, _) = feed(&mut l, b"\x12");
    assert_eq!(echo, b"^R\r\n^C^?z");
    feed(&mut l, b"\n");
    assert_eq!(read(&mut l, 100).unwrap(), b"\x03\x7fz\n");
}

#[test]
fn signals_flush_unless_noflsh() {
    let mut l = std();
    feed(&mut l, b"abc\ndef");
    let (echo, rs) = feed(&mut l, b"\x03");
    assert_eq!(rs[0], Received { signal: Some(SIGINT), flushed: true, dropped: false });
    assert_eq!(echo, b"^C");
    assert!(!l.readable(false));
    let (_, rs) = feed(&mut l, b"\x1c\x1a");
    assert_eq!(rs[0].signal, Some(SIGQUIT));
    assert_eq!(rs[1].signal, Some(SIGTSTP));
    let mut l = with(|t| t.lflag |= NOFLSH);
    feed(&mut l, b"abc\n");
    let (_, rs) = feed(&mut l, b"\x03");
    assert!(!rs[0].flushed);
    assert_eq!(read(&mut l, 100).unwrap(), b"abc\n");
    // Without ISIG the characters are data.
    let mut l = with(|t| t.lflag &= !(ISIG | ICANON));
    let (_, rs) = feed(&mut l, b"\x03");
    assert_eq!(rs[0].signal, None);
    assert_eq!(read(&mut l, 100).unwrap(), b"\x03");
}

#[test]
fn input_mapping() {
    let mut l = with(|t| t.iflag = INLCR | IGNCR);
    feed(&mut l, b"a\rb\n");
    // IGNCR drops CR; INLCR turns NL into CR, which ends no line.
    assert!(!l.readable(false));
    let mut l = with(|t| {
        t.iflag = ISTRIP;
        t.lflag &= !ICANON;
    });
    feed(&mut l, b"\xe1");
    assert_eq!(read(&mut l, 10).unwrap(), b"a");
    let mut l = with(|t| t.iflag = 0);
    feed(&mut l, b"x\r\n");
    assert_eq!(read(&mut l, 10).unwrap(), b"x\r\n");
}

#[test]
fn noncanonical_reads_and_poll_with_vmin() {
    let mut l = with(|t| {
        t.lflag &= !(ICANON | ECHO);
        t.cc[VMIN] = 3;
        t.cc[VTIME] = 0;
    });
    assert_eq!(l.mode(), ReadMode::Raw { min: 3, time: 0 });
    feed(&mut l, b"ab");
    assert!(l.readable(false));
    // poll waits for VMIN bytes when VTIME is 0.
    assert!(!l.readable(true));
    feed(&mut l, b"c");
    assert!(l.readable(true));
    assert_eq!(read(&mut l, 2).unwrap(), b"ab");
    assert_eq!(read(&mut l, 2).unwrap(), b"c");
}

#[test]
fn raw_echo_shows_control_characters() {
    let mut l = with(|t| t.lflag &= !ICANON);
    let (echo, _) = feed(&mut l, b"a\r\n\x01");
    // CR mapped by ICRNL echoes as a newline, a typed NL as ^J.
    assert_eq!(echo, b"a\r\n^J^A");
}

#[test]
fn switching_modes_moves_input() {
    let mut l = std();
    feed(&mut l, b"one\nhalf\x04tw");
    let mut raw = l.termios();
    raw.lflag &= !ICANON;
    l.set_termios(raw);
    // The lines and the half-typed line are raw input; the EOF mark is gone.
    assert_eq!(read(&mut l, 100).unwrap(), b"one\nhalftw");
    feed(&mut l, b"xy");
    let mut canon = l.termios();
    canon.lflag |= ICANON;
    l.set_termios(canon);
    assert_eq!(read(&mut l, 100).unwrap(), b"xy");
}

#[test]
fn flow_control() {
    let mut l = std();
    feed(&mut l, b"\x13");
    assert!(l.stopped());
    feed(&mut l, b"a");
    assert!(l.stopped());
    feed(&mut l, b"\x11");
    assert!(!l.stopped());
    // IXANY: any character restarts.
    let mut l = with(|t| t.iflag |= IXANY);
    feed(&mut l, b"\x13a");
    assert!(!l.stopped());
    // tcflow(TCOOFF) is not undone by VSTART.
    let mut l = std();
    l.stop(true);
    feed(&mut l, b"\x11");
    assert!(l.stopped());
    l.start(true);
    assert!(!l.stopped());
    // Turning IXON off restarts output stopped by VSTOP.
    feed(&mut l, b"\x13");
    let mut t = l.termios();
    t.iflag &= !IXON;
    assert!(l.set_termios(t));
    assert!(!l.stopped());
}

#[test]
fn output_processing() {
    let mut l = std();
    let mut out = Vec::new();
    l.output(b"a\nb\tc", &mut out);
    assert_eq!(out, b"a\r\nb\tc");
    assert_eq!(l.column(), 9);
    let mut l = with(|t| t.oflag = OPOST | XTABS | ONOCR | OCRNL);
    let mut out = Vec::new();
    l.output(b"\rab\tx\r", &mut out);
    assert_eq!(out, b"ab      x\n");
    let mut l = with(|t| t.oflag = OPOST | OLCUC);
    let mut out = Vec::new();
    l.output(b"abc", &mut out);
    assert_eq!(out, b"ABC");
    // Without OPOST nothing changes.
    let mut l = with(|t| t.oflag = ONLCR);
    let mut out = Vec::new();
    l.output(b"a\n", &mut out);
    assert_eq!(out, b"a\n");
}

#[test]
fn output_columns_can_be_put_back() {
    // Output processed for a write that then did not go out leaves no trace.
    let mut l = std();
    let mut out = Vec::new();
    l.output(b"ab", &mut out);
    let before = l.columns();
    l.output(b"cd\tx", &mut out);
    let after = l.columns();
    assert_eq!(after.0, 9);
    l.set_columns(before);
    assert_eq!(l.column(), 2);
    l.set_columns(after);
    // A tab typed now erases back to where the line began after it.
    let (echo, _) = feed(&mut l, b"\t\x7f");
    assert_eq!(&echo[1..], b"\x08\x08\x08\x08\x08\x08\x08");
}

#[test]
fn buffer_limits() {
    // Noncanonical: input beyond 4095 bytes is dropped, room() says when.
    let mut l = with(|t| t.lflag &= !(ICANON | ECHO));
    let (_, rs) = feed(&mut l, &[b'x'; BUF_SIZE]);
    assert!(rs[BUF_SIZE - 2].dropped == false && rs[BUF_SIZE - 1].dropped);
    assert!(!l.room());
    assert_eq!(l.available(), BUF_SIZE - 1);
    // Canonical: a full line still takes its end, in place of its last byte.
    let mut l = with(|t| t.lflag &= !ECHO);
    feed(&mut l, &[b'y'; BUF_SIZE + 10]);
    assert!(l.room());
    feed(&mut l, b"\n");
    let line = read(&mut l, 10000).unwrap();
    assert_eq!(line.len(), BUF_SIZE - 1);
    assert_eq!(line.last(), Some(&b'\n'));
}

#[test]
fn echoprt_and_echonl() {
    let mut l = with(|t| t.lflag = (t.lflag | ECHOPRT) & !ECHOKE);
    let (echo, _) = feed(&mut l, b"ab\x7f\x7fc");
    assert_eq!(echo, b"ab\\ba/c");
    let mut l = with(|t| t.lflag = (t.lflag & !ECHO) | ECHONL);
    let (echo, _) = feed(&mut l, b"secret\n");
    assert_eq!(echo, b"\r\n");
}

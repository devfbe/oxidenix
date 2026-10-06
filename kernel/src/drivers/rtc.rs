//! CMOS real-time clock: read once at boot; wall-clock time afterwards is
//! the boot time plus the timer ticks since then.

use spin::Once;
use x86_64::instructions::port::Port;

static BOOT_TIME: Once<u64> = Once::new();

fn cmos(reg: u8) -> u8 {
    unsafe {
        Port::<u8>::new(0x70).write(reg);
        Port::<u8>::new(0x71).read()
    }
}

fn update_in_progress() -> bool {
    cmos(0x0a) & 0x80 != 0
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn read_clock() -> u64 {
    // Read until two consecutive reads agree, so no update tears the value.
    let read = || {
        while update_in_progress() {}
        [0x00, 0x02, 0x04, 0x07, 0x08, 0x09].map(cmos)
    };
    let mut regs = read();
    loop {
        let again = read();
        if again == regs {
            break;
        }
        regs = again;
    }
    let status_b = cmos(0x0b);
    let bcd = |v: u8| if status_b & 0x04 == 0 { (v & 0x0f) + (v >> 4) * 10 } else { v };
    let [sec, min, hour_raw, day, month, year] = regs;
    let pm = hour_raw & 0x80 != 0;
    let mut hour = bcd(hour_raw & 0x7f);
    if status_b & 0x02 == 0 && pm {
        hour = (hour % 12) + 12;
    }
    let year = 2000 + bcd(year) as i64;
    let days = days_from_civil(year, bcd(month) as i64, bcd(day) as i64);
    (days * 86400 + hour as i64 * 3600 + bcd(min) as i64 * 60 + bcd(sec) as i64) as u64
}

pub fn init() {
    BOOT_TIME.call_once(read_clock);
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    let boot = BOOT_TIME.get().copied().unwrap_or(0);
    boot + crate::process::ticks() / crate::process::TIMER_HZ
}

/// Wall-clock time as (seconds, nanoseconds).
pub fn now_precise() -> (u64, u64) {
    let boot = BOOT_TIME.get().copied().unwrap_or(0);
    let ticks = crate::process::ticks();
    let hz = crate::process::TIMER_HZ;
    (boot + ticks / hz, (ticks % hz) * (1_000_000_000 / hz))
}

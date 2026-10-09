//! What procfs gives each client: one instance cannot take the channels or
//! the mapped grants the others need, and what goes is given back.

use procproto::admission::*;

#[test]
fn an_instance_takes_its_share_of_channels_only() {
    let mut c = Channels::default();
    let a = c.admit(1).unwrap();
    let b = c.admit(1).unwrap();
    assert_ne!(a, b);
    assert_eq!(c.admit(1), Err(ENOSPC));
    // The others keep every other slot.
    for instance in 2..MAX_CHANNELS as u64 {
        c.admit(instance).unwrap();
    }
    assert_eq!(c.held_by(1), CHANNELS_PER_INSTANCE);
    // Full: no instance gets more, not even a new one.
    assert_eq!(c.admit(1000), Err(ENOSPC));
    c.release(a);
    assert_eq!(c.admit(1), Ok(a));
}

#[test]
fn many_instances_each_get_a_channel() {
    let mut c = Channels::default();
    for instance in 1..=MAX_CHANNELS as u64 {
        assert!(c.admit(instance).is_ok(), "instance {instance}");
    }
    assert_eq!(c.admit(MAX_CHANNELS as u64 + 1), Err(ENOSPC));
}

#[test]
fn released_slots_and_unknown_slots() {
    let mut c = Channels::default();
    let s = c.admit(7).unwrap();
    c.release(s);
    c.release(s);
    c.release(MAX_CHANNELS + 5);
    assert_eq!(c.held_by(7), 0);
}

#[test]
fn grants_are_bounded_in_count_and_pages() {
    let mut g = Grants::default();
    assert_eq!(g.charge(GRANT_PAGES_PER_CHANNEL + 1), Err(ENOMEM));
    g.charge(GRANT_PAGES_PER_CHANNEL - 1).unwrap();
    assert_eq!(g.charge(2), Err(ENOMEM));
    g.charge(1).unwrap();
    g.uncharge(GRANT_PAGES_PER_CHANNEL - 1);
    g.uncharge(1);
    for _ in 0..GRANTS_PER_CHANNEL {
        g.charge(1).unwrap();
    }
    assert_eq!(g.may_map(), Err(ENOMEM));
    assert_eq!(g.room(), Err(ENOMEM));
    assert_eq!(g.charge(0), Err(ENOMEM));
    g.uncharge(1);
    assert_eq!(g.may_map(), Ok(()));
}

#[test]
fn the_room_is_the_limit_to_map_with() {
    let mut g = Grants::default();
    assert_eq!(g.room(), Ok(GRANT_PAGES_PER_CHANNEL));
    g.charge(200).unwrap();
    assert_eq!(g.room(), Ok(GRANT_PAGES_PER_CHANNEL - 200));
    g.charge(GRANT_PAGES_PER_CHANNEL - 200).unwrap();
    // No pages left: nothing more is mapped, whatever its size.
    assert_eq!(g.room(), Err(ENOMEM));
    g.uncharge(16);
    assert_eq!(g.room(), Ok(16));
}

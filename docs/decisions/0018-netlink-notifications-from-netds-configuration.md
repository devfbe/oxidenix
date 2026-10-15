# ADR 0018: Netlink notifications from netd's configuration: a change counter, announced by difference

Date: 2026-10-15. Status: accepted.

## Context

Programs learn of network configuration changes by joining rtnetlink's multicast groups
(`RTNLGRP_LINK`, `RTNLGRP_IPV4_IFADDR`, `RTNLGRP_IPV4_ROUTE`: `ip monitor`, network managers,
libraries that wait for an address). The configuration is netd's (DHCP), shared by every
instance; the netlink sockets are each instance's Linux server's (`servers/linux/src/netlink.rs`),
which so far only answered dumps from netd's `LINKS` (ADR 0008's channel). Until now nothing
was ever delivered to a group.

What Linux sends is a message per event: an address that goes is `RTM_DELADDR` and its routes'
`RTM_DELROUTE`s, a new lease `RTM_NEWADDR` and `RTM_NEWROUTE`s, a carrier change
`RTM_NEWLINK`. The instances must hear of netd's changes without a request of their own, while
none of their threads waits, and netd answers every request at once and keeps nothing per
instance that grows (ADR 0008).

Alternatives considered:

- **An event queue in the shared area**, netd appending a record per change for each channel:
  exact, but a queue per channel with overflow handling, and netd writing variable data into
  every instance's area for a rare event.
- **A pending request** the net thread keeps in netd, completed at the next change: breaks
  "netd answers at once", and needs cancellation when the channel goes.
- **The kernel relaying netd's announcements** (as it once relayed interface records):
  removed with R9 for good reasons; the channel is the only path between them.
- **A change counter and the instance computing the difference**: netd advances a counter in
  each channel's header page and pokes the net thread; the instance fetches `LINKS` and
  compares it with the configuration it last announced.

## Decision

- netd advances `ArenaHeader::links_seq` and pokes the net thread whenever its configuration
  changes (a lease comes or goes). `LINKS` carries everything the routes follow from (the
  default gateway joined the address, prefix and link state).
- The instance keeps the interfaces it last announced while any netlink socket is a member of
  a group (none without members). The net thread, when the counter moves, when a channel comes
  (netd may have started again), and when the channel dies, fetches the configuration and sends
  the members `netlink::changes(old, new)`: Linux's messages in Linux's order (routes, then
  addresses, then links that went; links, addresses, routes that came). A dead channel whose
  netd cannot be reached means no interfaces (what a dump would show): everything is announced
  gone, and the net thread looks for netd again every second while members wait.
- The routes are derived, not netd's: `netlink::routes` makes the main table's subnet and
  default routes and the local table's own addresses from the interfaces, so dumps
  (`RTM_GETROUTE`) and notifications agree by construction.
- For the self-tests netd takes `RENEW` (its lease goes, DHCP asks again a second later) only
  when the kernel started it in test mode (`test=1`): one instance can never take the others'
  network away.

## Consequences

- Nothing per event crosses the channel; the cost is one `LINKS` request per change per
  instance with members, and none for instances without.
- Changes that come and go between two looks of the net thread are not seen (a lease lost and
  regained within one round: the instance sees no difference). Linux would announce both;
  since the state before and after is the same, a program that tracks the configuration is
  not misled, but one that counts events would see fewer.
- `nettest` checks it end to end with a renewal (`TEST_NET_RENEW`); `crates/netlink` checks
  the messages and their order on the host.

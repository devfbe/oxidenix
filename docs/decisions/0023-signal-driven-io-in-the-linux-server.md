# ADR 0023: Signal-driven I/O in the Linux server, on the descriptions' watches

Date: 2026-10-16. Status: accepted.

## Context

`O_ASYNC` was a flag F_SETFL ignored: `F_SETOWN` and `F_SETOWN_EX` were kept (a socket's
`SIGURG` used them), but no file ever sent `SIGIO`, `F_SETSIG` was `EINVAL` and `FIOASYNC`
`ENOTTY`. Programs that drive their I/O by signals (the classic `SIGIO` servers, terminal
programs that read the keyboard by `SIGIO`, and the real-time-signal event loops that
`F_SETSIG` was made for) therefore never woke up.

Linux's model (fs/fcntl.c, `man 2 fcntl`, `man 7 socket`): a file whose driver has a
`fasync` method (pipes and FIFOs, sockets, terminals and pty masters, inotify and fanotify,
some devices; not eventfd, timerfd, signalfd, epoll, regular files) keeps a list of
`fasync_struct` entries, one per open file with `FASYNC`, each naming the descriptor it was
turned on with. The driver calls `kill_fasync(list, SIGIO, reason)` at chosen points, with a
reason (`POLL_IN`, `POLL_OUT`, `POLL_ERR`, `POLL_PRI`, `POLL_HUP`); `send_sigio` then signals
the file's owner (`f_owner`: a thread, a process or a process group) with `SIGIO`
(`SI_KERNEL`), or with the `F_SETSIG` signal and a siginfo (si_code the reason, si_band its
poll events, si_fd the descriptor), and with `SIGIO` instead when that signal cannot be
queued (a real-time signal beyond `RLIMIT_SIGPENDING`). The points and their gates are each
driver's: a pipe signals its readers at the end of every write and its writers at the end of
every read; a socket signals new data unless a receive sleeps for it
(`SOCKWQ_ASYNC_WAITDATA`) and room only after a send found none or poll found it unwritable
(`SOCKWQ_ASYNC_NOSPACE`); a terminal signals input when readers can take it (a canonical
line's end) and room only after a write left output undone (`TTY_DO_WRITE_WAKEUP`); inotify
every event queued (not merged).

The server already had the place where a description's listeners meet: its watch
(`poll::Watch`, Linux's wait queue), found by the description's id from any thread (the
calling thread, the net thread, the service threads), which every file reports to.

## Decision

- **The O_ASYNC entry is part of the watch.** `Watch::set_async` puts the entry (the
  descriptor) under a leaf lock of its own and sets `O_ASYNC` with it; turning it off, the
  description's end (`poll::forget`, the first thing its end does) and a terminal's hangup
  take it out again (a hung-up terminal's file shows `O_ASYNC` off from then on,
  `Description::status_flags`). The description's owner and F_SETSIG signal (`files::Fown`,
  Linux's fown_struct) live in the watch too, so a file's event, which knows only the
  description's id, finds everything there and never touches the description itself (which
  may be ending on another thread). A global count of entries (`ASYNC`) makes the files'
  calls a single load while no description of the instance has `O_ASYNC`; the readiness
  reports are untouched.
- **Each file calls `files::sigio(id, Io)` where Linux's driver calls kill_fasync**, with the
  same reason and the same gate, not derived from the readiness reports (whose events are
  EPOLLET's edges, a different set: a pipe read that found nothing still signals the
  writers; a socket's room signals only after `NOSPACE`). Pipes (`pipe.rs`: end of each read
  and write and before each sleep, the last reader's or writer's close), AF_UNIX sockets
  (`unix.rs`: data, connections, the peer's shutdown and close, room with `NOSPACE`, the
  stream receives' `WAITDATA`), internet sockets (`inet.rs`: the net thread derives Linux's
  events from what netd changed: data, a connection to accept, a connect done, room with
  `NOSPACE`, the peer's FIN, an error; urgent data's `POLL_PRI` with an F_SETSIG signal only,
  `SIGURG` itself as before), netlink sockets (datagrams, overruns), terminals and pty masters
  (`tty.rs`: a canonical line's end, any input in noncanonical mode, the slave's output for
  the master, room after an undone write, both ways; `ldisc::Received::line_end`), inotify
  (each event queued, the overflow event when the loss happens).
- **Sending is the signal module's** (`signal::send_sigio`): the owner, the signal and the
  descriptor are read under the entry's lock and sent after it, under the process lock, which
  comes after every file's lock (the process code already reports to watches only after
  letting go of it). A real-time `F_SETSIG` signal is posted with `may_fail`: beyond
  `RLIMIT_SIGPENDING` (or without memory) the target gets `SIGIO` instead; a signal with
  codes of its own (`SIGCHLD`, the faults) gets `SI_SIGIO` as si_code. The permission check
  (`sigio_perm`) always passes: everyone is root.
- **fcntl and ioctl as Linux's**: `F_SETFL` turns `O_ASYNC` on or off only on a change, and
  only for a file with signal-driven I/O (for the others the flag stays as it is, as Linux's
  `SETFL_MASK` has no `FASYNC`; `O_ASYNC` given to open(2) is a flag without an entry);
  `FIOASYNC` is `ENOTTY` for those; a hung-up terminal refuses both (`ENOTTY`). `F_SETSIG`
  takes 0 to 64, `F_GETSIG` returns it. `F_GETOWN` and `F_GETOWN_EX` give 0 once the owner is
  gone. Turning `O_ASYNC` on for a terminal without an owner makes its foreground process
  group, or the caller, the owner (`__tty_fasync`). Sockets take `FIOSETOWN`/`SIOCSPGRP`
  and `FIOGETOWN`/`SIOCGPGRP` (`sock_ioctl`).

## Consequences

Signal-driven I/O works for every file Linux gives it to, with Linux's signals, codes and
queueing (the test `sigiotest` runs unchanged on Linux and here). It costs nothing while no
description has `O_ASYNC`; with one, an event costs a lookup in the watches' table and, for
a description with the entry, the process lock. Not done: the owner is held by its number
(as `SIGURG`'s was), not by identity as Linux's counted `struct pid`: once the owner is
reaped and its number given out again (after the 32768 of `pid_max`), the new process would
get the signals. `F_NOTIFY` (dnotify) and `F_SETLEASE`, which also signal the owner, and
`POLL_MSG` (no file here sends it) are not implemented.

# ADR 0007: Terminals in the Linux server: a raw console device, devices by number, the controlling terminal per session

Date: 2026-10-09. Status: accepted.

## Context

ADR 0004 moves the tty layer into the Linux server and leaves the kernel the console as a
device. Phase R6d carries it out (docs/design/linux-server.md, "The terminal"). Four questions
were not settled by ADR 0004: what exactly the kernel's device does, how the server learns
that a node is a terminal, where the controlling terminal lives while processes and sessions
are still the kernel's (until R8), and how the terminal layer can be tested when QEMU's serial
port carries no input.

## Decision

- **The console device is raw and granted.** The kernel moves bytes and nothing else: output
  to the framebuffer console's VT100 interpreter and the serial mirror unchanged (a line feed
  keeps the column, as on a VT; `ONLCR` is the terminal's, and the kernel's own messages add
  their carriage returns), input from the keyboard and the console's answers in a ring. It is
  held by one instance at a time: the tree the kernel starts gets it and holds it until its
  first process ends; then the monitor holds it. Input reaches the holder's service thread as
  an event set from the keyboard interrupt by a flag and a wakeup, never through a lock of the
  instance.
- **Devices are named by number.** A character device node's number selects its driver, as on
  Linux: (5,0), (5,1), (5,2) and (136,n) are the server's terminals wherever the node is (the
  kernel's `/dev`, which now reports `st_rdev`, or the server's devpts); the others stay the
  kernel's. No path is special.
- **The controlling terminal belongs to a session.** The terminal records the session it
  controls; a process's controlling terminal is the terminal recording its session. That is
  exact for everything but `TIOCNOTTY` by a process that does not lead its session (it
  dissociates the whole session only when its leader does), and needs no per-process state in
  the server before R8. The kernel reports a session leader's exit (`EVENT_SESSION_END`), and
  answers the queries job control needs (`proc_ids`, `signal_state`) and sends the terminal's
  signals (`signal_group`), all within the caller's instance; these calls go with R8.
- **The first process of a tree gets the console as its controlling terminal**, which Linux
  never gives through `/dev/console` (a getty or busybox's `cttyhack` does it there); without
  it the interactive shell had no job control.
- **Pseudo-terminals come with R6d**, not later: with the line discipline generic over its
  driver they cost a master file and a devpts, and they are how the terminal layer is tested
  (the keyboard is the console's only input). devpts is a tmpfs of the server's whose names
  only the server makes.

## Consequences

The kernel loses its line discipline, its terminal ioctls and its foreground group; Ctrl+C
and Ctrl+Z are signals the server sends. Background processes of a tree whose first process
ended find their terminal hung up (reads 0, writes EIO), as on Linux after the session's
leader exits. Programs that drive the console with `\n` in raw mode (ncurses with
`TERM=linux` moves down with `cud1=^J`) now get a line feed, not a new line. Packet mode,
the console's VT ioctls and serial input are not done; serial input would be a second input
source of the same device.

## Update (R8, ADR 0010)

The transitional calls went with R8: the terminal asks the server's process table for
process groups, sessions and orphaned groups, sends its signals through the server's signal
code, and learns of a session leader's end from the process model.

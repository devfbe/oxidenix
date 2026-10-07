# ADR 0002: One Linux server instance per process tree

Date: 2026-10-07. Status: accepted.

## Context

The Linux server (`docs/design/linux-server.md`) runs in the shared region of the Linux
processes it serves. One instance for all Linux processes would make a bug in it take down
every Linux program, as a bug in a Linux kernel does; an instance per process tree contains it.

## Decision

Each process tree the kernel starts — `/init` at boot, a program run from the kernel monitor —
gets its own instance of the Linux server: its own shared region with its own heap and state.
`fork`, `clone` and `exec` stay in the instance of their process.

## Consequences

- An instance is a container: its own VFS with its own tmpfs (the initramfs unpacked into it,
  `/tmp`), descriptors, pipes, pids, signals and `/proc` view. Processes of different trees
  cannot share pipes or tmpfs files, signal each other, or see each other's pids.
- Shared between instances, through the servers and the kernel: the disk (diskfs at `/data`),
  the network (netd), the console as a device (held by one instance at a time).
- Memory: the server's code and the initramfs image are read-only and mapped from the same
  pages in every instance; each instance pays for its own heap and tmpfs contents.
- A server bug ends the processes of one tree; the kernel and other trees keep running.

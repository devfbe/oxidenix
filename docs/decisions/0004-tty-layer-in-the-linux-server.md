# ADR 0004: The tty layer moves into the Linux server

Date: 2026-10-07. Status: accepted.

## Context

The kernel implements the tty line discipline, job control's foreground process group and
the console terminal (`drivers/tty.rs`). These are Linux semantics.

## Decision

The line discipline, termios, sessions and process groups on the terminal move into the
Linux server. The kernel keeps the framebuffer console and the keyboard as a device: output
of bytes (with the ANSI interpreter of the console driver) and input of key bytes, granted to
one Linux server instance at a time (ADR 0002).

## Consequences

The kernel monitor (fallback shell) uses the console device directly while no instance holds
it. Ctrl+C and Ctrl+Z become signals in the instance that holds the console.

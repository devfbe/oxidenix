# ADR 0003: Linux programs get a 46-bit address space

Date: 2026-10-07. Status: accepted.

## Context

Restricted mode needs the Linux server mapped above the program in the lower half of the
address space, invisible to the program (`docs/design/linux-server.md`). With 4-level paging the
lower half has 47 bits.

## Decision

The program gets PML4 slots 0–127 (`0` to `0x3fff_ffff_ffff`, 64 TiB), the server's shared
region slots 128–255.

## Consequences

`mmap` never returns addresses above 64 TiB, and fixed mappings above it fail with `ENOMEM`.
Programs that need more address space, or hard-code addresses above 64 TiB, do not run; none
in the userland targeted (Bash, BusyBox, Node.js/V8, whose pointer cages fit) does. 5-level
paging could restore the full range later without changing the design.

# ADR 0005: Shared-memory rings with granted buffers for the data plane

Date: 2026-10-08. Status: accepted.

## Context

Once the Linux server owns files and sockets, its traffic with diskfs and netd is the whole
I/O path. Today that traffic would be kernel IPC: messages copied through the kernel in 32 KiB
pieces, a synchronous round trip each, under one global lock (`docs/io-path-audit.md`,
findings 1-7). Alternatives considered:

- **Faster IPC** (no global lock, page remapping for large messages, as L4's string items):
  keeps one round trip and two address space switches per request and copies or remaps per
  message; batching would need a new message format anyway.
- **The kernel moves data between grants on request** (as Zircon's `zx_vmo_read` into a
  peer's memory): one copy per request through the kernel, a system call per request.
- **Shared-memory rings with granted buffers** (as virtio, io_uring, Fuchsia's block FIFO):
  requests and completions are descriptors in shared memory, data stays in the client's pages,
  which the service and its device reach directly.

## Decision

The data plane between the Linux server and the device servers uses single-producer
single-consumer rings in a shared memory object per channel, with buffers named by grants of
the client's memory objects (`docs/design/io-rings.md`). The control plane stays synchronous
IPC. Devices reach granted pages through device addresses the kernel hands out per grant, so
an IOMMU can confine them later without an interface change.

## Consequences

- Zero copies for file data with DMA (the page cache page is the DMA target); one for TCP.
- No system call per request under load; one wakeup per batch when idle.
- The services must treat ring memory as hostile: copy each descriptor out once and validate
  it; the kernel must revoke a grant only after the service acknowledged it (no DMA into a page
  after its revocation).
- Memory ordering becomes part of the ABI; the ring crate documents and tests it.
- Two protocols coexist while the kernel's `RemoteFs` still serves the kernel's own `/data`
  view (until R6c.3 ends).

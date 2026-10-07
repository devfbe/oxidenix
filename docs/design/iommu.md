# DMA isolation with an IOMMU

Status: planned.

## Problem

Servers drive bus-mastering devices from user space: netd the network card, diskfs the disk.
A device writes to and reads from whatever physical address the driver puts into its
descriptors. Without an IOMMU, a server that is compromised (diskfs parses the metadata of the
disk image, netd every packet from the network) can make its device read or write any physical
memory, the kernel included: such a server is as trusted as the kernel, which defeats the point
of running drivers in user space.

## Design

The machine gets an IOMMU (Intel VT-d, QEMU `q35` with `-device intel-iommu`). The kernel
programs its DMA remapping so that each device handed to a server can reach exactly that
server's DMA area and nothing else.

### VT-d (`kernel/src/drivers/iommu.rs`)

- The ACPI DMAR table names the remapping hardware (DRHD) and its register page.
- A root table (one entry per bus) points to context tables (one entry per device and
  function). Only devices handed to a server get a context entry; any other device's DMA is
  blocked and reported as a fault.
- Each such device gets its own domain with second-level page tables (4 levels, 48-bit
  addresses) that map the server's DMA area at I/O virtual address = physical address, so
  drivers keep giving the device physical addresses and nothing changes for them.
- Translation is enabled before the first server starts. The kernel invalidates the context
  cache and IOTLB globally after changing the tables (they only change at boot).
- Faults (a device reaching outside its area) raise the IOMMU's fault interrupt; the kernel
  logs the device, address and reason, and counts them.
- Fail closed: without an IOMMU, `dma_map` refuses and no bus-mastering device is handed to a
  server (the kernel says so at boot).

### Devices owned by servers

A server is given a PCI function rather than loose resources. It may read that function's
configuration space (`pci_config`, a new syscall; writing stays the kernel's job), map its
memory BARs uncached into its address space, take its interrupt line, and use its DMA area,
which the IOMMU confines the device to.

### Modern virtio

QEMU's virtio devices honour the IOMMU only with `iommu_platform=on`, which is a virtio 1.0
feature (`VIRTIO_F_ACCESS_PLATFORM`); the legacy interface bypasses it. `crates/virtio` moves to
the modern PCI transport: the capability list locates the common, notification, ISR and
device-specific configuration in memory BARs; features are 64-bit; queue addresses are set per
ring. Drivers require `VERSION_1` and `ACCESS_PLATFORM`. The devices run with
`disable-legacy=on,iommu_platform=on`. The transport uses no port I/O, so the kernel can use the
crate too (for the self-test below).

## Testing

- The whole suite runs on q35 with the IOMMU on.
- A boot-time self-test (in test mode) proves the isolation end to end: before diskfs gets the
  disk, the kernel sends the device one read into the DMA area (it must succeed) and one into
  memory outside it (it must fail with an IOMMU fault, and the memory must stay unchanged),
  then resets the device. A failure ends the test run.

## Steps

1. Switch the machine to q35 (AHCI boot disk, ICH9 interrupt routing); the suite passes.
2. Servers own PCI functions (`pci_config`, memory BAR mappings); `crates/virtio` on the modern
   transport; netd and diskfs on modern-only devices.
3. VT-d DMA remapping, `iommu_platform=on`, fail-closed `dma_map`, the self-test; the README's
   open issue about DMA goes away.

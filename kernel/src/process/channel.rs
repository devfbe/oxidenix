//! Channels: the kernel's part of the data plane between the Linux server
//! and the device servers (docs/design/io-rings.md, ADR 0005).
//!
//! A channel is one memory object holding a submission and a completion
//! ring and, if the client asks for one, a shared area for the protocol's
//! own state (layout: `ring::channel`). The client (a Linux server instance)
//! creates it, which maps it into the instance's region (`chan_create`),
//! and offers it to a service (`chan_connect`): the kernel sends the
//! service a control request (`ipc::call_control`, an `Offer`), the service
//! maps the channel into its own address space (`chan_attach`) and
//! answers. From then on both ends move descriptors through the rings
//! without the kernel; a futex on a ring word is the doorbell (both ends'
//! futexes are keyed by the object, see `futex`).
//!
//! **Grants.** The client grants page ranges of its memory objects to the
//! channel (`grant`). A grant pins its pages (`PageCache::pin`): they stay
//! the object's pages, present, never truncated away or reclaimed, and
//! their frames are referenced by the grant. The service maps a grant
//! (`grant_map`, read-only unless granted writable; `Backing::Granted`) and
//! asks for device addresses of it (`grant_dma`, through its server's
//! `DmaDomain`). `revoke` takes a grant back: its mappings in the service
//! are gone (with TLB shootdowns) when the call returns, and its device
//! mappings are removed from the domain. Without an IOMMU the kernel cannot
//! take a device address back, so a grant the service has device addresses
//! of stays pinned ("draining") until the service lets go of them
//! (`grant_dma_unmap`), or, if the service dies, until its device is reset
//! (its next registration): the pages are never freed while a device may
//! still reach them.
//!
//! **Teardown.** When the client's end goes (its handle closed, or the
//! instance ended) or the service's (it detached, or its process ended),
//! the kernel takes back every grant as above, sets the end's bit in the
//! channel's `state` and hangs the memory up (`PageCache::hang_up`): every
//! futex wait on it fails from then on and every sleeper wakes, so no end
//! sleeps forever on a dead peer. Ends do not lose their mapping of the
//! ring memory itself (it is plain memory, theirs to unmap: the client by
//! closing its handle, the service by `chan_detach`).
//!
//! A peer is hostile: the kernel never reads the rings; it validates every
//! grant id, offset and length a service passes; a grant id is reused only
//! once the old grant is fully gone. Teardowns triggered where the kernel
//! may not sleep (a process's end, the last reference of an instance) are
//! done by the `channels` kernel thread (`worker`).

use super::address_space::{Backing, Hold, Mm, Prot, PAGE};
use super::errno::*;
use super::{ipc, Pid, Server};
use crate::fs::cache::PageCache;
use crate::memory;
use crate::sync::{IrqSpinLock, Mutex};
use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use ring::channel::{Header, Layout, Offer, CLIENT_GONE, SERVICE_GONE, STATE_OFFSET};
use x86_64::structures::paging::PhysFrame;

/// Most grants a channel holds at once, pages per grant, and pages granted
/// in all (each pinned page costs the kernel a few bytes).
const MAX_GRANTS: usize = 4096;
const MAX_GRANT_PAGES: u64 = 1 << 14;
const MAX_GRANTED_PAGES: u64 = 1 << 16;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Every channel, by id (ascending: ids only grow); the attached ones also
/// with their service. Grown fallibly: nothing here allocates under the
/// lock where failing would panic.
///
/// Invariant: `Entry::service` is `Some` exactly while the channel's
/// `Inner::service` is, and is the same service (`attach` makes both,
/// `service_gone` ends both). Both change only with this lock held and,
/// inside it, the channel's `inner` lock (always in that order); whoever
/// reads both (`attached`) holds both, so it never sees one without the
/// other. An entry goes only with its channel (`Channel::drop`), which
/// cannot happen while it is attached (the entry's `Registered` holds it).
///
/// An attached channel lives until its service detaches, dies or executes
/// a new program, even when its client is gone: it is the service's to let
/// go (it sees `CLIENT_GONE`); what it then holds is the ring memory and
/// the grants still draining, bounded by its server's lifetime.
static CHANNELS: IrqSpinLock<Vec<Entry>> = IrqSpinLock::new(Vec::new());

struct Entry {
    id: u64,
    channel: Weak<Channel>,
    service: Option<Registered>,
}

/// An attached channel's service: an attached channel lives as long as
/// its service end.
struct Registered {
    pid: Pid,
    channel: Arc<Channel>,
    /// The teardown to queue when the process ends or executes a new
    /// program (made at attach: queuing it then allocates nothing).
    exit: Option<Box<WorkNode>>,
}

fn entry(channels: &mut [Entry], id: u64) -> Option<&mut Entry> {
    channels.binary_search_by_key(&id, |e| e.id).ok().map(|i| &mut channels[i])
}

pub struct Channel {
    id: u64,
    layout: Layout,
    memory: Arc<PageCache>,
    /// The header page, with a reference of ours (for `state`).
    header: PhysFrame,
    /// The submission ring's first page, with a reference of ours (for its
    /// `tail`, the word a service's doorbell watch compares: `watch`).
    doorbell: PhysFrame,
    inner: IrqSpinLock<Inner>,
}

struct Inner {
    /// The service registration the channel was offered to, while the
    /// offer stands.
    offered: Option<ipc::Instance>,
    service: Option<ServiceEnd>,
    /// The request carrying the offer, while `connect` waits for it.
    offer_request: Option<u64>,
    client_gone: bool,
    service_gone: bool,
    /// Grants by id: live ones and revoked ones still draining.
    grants: Grants,
    /// Grants being made (`grant` reserves a slot before it pins).
    reserved: usize,
    /// Pages of `grants` and of the grants being made, each released only
    /// by whoever took it.
    pages: u64,
}

/// Grants by id (from 1), in a table of slots: an id is the lowest free
/// one, and comes back only once its grant is fully gone.
#[derive(Default)]
struct Grants {
    slots: Vec<Option<Arc<Grant>>>,
    live: usize,
}

impl Grants {
    fn get(&self, id: u32) -> Option<&Arc<Grant>> {
        self.slots.get((id as usize).checked_sub(1)?)?.as_ref()
    }

    /// Enters `grant` under the lowest free id (ENOMEM without room).
    fn insert(&mut self, grant: Arc<Grant>) -> Result<u32, i64> {
        let index = match self.slots.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.slots.try_reserve(1).map_err(|_| ENOMEM)?;
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.slots[index] = Some(grant);
        self.live += 1;
        Ok(index as u32 + 1)
    }

    /// Takes grant `id` out if it is `grant`.
    fn remove(&mut self, id: u32, grant: &Arc<Grant>) -> Option<Arc<Grant>> {
        let slot = self.slots.get_mut((id as usize).checked_sub(1)?)?;
        if !slot.as_ref().is_some_and(|g| Arc::ptr_eq(g, grant)) {
            return None;
        }
        self.live -= 1;
        slot.take()
    }

    fn iter(&self) -> impl Iterator<Item = (u32, &Arc<Grant>)> {
        self.slots.iter().enumerate().filter_map(|(i, g)| g.as_ref().map(|g| (i as u32 + 1, g)))
    }
}

#[derive(Clone)]
struct ServiceEnd {
    mm: Weak<Mm>,
    server: Arc<Server>,
}

/// Pages of a memory object granted to a channel's service, pinned while
/// the grant lives.
pub struct Grant {
    object: Arc<PageCache>,
    /// Page index of the first page in `object`.
    first: u64,
    /// The pinned frames (one reference each, the grant's).
    frames: Vec<PhysFrame>,
    writable: bool,
    /// Held (sleeping) across the changes of the service's mappings, so a
    /// mapping or a device address can never be made after the revoke.
    state: Mutex<GrantState>,
    /// The next grant in its domain's quarantine.
    quarantined: spin::Mutex<Option<Arc<Grant>>>,
}

#[derive(Default)]
struct GrantState {
    revoked: bool,
    /// The service has device addresses of it (`grant_dma`).
    device: bool,
}

impl Drop for Grant {
    fn drop(&mut self) {
        for (i, &frame) in self.frames.iter().enumerate() {
            self.object.unpin(self.first + i as u64, frame);
        }
    }
}

impl Grant {
    fn hold(self: &Arc<Self>) -> Hold {
        self.clone()
    }

    fn bytes(&self) -> u64 {
        self.frames.len() as u64 * PAGE
    }
}

/// Where a server's devices reach granted pages: the one place that maps
/// and unmaps pages for device access. Without an IOMMU a device address
/// is the physical address and nothing can be taken back; with one
/// (docs/design/iommu.md) `map` enters the page into the device's domain
/// and `unmap` removes it (and flushes the IOTLB), confining the device to
/// the grants. It belongs to the `Server` and outlives its processes, like
/// the DMA area: grants a dead server's device might still reach wait here
/// until the device is reset.
#[derive(Default)]
pub struct DmaDomain {
    /// A list through `Grant::quarantined`: entering one never allocates.
    quarantine: spin::Mutex<Option<Arc<Grant>>>,
}

impl DmaDomain {
    /// The device address of `frame`, which the device may write if
    /// `writable` (with an IOMMU: the domain's entry is read-only
    /// otherwise; without one the device can do anything).
    fn map(&self, frame: PhysFrame, _writable: bool) -> u64 {
        frame.start_address().as_u64()
    }

    /// Takes the device addresses of `grant` back; whether its pages are
    /// out of every device's reach now (never without an IOMMU).
    fn unmap(&self, _grant: &Grant) -> bool {
        false
    }

    /// Keeps `grant` (and its pins) until the device is reset.
    fn quarantine(&self, grant: Arc<Grant>) {
        let mut head = self.quarantine.lock();
        *grant.quarantined.lock() = head.take();
        *head = Some(grant);
    }

    /// The server's device was reset (a new process of the server
    /// registered): nothing quarantined is reachable any more.
    pub fn device_reset(&self) {
        let mut next = self.quarantine.lock().take();
        // One at a time (a recursive drop of a long list could overflow
        // the stack); each takes its pins along.
        while let Some(grant) = next {
            next = grant.quarantined.lock().take();
        }
    }
}

impl Channel {
    /// A new channel of `slots` slots per ring and `shared` pages of
    /// shared area (see `ring::channel`).
    pub fn new(slots: u64, shared: u64) -> Result<Arc<Channel>, i64> {
        let (slots, shared) = (u32::try_from(slots).map_err(|_| EINVAL)?, u32::try_from(shared).map_err(|_| EINVAL)?);
        let layout = Layout::with_shared(slots, shared).ok_or(EINVAL)?;
        let memory = PageCache::anonymous(layout.pages as u64).map_err(|_| ENOMEM)?;
        let header = memory.map_page(0).map_err(|_| ENOMEM)?;
        let doorbell = match memory.map_page((layout.submission / PAGE as usize) as u64) {
            Ok(frame) => frame,
            Err(_) => {
                memory::with_frames(|f| unsafe { x86_64::structures::paging::FrameDeallocator::deallocate_frame(f, header) });
                return Err(ENOMEM);
            }
        };
        // Positions start at 0 (the memory is zeroed).
        unsafe { (memory::phys_to_virt(header.start_address().as_u64()) as *mut Header).write(Header::new(&layout)) };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let inner = Inner { offered: None, service: None, offer_request: None, client_gone: false, service_gone: false, grants: Grants::default(), reserved: 0, pages: 0 };
        // From here on, dropping the channel releases both pages.
        let channel = Channel { id, layout, memory, header, doorbell, inner: IrqSpinLock::new(inner) };
        let channel = Arc::try_new(channel).map_err(|_| ENOMEM)?;
        let mut channels = CHANNELS.lock();
        channels.try_reserve(1).map_err(|_| ENOMEM)?;
        channels.push(Entry { id, channel: Arc::downgrade(&channel), service: None });
        drop(channels);
        Ok(channel)
    }

    pub fn memory(&self) -> &Arc<PageCache> {
        &self.memory
    }

    pub fn pages(&self) -> u64 {
        self.layout.pages as u64
    }

    /// The submission ring's `tail` (the client rings the service's
    /// doorbell with a futex wake on it).
    fn submission_tail(&self) -> &AtomicU32 {
        let at = self.doorbell.start_address().as_u64() + (self.layout.submission % PAGE as usize + ring::TAIL_OFFSET) as u64;
        unsafe { &*(memory::phys_to_virt(at) as *const AtomicU32) }
    }

    fn state(&self) -> &AtomicU32 {
        unsafe { &*(memory::phys_to_virt(self.header.start_address().as_u64() + STATE_OFFSET as u64) as *const AtomicU32) }
    }

    /// Marks an end gone and wakes every sleeper on the channel for good.
    fn set_gone(&self, bit: u32) {
        self.state().fetch_or(bit, Ordering::SeqCst);
        self.memory.hang_up();
        super::futex::wake_object(&self.memory);
    }

    // ------------------------------------------------------------ client

    /// Offers the channel to the service registered as `name` and waits
    /// until it attached it (see `restricted::SYS_CHAN_CONNECT`).
    pub fn connect(&self, name: &str, client: Pid) -> Result<(), i64> {
        {
            let inner = self.inner.lock();
            if inner.offered.is_some() || inner.service.is_some() || inner.service_gone {
                return Err(EISCONN);
            }
        }
        let to = match ipc::instance(name) {
            Some(to) => to,
            None => {
                // A server of the kernel's that died is started again.
                let server = super::server_named(name).ok_or(ENOENT)?;
                server.revive().map_err(|_| EIO)?;
                ipc::instance(name).ok_or(EIO)?
            }
        };
        {
            let mut inner = self.inner.lock();
            if inner.offered.is_some() || inner.service.is_some() || inner.service_gone {
                return Err(EISCONN);
            }
            inner.offered = Some(to);
        }
        let offer = Offer { channel: self.id, slots: self.layout.slots, shared: self.layout.shared_pages as u32, client }.encode();
        let mut message = Vec::new();
        let sent = message.try_reserve_exact(offer.len()).map_err(|_| ENOMEM).and_then(|_| {
            message.extend_from_slice(&offer);
            ipc::send_control(to, message)
        });
        let id = match sent {
            Ok(id) => id,
            Err(e) => {
                self.inner.lock().offered = None;
                return Err(e);
            }
        };
        self.inner.lock().offer_request = Some(id);
        // Done once the service attached (it need not have answered),
        // answered, or died; a signal (always a fatal one) gives the offer
        // up unless the service attached.
        let answer = loop {
            let wait = super::sched::prepare_to_wait(ipc::reply_chan(id));
            if self.inner.lock().service.is_some() {
                break None;
            }
            if let Some(answer) = ipc::take_reply(id) {
                break Some(answer);
            }
            if super::signal::interrupted() || super::signal::dying() {
                break Some(Err(EINTR));
            }
            wait.sleep();
        };
        ipc::abandon(id);
        let refused = match answer {
            None => 0,
            Some(Ok(reply)) => match <[u8; 8]>::try_from(reply.as_slice()).map(i64::from_le_bytes) {
                Ok(status) if status < 0 && status > -4096 => -status,
                _ => ECONNREFUSED,
            },
            Some(Err(e)) => e,
        };
        // Attached or not is the kernel's to say, not the answer's: from
        // here on, the offer is decided either way.
        let mut inner = self.inner.lock();
        inner.offer_request = None;
        if inner.service.is_some() {
            return Ok(());
        }
        inner.offered = None;
        Err(refused)
    }

    /// Grants `pages` pages of `object` from byte `offset` to the service
    /// (see `restricted::SYS_GRANT`); returns the grant's id.
    pub fn grant(&self, object: &Arc<PageCache>, offset: u64, pages: u64, writable: bool) -> Result<u32, i64> {
        if offset % PAGE != 0 || pages == 0 || pages > MAX_GRANT_PAGES {
            return Err(EINVAL);
        }
        let first = offset / PAGE;
        first.checked_add(pages).ok_or(EINVAL)?;
        // A slot and the pages first, released below on failure (and only
        // by this call: a teardown meanwhile leaves them alone).
        self.reserve(pages)?;
        let mut frames = Vec::new();
        if frames.try_reserve_exact(pages as usize).is_err() {
            self.unreserve(pages);
            return Err(ENOMEM);
        }
        for i in 0..pages {
            match object.pin(first + i) {
                Ok(frame) => frames.push(frame),
                Err(e) => {
                    for (j, frame) in frames.into_iter().enumerate() {
                        object.unpin(first + j as u64, frame);
                    }
                    self.unreserve(pages);
                    return Err(e);
                }
            }
        }
        self.enter(object, first, frames, writable, pages)
    }

    /// Grants a run of a cached object's pages to be filled (`fill`:
    /// missing ones, made pending, `PageCache::pin_fill`) or written back
    /// (dirty ones, `pin_dirty`) among the `pages` from byte `offset` (see
    /// `restricted::GRANT_FILL`): the grant's id, the run's first page, its
    /// length and the file's size then.
    pub fn grant_run(&self, object: &Arc<PageCache>, offset: u64, pages: u64, fill: bool, writable: bool) -> Result<(u32, u64, u64, u64), crate::fs::cache::Scan> {
        use crate::fs::cache::Scan;
        if offset % PAGE != 0 || pages == 0 {
            return Err(Scan::Errno(EINVAL));
        }
        let window = pages;
        let pages = pages.min(crate::fs::cache::MAX_RUN);
        self.reserve(pages).map_err(Scan::Errno)?;
        let pinned = if fill { object.pin_fill(offset / PAGE, window) } else { object.pin_dirty(offset / PAGE, window) };
        let (first, frames, size) = match pinned {
            Ok(run) => run,
            Err(e) => {
                self.unreserve(pages);
                return Err(e);
            }
        };
        let count = frames.len() as u64;
        // Only what the run took stays reserved.
        self.inner.lock().pages -= pages - count;
        match self.enter(object, first, frames, writable, count) {
            Ok(id) => Ok((id, first, count, size)),
            Err(e) => {
                // Nobody will fill or write them: pending pages go again,
                // dirty ones are dirty again.
                let _ = if fill { object.filled(first, count, false) } else { object.redirty(first, count) };
                Err(Scan::Errno(e))
            }
        }
    }

    /// Reserves a grant's slot and `pages` pages (EPIPE once an end is
    /// gone, ENOTCONN before the service attached, ENOSPC beyond the
    /// limits); `unreserve` and `enter` give them back.
    fn reserve(&self, pages: u64) -> Result<(), i64> {
        let mut inner = self.inner.lock();
        if inner.service_gone || inner.client_gone {
            return Err(EPIPE);
        }
        if inner.service.is_none() {
            return Err(ENOTCONN);
        }
        if inner.grants.live + inner.reserved >= MAX_GRANTS || inner.pages + pages > MAX_GRANTED_PAGES {
            return Err(ENOSPC);
        }
        inner.reserved += 1;
        inner.pages += pages;
        Ok(())
    }

    fn unreserve(&self, pages: u64) {
        let mut inner = self.inner.lock();
        inner.reserved -= 1;
        inner.pages -= pages;
    }

    /// Makes the pinned `frames` (pages from `first` of `object`) a grant
    /// under the slot and `pages` pages reserved for it; on failure they
    /// are unpinned and the reservation goes.
    fn enter(&self, object: &Arc<PageCache>, first: u64, frames: Vec<PhysFrame>, writable: bool, pages: u64) -> Result<u32, i64> {
        let grant = Grant {
            object: object.clone(),
            first,
            frames,
            writable,
            state: Mutex::new(GrantState::default()),
            quarantined: spin::Mutex::new(None),
        };
        // (Dropping the grant unpins its frames.)
        let Ok(grant) = Arc::try_new(grant) else {
            let mut inner = self.inner.lock();
            inner.reserved -= 1;
            inner.pages -= pages;
            return Err(ENOMEM);
        };
        let mut inner = self.inner.lock();
        inner.reserved -= 1;
        let entered = if inner.service_gone || inner.client_gone { Err(EPIPE) } else { inner.grants.insert(grant) };
        if entered.is_err() {
            inner.pages -= pages;
        }
        entered
    }

    /// Takes grant `id` back (see `restricted::SYS_REVOKE`): 0, or
    /// `REVOKE_DRAINING` if a device may still reach its pages.
    pub fn revoke(&self, id: u32) -> Result<i64, i64> {
        let (grant, service) = {
            let inner = self.inner.lock();
            if inner.service_gone {
                return Ok(0);
            }
            (inner.grants.get(id).cloned().ok_or(EINVAL)?, inner.service.clone())
        };
        let mut st = grant.state.lock();
        if st.revoked {
            // Revoked before, or the service went meanwhile.
            return if self.inner.lock().service_gone { Ok(0) } else { Err(EINVAL) };
        }
        let draining = self.retire(&grant, &mut st, service.as_ref());
        drop(st);
        if !draining {
            self.forget(id, &grant);
        }
        Ok(draining as i64)
    }

    /// Revokes `grant` (its state locked in `st`): the service's mappings
    /// go, then its device mappings. Whether a device may still reach it.
    fn retire(&self, grant: &Arc<Grant>, st: &mut GrantState, service: Option<&ServiceEnd>) -> bool {
        st.revoked = true;
        let Some(service) = service else { return false };
        if let Some(mm) = service.mm.upgrade() {
            // The range stays reserved: the service may still copy to the
            // address it knew, which must fault, not reach a new mapping.
            mm.lock().unmap_grant(&grant.hold(), self.id);
        }
        st.device && !service.server.domain.unmap(grant)
    }

    /// Drops `grant` (id `id`) from the channel; its pins go with the last
    /// reference.
    fn forget(&self, id: u32, grant: &Arc<Grant>) {
        let gone = {
            let mut inner = self.inner.lock();
            let gone = inner.grants.remove(id, grant);
            if gone.is_some() {
                inner.pages -= grant.frames.len() as u64;
            }
            gone
        };
        drop(gone);
    }

    /// The client's end is gone: every grant is revoked (draining ones
    /// stay until the service lets go), the service sees `CLIENT_GONE`.
    fn client_gone(&self) {
        let (grants, service) = {
            let mut inner = self.inner.lock();
            if inner.client_gone {
                return;
            }
            inner.client_gone = true;
            inner.offered = None;
            (inner.grants.iter().map(|(id, g)| (id, g.clone())).collect::<Vec<_>>(), inner.service.clone())
        };
        for (id, grant) in grants {
            let mut st = grant.state.lock();
            let draining = !st.revoked && self.retire(&grant, &mut st, service.as_ref());
            let keep = draining || (st.revoked && st.device);
            drop(st);
            if !keep {
                self.forget(id, &grant);
            }
        }
        self.set_gone(CLIENT_GONE);
    }

    // ----------------------------------------------------------- service

    /// The channel `id`, attached to the calling process.
    /// The service is its address space, not its process id: after an
    /// exec (which ends the service's end, see `service_exited`) the same
    /// process is no longer the service.
    fn attached(id: u64) -> Result<(Arc<Channel>, ServiceEnd), i64> {
        let mm = super::current_mm().ok_or(ENOENT)?;
        let mut channels = CHANNELS.lock();
        let channel = entry(&mut channels, id).and_then(|e| e.service.as_ref().map(|r| r.channel.clone())).ok_or(ENOENT)?;
        let service = channel.inner.lock().service.clone().filter(|s| core::ptr::eq(s.mm.as_ptr(), Arc::as_ptr(&mm)));
        drop(channels);
        Ok((channel, service.ok_or(ENOENT)?))
    }

    /// The service's end is gone (`died`: its process ended): every grant
    /// is revoked, those a dead service's device may still reach go to its
    /// server's quarantine; the client sees `SERVICE_GONE`.
    fn service_gone(&self, died: bool) {
        // The service ends in the registry and in the channel at once (see
        // `CHANNELS`); the registry's reference is dropped at the end (the
        // caller holds another one).
        let (grants, service, registered) = {
            let mut channels = CHANNELS.lock();
            let mut inner = self.inner.lock();
            if inner.service_gone {
                return;
            }
            inner.service_gone = true;
            inner.offered = None;
            let grants = core::mem::take(&mut inner.grants);
            // Exactly their pages: grants being made release their own.
            inner.pages -= grants.iter().map(|(_, g)| g.frames.len() as u64).sum::<u64>();
            let registered = entry(&mut channels, self.id).and_then(|e| e.service.take());
            (grants, inner.service.take(), registered)
        };
        let mm = service.as_ref().and_then(|s| s.mm.upgrade());
        for grant in grants.slots.into_iter().flatten() {
            let mut st = grant.state.lock();
            st.revoked = true;
            if let Some(mm) = &mm {
                mm.lock().unmap_grant(&grant.hold(), self.id);
            }
            let device = st.device;
            drop(st);
            // A detaching service vouches that its devices are done.
            if let (true, true, Some(s)) = (died, device, &service) {
                if !s.server.domain.unmap(&grant) {
                    s.server.domain.quarantine(grant);
                }
            }
            // (Otherwise the pins go with the grant's last reference.)
        }
        if let Some(mm) = &mm {
            let mut space = mm.lock();
            space.unmap_object(&self.memory);
            space.unmap_revoked(self.id);
        }
        self.set_gone(SERVICE_GONE);
        drop(registered);
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        memory::with_frames(|f| unsafe {
            x86_64::structures::paging::FrameDeallocator::deallocate_frame(f, self.header);
            x86_64::structures::paging::FrameDeallocator::deallocate_frame(f, self.doorbell);
        });
        let mut channels = CHANNELS.lock();
        if let Ok(i) = channels.binary_search_by_key(&self.id, |e| e.id) {
            if channels[i].channel.strong_count() == 0 {
                channels.remove(i);
            }
        }
    }
}

/// The client's end of a channel, as a handle of the Linux server holds it
/// (`linux::Object::Channel`): when the last reference goes, so does the
/// end, and the region mapping at `addr`.
pub struct ClientEnd {
    pub channel: Arc<Channel>,
    /// Its teardown (`Work::ClientGone`), made with the end: the drop may
    /// run where the kernel can neither sleep nor fail an allocation (the
    /// instance's end), so it only queues this. None once torn down.
    teardown: Option<Box<WorkNode>>,
}

impl ClientEnd {
    /// The end of `channel` mapped at `addr` of `instance`'s region; if it
    /// cannot be made, the channel's client end goes now.
    pub fn new(channel: Arc<Channel>, instance: Weak<super::linux::Instance>, addr: u64) -> Result<ClientEnd, i64> {
        let work = Work::ClientGone { channel: channel.clone(), instance, addr };
        match Box::<WorkNode>::try_new_uninit() {
            Ok(node) => Ok(ClientEnd { channel, teardown: Some(Box::write(node, WorkNode { work, next: None })) }),
            Err(_) => {
                work.run();
                Err(ENOMEM)
            }
        }
    }

    /// Tears the end down now (from a call that may sleep), so that when
    /// `handle_close` returns, the service's mappings of the grants are gone
    /// and the pins released.
    pub fn close(mut self) {
        if let Some(node) = self.teardown.take() {
            node.work.run();
        }
    }
}

impl Drop for ClientEnd {
    fn drop(&mut self) {
        if let Some(node) = self.teardown.take() {
            defer(node);
        }
    }
}

fn client_gone(channel: &Channel, instance: &Weak<super::linux::Instance>, addr: u64) {
    if let Some(instance) = instance.upgrade() {
        instance.unmap_object(addr);
        instance.channel_gone();
    }
    channel.client_gone();
}

// ------------------------------------------------------- the service's calls

/// chan_attach(channel) -> addr: maps a channel offered to the calling
/// process's service into its address space: the header page read-only
/// (only the kernel writes `state`), the rings and the shared area read
/// and write.
pub fn attach(id: u64) -> Result<i64, i64> {
    let me = super::current_pid();
    let server = super::with_current(|p| p.server.clone()).ok_or(EPERM)?;
    let channel = entry(&mut CHANNELS.lock(), id).and_then(|e| e.channel.upgrade()).ok_or(ENOENT)?;
    let to = {
        let inner = channel.inner.lock();
        if inner.client_gone {
            return Err(EPIPE);
        }
        if inner.service.is_some() {
            return Err(EISCONN);
        }
        inner.offered.ok_or(ENOENT)?
    };
    // Only the service it was offered to.
    if ipc::server_of(to) != Some(me) {
        return Err(ENOENT);
    }
    let mm = super::current_mm().ok_or(EINVAL)?;
    let exit = Box::try_new(WorkNode { work: Work::ServiceGone(Arc::downgrade(&channel)), next: None }).map_err(|_| ENOMEM)?;
    let len = channel.layout.bytes() as u64;
    let addr = {
        let mut space = mm.lock();
        let floor = space.brk_end;
        let start = space.find_free(len, floor).ok_or(ENOMEM)?;
        // The header (the kernel's `state`) read-only, the rings writable.
        let header = Backing::File { cache: channel.memory.clone(), offset: 0, shared: true, may_write: false, _hold: None };
        let rings = Backing::File { cache: channel.memory.clone(), offset: PAGE, shared: true, may_write: true, _hold: None };
        let read = Prot { read: true, write: false, exec: false };
        let mapped = space
            .map(start, PAGE, read, header, false)
            .and_then(|_| space.map(start + PAGE, len - PAGE, Prot::RW, rings, false))
            .and_then(|_| space.populate(start, PAGE, false))
            .and_then(|_| space.populate(start + PAGE, len - PAGE, true));
        if mapped.is_err() {
            space.unmap(start, len);
            return Err(ENOMEM);
        }
        start
    };
    let attached = {
        let mut channels = CHANNELS.lock();
        let mut inner = channel.inner.lock();
        if inner.client_gone || inner.service.is_some() || inner.offered != Some(to) {
            Err(if inner.client_gone { EPIPE } else { ECONNREFUSED })
        } else {
            inner.service = Some(ServiceEnd { mm: Arc::downgrade(&mm), server });
            if let Some(entry) = entry(&mut channels, id) {
                entry.service = Some(Registered { pid: me, channel: channel.clone(), exit: Some(exit) });
            }
            Ok(inner.offer_request)
        }
    };
    match attached {
        // The connect waiting for it is complete.
        Ok(Some(request)) => ipc::wake(request),
        Ok(None) => {}
        Err(e) => {
            mm.lock().unmap(addr, len);
            return Err(e);
        }
    }
    Ok(addr as i64)
}

/// chan_detach(channel): the service lets go of the channel: its mappings
/// of the channel and of every grant go, and the client sees
/// `SERVICE_GONE`. The service vouches that its devices no longer use the
/// grants.
pub fn detach(id: u64) -> Result<i64, i64> {
    let (channel, _) = Channel::attached(id)?;
    channel.service_gone(false);
    Ok(0)
}

/// The grant `grant` of the attached channel `id`, and the service.
fn grant_of(id: u64, grant: u64) -> Result<(Arc<Channel>, ServiceEnd, Arc<Grant>), i64> {
    let (channel, service) = Channel::attached(id)?;
    let g = u32::try_from(grant).ok().and_then(|g| channel.inner.lock().grants.get(g).cloned()).ok_or(ENOENT)?;
    Ok((channel, service, g))
}

/// grant_map(channel, grant, info) -> addr: maps a grant into the calling
/// service, read-only unless granted writable; stores (pages, writable) as
/// two u64s at `info`.
pub fn grant_map(id: u64, grant: u64, info: u64) -> Result<i64, i64> {
    let (_, service, g) = grant_of(id, grant)?;
    super::uaccess::write(info, [g.frames.len() as u64, g.writable as u64])?;
    let st = g.state.lock();
    if st.revoked {
        return Err(ENOENT);
    }
    // The service's address space, which revoke and teardown reach.
    let mm = service.mm.upgrade().ok_or(ENOENT)?;
    let mut space = mm.lock();
    let floor = space.brk_end;
    let start = space.find_free(g.bytes(), floor).ok_or(ENOMEM)?;
    space.map_granted(start, &g.frames, g.writable, g.hold()).map_err(|_| ENOMEM)?;
    drop(space);
    drop(st);
    Ok(start as i64)
}

/// grant_dma(channel, grant, offset) -> the device address of the byte at
/// `offset` of the grant (valid to the end of its page).
pub fn grant_dma(id: u64, grant: u64, offset: u64) -> Result<i64, i64> {
    let (_, service, g) = grant_of(id, grant)?;
    if offset >= g.bytes() {
        return Err(EINVAL);
    }
    let mut st = g.state.lock();
    if st.revoked {
        return Err(ENOENT);
    }
    st.device = true;
    let address = service.server.domain.map(g.frames[(offset / PAGE) as usize], g.writable) + offset % PAGE;
    Ok(address as i64)
}

/// Most device addresses one `grant_dma_pages` call returns.
const MAX_DMA_PAGES: u64 = 512;

/// grant_dma_pages(channel, grant, first, count, out): the device
/// addresses of `count` pages of the grant from page `first`, stored as
/// u64s at `out` (each valid for its whole page): `grant_dma` for a range,
/// in one call.
pub fn grant_dma_pages(id: u64, grant: u64, first: u64, count: u64, out: u64) -> Result<i64, i64> {
    let (_, service, g) = grant_of(id, grant)?;
    if count == 0 || count > MAX_DMA_PAGES || first.checked_add(count).is_none_or(|end| end > g.frames.len() as u64) {
        return Err(EINVAL);
    }
    // The addresses as the caller's u64s, in one buffer allocated fallibly.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.try_reserve_exact(count as usize * 8).map_err(|_| ENOMEM)?;
    {
        let mut st = g.state.lock();
        if st.revoked {
            return Err(ENOENT);
        }
        st.device = true;
        for &frame in &g.frames[first as usize..(first + count) as usize] {
            bytes.extend_from_slice(&service.server.domain.map(frame, g.writable).to_le_bytes());
        }
    }
    super::uaccess::copy_to(out, &bytes)?;
    Ok(0)
}

/// chan_watch(channel, value): arms the calling service's doorbell watch
/// on the channel's submission ring: if its `tail` still holds `value`,
/// the client's next doorbell (a futex wake on it), or the client's end
/// going, makes the service's `ipc_receive` return `ipc::DOORBELL`. For a
/// service whose event loop sleeps in `ipc_receive` (it announced the
/// sleep in the ring first, `ring::Consumer::prepare_sleep`). EAGAIN if
/// the tail moved, EPIPE once the client is gone. One-shot; arming it
/// again before it rang changes nothing.
pub fn watch(id: u64, value: u64) -> Result<i64, i64> {
    let (channel, service) = Channel::attached(id)?;
    let value = u32::try_from(value).map_err(|_| EINVAL)?;
    let offset = (channel.layout.submission + ring::TAIL_OFFSET) as u64;
    super::futex::object_watch(&channel.memory, offset, channel.submission_tail(), value, &service.server.doorbell, super::current_pid())
}

/// grant_dma_unmap(channel, grant): the service's devices are done with
/// the grant: its device addresses become invalid, and a revoked grant
/// that waited for this goes.
pub fn grant_dma_unmap(id: u64, grant: u64) -> Result<i64, i64> {
    let (channel, service, g) = grant_of(id, grant)?;
    let mut st = g.state.lock();
    st.device = false;
    service.server.domain.unmap(&g);
    let revoked = st.revoked;
    drop(st);
    if revoked {
        channel.forget(grant as u32, &g);
    }
    Ok(0)
}

/// set_copy_fixup(insn, fixup): the calling server's copy routine on
/// granted memory. A revoke takes a grant from the service at once (see
/// docs/design/io-rings.md, "The service's contract for grant memory"), so
/// a CPU copy into or out of it may fault; a fault of the instruction at
/// `insn` that cannot be resolved resumes at `fixup` (the routine reports
/// the failure) instead of killing the service. Once per program (EBUSY
/// after); servers only. The fixup hides every unresolved fault of that
/// instruction, whatever the address: a service's own bug there (a wild
/// pointer into its copy routine) becomes a failed copy, not a crash, so
/// the routine must be used only for copies whose failure it reports. A
/// revoked grant's range stays reserved and inaccessible until the service
/// unmaps it (`Backing::Revoked`), so the fault is certain: the address
/// never reaches another mapping meanwhile. The registration is the
/// task's: a thread the server creates has none (servers are single-
/// threaded; one that is not registers again from each thread).
pub fn set_copy_fixup(insn: u64, fixup: u64) -> Result<i64, i64> {
    use super::address_space::USER_END;
    if insn >= USER_END || fixup >= USER_END {
        return Err(EINVAL);
    }
    super::with_current(|p| {
        if p.server.is_none() {
            return Err(EPERM);
        }
        if p.copy_fixup.is_some() {
            return Err(EBUSY);
        }
        p.copy_fixup = Some((insn, fixup));
        Ok(0)
    })
}

/// Where a fault of the current task's instruction at `rip` resumes, if it
/// is the copy routine `set_copy_fixup` registered.
pub fn copy_fixup(rip: u64) -> Option<u64> {
    super::with_current(|p| p.copy_fixup.filter(|&(insn, _)| insn == rip).map(|(_, fixup)| fixup))
}

/// The process `pid` ended, or executed a new program (whose devices are
/// the old program's: their grants are treated as after a death): the
/// channels it served lose their service. (Called where the kernel may
/// not sleep: the teardown is deferred.)
pub fn service_exited(pid: Pid) {
    let mut channels = CHANNELS.lock();
    for e in channels.iter_mut() {
        if let Some(node) = e.service.as_mut().filter(|r| r.pid == pid).and_then(|r| r.exit.take()) {
            defer(node);
        }
    }
}

// ------------------------------------------------------- deferred teardown

enum Work {
    ClientGone { channel: Arc<Channel>, instance: Weak<super::linux::Instance>, addr: u64 },
    /// (The registry keeps the channel until this ran.)
    ServiceGone(Weak<Channel>),
}

impl Work {
    fn run(&self) {
        match self {
            Work::ClientGone { channel, instance, addr } => client_gone(channel, instance, *addr),
            Work::ServiceGone(channel) => {
                if let Some(channel) = channel.upgrade() {
                    channel.service_gone(true);
                }
            }
        }
    }
}

/// A teardown, allocated in advance by whoever may need it queued.
struct WorkNode {
    work: Work,
    next: Option<Box<WorkNode>>,
}

/// The queued teardowns (a list of their nodes: queuing never allocates).
static WORK: IrqSpinLock<Option<Box<WorkNode>>> = IrqSpinLock::new(None);

fn work_chan() -> usize {
    &WORK as *const _ as usize
}

fn defer(mut node: Box<WorkNode>) {
    {
        let mut head = WORK.lock();
        node.next = head.take();
        *head = Some(node);
    }
    super::wakeup(work_chan());
}

/// Does the deferred teardowns now (from a context that may sleep: the
/// worker, or a call that just dropped a channel and wants it done).
pub fn run_deferred() {
    loop {
        let node = {
            let mut head = WORK.lock();
            let Some(mut node) = head.take() else { return };
            *head = node.next.take();
            node
        };
        node.work.run();
    }
}

/// Waits until no channel is left (every client's end went and each
/// service let go of its channels: diskfs released what the clients held),
/// at most until `deadline`: for a shutdown. Whether none is.
pub fn settle(deadline: u64) -> bool {
    loop {
        run_deferred();
        if CHANNELS.lock().iter().all(|e| e.channel.strong_count() == 0) {
            return true;
        }
        if crate::time::now() >= deadline {
            return false;
        }
        super::sched::prepare_to_sleep().sleep_until(crate::time::now() + 10_000_000);
    }
}

/// The `channels` kernel thread: does the teardowns deferred from where
/// the kernel could not sleep.
pub fn worker() -> ! {
    loop {
        let wait = super::sched::prepare_to_wait(work_chan());
        if WORK.lock().is_none() {
            wait.sleep();
        } else {
            drop(wait);
            run_deferred();
        }
    }
}

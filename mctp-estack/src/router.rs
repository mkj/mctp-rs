// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * Copyright (c) 2024-2025 Code Construct
 */

//! MCTP Routing

#[allow(unused)]
use crate::fmt::{debug, error, info, trace, warn};

use core::cell::RefCell;
use core::future::{poll_fn, Future};
use core::pin::pin;
use core::task::Poll;

use crate::reassemble::Reassembler;
use crate::{
    AppCookie, Fragmenter, ReceiveHandle, SendOutput, Stack, MAX_MTU,
    MAX_PAYLOAD,
};
use mctp::{Eid, Error, MsgIC, MsgType, Result, Tag, TagValue};

use embassy_sync::waitqueue::{MultiWakerRegistration, WakerRegistration};
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};

use heapless::Vec;

// TODO sizing is a bit arbitrary. They don't take up much space.
const MAX_LISTENERS: usize = 20;
const MAX_RECEIVERS: usize = 50;

// TODO: feature to configure mutex?
type RawMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
type AsyncMutex<T> = embassy_sync::mutex::Mutex<RawMutex, T>;
type BlockingMutex<T> =
    embassy_sync::blocking_mutex::Mutex<RawMutex, RefCell<T>>;

type PortRawMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
// type PortRawMutex = embassy_sync::blocking_mutex::raw::NoopRawMutex;

// Identifier for a Port
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId(pub u8);

/// A trait implemented by applications to determine the routing table.
pub trait PortLookup: Send {
    /// Returns the `PortId` for a destination EID.
    ///
    /// `PortId` is an index into the array of `ports` provided to [`Router::new`]
    ///
    /// Return `None` to drop the packet as unreachable. This lookup
    /// is only called for outbound packets - packets destined for the local EID
    /// will not be passed to this callback.
    ///
    /// `source_port` is the incoming interface of a forwarded packet,
    /// or `None` for locally generated packets.
    fn by_eid(
        &mut self,
        eid: Eid,
        source_port: Option<PortId>,
    ) -> Option<PortId>;
}

/// Used like `heapless::Vec`, but lets the mut buffer be written into
/// without zero-fill every time.
struct PktBuf {
    data: [u8; MAX_MTU],
    len: usize,
    dest: Eid,
}

impl PktBuf {
    const fn new() -> Self {
        Self {
            data: [0u8; MAX_MTU],
            len: 0,
            dest: Eid(0),
        }
    }

    fn set(&mut self, data: &[u8]) -> Result<()> {
        let hdr = Reassembler::header(data);
        debug_assert!(hdr.is_ok());
        let hdr = hdr?;
        let dst = self.data.get_mut(..data.len()).ok_or(Error::NoSpace)?;
        dst.copy_from_slice(data);
        self.len = data.len();
        self.dest = Eid(hdr.dest_endpoint_id());
        Ok(())
    }
}

impl core::ops::Deref for PktBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

/// The "producer" side of a queue of packets to send out a MCTP port/interface.
///
/// It will be used by `Routing` to enqueue packets to a port.
pub struct PortTop<'a> {
    /// Forwarded packet queue.
    /// The outer mutex will not be held over an await.
    packets: AsyncMutex<Sender<'a, PortRawMutex, PktBuf>>,

    /// Temporary storage to flatten vectorised local sent messages
    // prior to fragmentation and queueing.
    message: AsyncMutex<Vec<u8, MAX_PAYLOAD>>,

    mtu: usize,
}

impl PortTop<'_> {
    /// Enqueues a packet.
    ///
    /// Do not call with locks held.
    /// May block waiting for a port queue to flush.
    /// Packet must be a valid MCTP packet, may panic otherwise.
    async fn forward_packet(&self, pkt: &[u8]) -> Result<()> {
        debug_assert!(Reassembler::header(pkt).is_ok());

        let mut sender = self.packets.lock().await;
        // Note: must not await while holding `sender`

        // Check space first (can't rollback after try_send)
        if pkt.len() > self.mtu {
            debug!("Forward packet too large");
            return Err(Error::NoSpace);
        }

        // Get a slot to send
        let slot = sender.try_send().ok_or_else(|| {
            debug!("Dropped forward packet");
            Error::TxFailure
        })?;

        // Fill the buffer
        // OK unwrap: pkt.len() checked above.
        slot.set(pkt).unwrap();
        sender.send_done();
        Ok(())
    }

    /// Fragments and enqueues a message.
    ///
    /// Do not call with locks held.
    /// May block waiting for a port queue to flush.
    async fn send_message(
        &self,
        fragmenter: &mut Fragmenter,
        pkt: &[&[u8]],
    ) -> Result<Tag> {
        trace!("send_message");
        let mut msg;
        let payload = if pkt.len() == 1 {
            // Avoid the copy when sending a single slice
            pkt[0]
        } else {
            msg = self.message.lock().await;
            msg.clear();
            for p in pkt {
                msg.extend_from_slice(p).map_err(|_| {
                    debug!("Message too large");
                    Error::NoSpace
                })?;
            }
            &msg
        };

        loop {
            let mut sender = self.packets.lock().await;

            let qpkt = sender.send().await;
            qpkt.len = 0;
            qpkt.dest = fragmenter.dest();
            let r = fragmenter.fragment(payload, &mut qpkt.data);
            match r {
                SendOutput::Packet(p) => {
                    qpkt.len = p.len();
                    sender.send_done();
                    if fragmenter.is_done() {
                        break Ok(fragmenter.tag());
                    }
                }
                SendOutput::Error { err, .. } => {
                    debug!("Error packetising");
                    sender.send_done();
                    break Err(err);
                }
                SendOutput::Complete { .. } => unreachable!(),
            }
        }
    }
}

/// The "consumer" side of a queue of packets to send out a MCTP interface,
///
/// An MCTP transport implementation will read packets to send with
/// [`outbound()`](Self::outbound).
pub struct PortBottom<'a> {
    /// packet queue
    packets: Receiver<'a, PortRawMutex, PktBuf>,
}

impl PortBottom<'_> {
    /// Retrieve an outbound packet to send for this port.
    ///
    /// Should call [`outbound_done()`](Self::outbound_done) to consume the
    /// packet and advance the queue.
    /// `outbound()` may be called multiple times to peek at the same packet.
    /// Also returns the destination EID.
    pub async fn outbound(&mut self) -> (&[u8], Eid) {
        if self.packets.len() > 1 {
            trace!("packets avail {}", self.packets.len());
        }
        let pkt = self.packets.receive().await;
        (pkt, pkt.dest)
    }

    /// Attempt to retrieve an outbound packet.
    ///
    /// This is the same as [`outbound()`](Self::outbound) but returns
    /// `None` immediately if not available.
    ///
    /// Should call [`outbound_done()`](Self::outbound_done) to consume the
    /// packet and advance the queue.
    /// `try_outbound()` may be called multiple times to peek at the same packet.
    pub fn try_outbound(&mut self) -> Option<(&[u8], Eid)> {
        trace!("packets avail {} try", self.packets.len());
        self.packets.try_receive().map(|pkt| (&**pkt, pkt.dest))
    }

    /// Consume the outbound packet and advance the queue.
    pub fn outbound_done(&mut self) {
        self.packets.receive_done()
    }
}

/// Storage for a Port, being a physical MCTP interface.
// TODO: instead of storing Vec<u8, N>, it could
// store `&'r []` and a length field, which would allow different ports
// have different MAX_MESSAGE/MAX_MTU. Does add another lifetime parameter.
pub struct PortStorage<const FORWARD_QUEUE: usize = 4> {
    /// forwarded packet queue
    packets: [PktBuf; FORWARD_QUEUE],
}

impl<const FORWARD_QUEUE: usize> PortStorage<FORWARD_QUEUE> {
    pub fn new() -> Self {
        Self {
            packets: [const { PktBuf::new() }; FORWARD_QUEUE],
        }
    }
}

impl<const FORWARD_QUEUE: usize> Default for PortStorage<FORWARD_QUEUE> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PortBuilder<'a> {
    /// forwarded packet queue
    packets: Channel<'a, PortRawMutex, PktBuf>,
}

impl<'a> PortBuilder<'a> {
    pub fn new<const FORWARD_QUEUE: usize>(
        storage: &'a mut PortStorage<FORWARD_QUEUE>,
    ) -> Self {
        // PortBuilder and PortStorage need to be separate structs, since
        // zerocopy_channel::Channel takes a slice.
        Self {
            packets: Channel::new(storage.packets.as_mut_slice()),
        }
    }

    pub fn build(&mut self, mtu: usize) -> Result<(PortTop, PortBottom)> {
        if mtu > MAX_MTU {
            debug!("port mtu {} > MAX_MTU {}", mtu, MAX_MTU);
            return Err(Error::BadArgument);
        }

        let (ps, pr) = self.packets.split();

        let t = PortTop {
            message: AsyncMutex::new(Vec::new()),
            packets: AsyncMutex::new(ps),
            mtu,
        };
        let b = PortBottom { packets: pr };
        Ok((t, b))
    }
}

/// An async MCTP stack with routing.
///
/// This interfaces between transport ports and MCTP using applications.
///
/// Applications can use [`req()`](Self::req) and [`listener()`](Self::listener)
/// to obtain instances of the [`mctp`] async traits.
///
/// Device-provided input handlers feed input MCTP packets to
/// [`inbound()`](Self::inbound).
///
/// For outbound packets each port has queue split into `PortTop` and `PortBottom`.
/// `Router` will feed packets for a port into the top, and device output handlers
/// will read from [`PortBottom`] and write out the specific MCTP transport.
///
/// [`update_time()`](Self::update_time) should be called periodically to
/// handle timeouts.
///
/// Packets not destined for the local EID will be forwarded out a port
/// determined by the user-provided [`PortLookup`] implementation.
pub struct Router<'r> {
    inner: AsyncMutex<RouterInner<'r>>,
    ports: &'r [PortTop<'r>],

    /// Listeners for different message types.
    // Has a separate non-async Mutex so it can be used by RouterAsyncListener::drop()
    // TODO filter by more than just MsgType, maybe have a Map of some sort?
    app_listeners:
        BlockingMutex<[Option<(MsgType, WakerRegistration)>; MAX_LISTENERS]>,
}

pub struct RouterInner<'r> {
    /// Core MCTP stack
    stack: Stack,

    // Wakers for RouterAsyncReqChannel and RouterAsyncRespChannel
    app_receive_wakers: MultiWakerRegistration<MAX_RECEIVERS>,

    lookup: &'r mut dyn PortLookup,
}

impl<'r> Router<'r> {
    /// Create a new Router.
    ///
    /// The EID of the provided `stack` is used to match local destination packets.
    ///
    /// `ports` is a list of transport interfaces for the router. The indices
    /// of the `ports`  slice are used as `PortId` identifiers.
    ///
    /// `lookup` callbacks define the routing table for outbound packets.
    pub fn new(
        stack: Stack,
        ports: &'r [PortTop<'r>],
        lookup: &'r mut dyn PortLookup,
    ) -> Self {
        let inner = RouterInner {
            stack,
            app_receive_wakers: MultiWakerRegistration::new(),
            lookup,
        };

        Self {
            inner: AsyncMutex::new(inner),
            app_listeners: BlockingMutex::new(RefCell::new(
                [const { None }; MAX_LISTENERS],
            )),
            ports,
        }
    }

    /// Called periodically to update the clock and check timeouts.
    ///
    /// A suitable interval (milliseconds) for the next call to `update_time()` will
    /// be returned, currently a maximum of 100 ms.
    pub async fn update_time(&self, now_millis: u64) -> Result<u64> {
        let mut inner = self.inner.lock().await;
        let (next, expired) = inner.stack.update(now_millis)?;
        if expired {
            // Wake pending sockets in case one was waiting on a now-expired response.
            // TODO something more efficient, maybe Reassembler should hold a waker?
            inner.app_receive_wakers.wake();
        }
        Ok(next)
    }

    /// Provide an incoming packet to the router.
    ///
    /// This expects a single MCTP packet, with no transport binding header.
    ///
    /// Returns the packet's MCTP source EID for any valid packet,
    /// regardless of whether the packet is handled, forwarded, or dropped.
    pub async fn inbound(&self, pkt: &[u8], port: PortId) -> Option<Eid> {
        let mut inner = self.inner.lock().await;

        let Ok(header) = Reassembler::header(pkt) else {
            return None;
        };
        // Source EID is returned even if packet routing fails
        let ret_src = Some(Eid(header.source_endpoint_id()));

        // Handle locally if possible
        if inner.stack.is_local_dest(pkt) {
            match inner.stack.receive(pkt) {
                // Complete message
                Ok(Some((msg, handle))) => {
                    let typ = msg.typ;
                    let tag = msg.tag;
                    drop(inner);
                    self.incoming_local(tag, typ, handle).await;
                    return ret_src;
                }
                // Fragment consumed, message is incomplete
                Ok(None) => {
                    return ret_src;
                }
                Err(e) => {
                    debug!("Dropped local recv packet. {}", e);
                    return ret_src;
                }
            }
        }

        // Look for a route to forward to
        let dest_eid = Eid(header.dest_endpoint_id());

        let Some(p) = inner.lookup.by_eid(dest_eid, Some(port)) else {
            debug!("No route for recv {}", dest_eid);
            return ret_src;
        };
        drop(inner);

        let Some(top) = self.ports.get(p.0 as usize) else {
            debug!("Bad port ID from lookup");
            return ret_src;
        };

        let _ = top.forward_packet(pkt).await;
        ret_src
    }

    async fn incoming_local(
        &self,
        tag: Tag,
        typ: MsgType,
        handle: ReceiveHandle,
    ) {
        trace!("incoming local, type {}", typ.0);
        if tag.is_owner() {
            self.incoming_listener(typ, handle).await
        } else {
            self.incoming_response(tag, handle).await
        }
    }

    async fn incoming_listener(&self, typ: MsgType, handle: ReceiveHandle) {
        let mut inner = self.inner.lock().await;
        let mut handle = Some(handle);

        // wake the packet listener
        self.app_listeners.lock(|a| {
            let mut a = a.borrow_mut();
            // Find the matching listener
            for (cookie, entry) in a.iter_mut().enumerate() {
                if let Some((t, waker)) = entry {
                    trace!("entry. {} vs {}", t.0, typ.0);
                    if *t == typ {
                        // OK unwrap: only set once
                        let handle = handle.take().unwrap();
                        inner
                            .stack
                            .set_cookie(&handle, Some(AppCookie(cookie)));
                        inner.stack.return_handle(handle);
                        waker.wake();
                        trace!("listener match");
                        break;
                    }
                }
            }
        });

        if let Some(handle) = handle.take() {
            trace!("listener no match");
            inner.stack.finished_receive(handle);
        }
    }

    async fn incoming_response(&self, _tag: Tag, handle: ReceiveHandle) {
        let mut inner = self.inner.lock().await;
        inner.stack.return_handle(handle);
        // TODO: inefficient waking them all. should
        // probably wake only the useful one.
        inner.app_receive_wakers.wake();
    }

    fn app_bind(&self, typ: MsgType) -> Result<AppCookie> {
        self.app_listeners.lock(|a| {
            let mut a = a.borrow_mut();

            // Check for existing binds with the same type
            for bind in a.iter() {
                if bind.as_ref().is_some_and(|(t, _)| *t == typ) {
                    return Err(Error::AddrInUse);
                }
            }

            // Find a free slot
            if let Some((i, bind)) =
                a.iter_mut().enumerate().find(|(_i, bind)| bind.is_none())
            {
                *bind = Some((typ, WakerRegistration::new()));
                return Ok(AppCookie(i));
            }

            Err(Error::NoSpace)
        })
    }

    fn app_unbind(&self, cookie: AppCookie) -> Result<()> {
        self.app_listeners.lock(|a| {
            let mut a = a.borrow_mut();
            let bind = a.get_mut(cookie.0).ok_or(Error::BadArgument)?;

            if bind.is_none() {
                return Err(Error::BadArgument);
            }

            // Clear the bind.
            *bind = None;
            // No need to wake any waker, unbind only occurs
            // on RouterAsyncListener::drop.
            Ok(())
        })
    }

    /// Receive a message.
    ///
    /// Listeners will pass the cookie returned from `[app_bind]`.
    /// Other receivers will pass `tag_eid`.
    async fn app_recv_message<'f>(
        &self,
        cookie: Option<AppCookie>,
        tag_eid: Option<(Tag, Eid)>,
        buf: &'f mut [u8],
    ) -> Result<(&'f mut [u8], Eid, MsgType, Tag, MsgIC)> {
        // Allow single use inside poll_fn
        let mut buf = Some(buf);

        poll_fn(|cx| {
            // Lock it inside the poll_fn
            let l = self.inner.lock();
            let l = pin!(l);
            let mut inner = match l.poll(cx) {
                Poll::Ready(i) => i,
                Poll::Pending => return Poll::Pending,
            };

            trace!("poll recv message");

            // Find the message's handle
            // TODO: get_deferred is inefficient lookup, does it matter?
            let handle = match (cookie, tag_eid) {
                // lookup by cookie for Listener
                (Some(cookie), None) => {
                    inner.stack.get_deferred_bycookie(&[cookie])
                }
                // lookup by tag/eid for ReqChannel
                (None, Some((tag, eid))) => inner.stack.get_deferred(eid, tag),
                // one of them must have been set
                _ => unreachable!(),
            };

            let Some(handle) = handle else {
                // No message handle. Maybe it hasn't arrived yet, find the waker
                // to register.

                if let Some(cookie) = cookie {
                    // This is a Listener.
                    trace!("listener, cookie index {}", cookie.0);
                    self.app_listeners.lock(|a| {
                        let mut a = a.borrow_mut();
                        let Some(bind) = a.get_mut(cookie.0) else {
                            debug_assert!(false, "recv bad cookie");
                            return;
                        };
                        let Some((_typ, waker)) = bind else {
                            debug_assert!(false, "recv no listener");
                            return;
                        };
                        waker.register(cx.waker());
                    });
                } else {
                    // Other receivers.
                    trace!("other recv");
                    inner.app_receive_wakers.register(cx.waker());
                }
                trace!("pending");
                return Poll::Pending;
            };

            // A matching message was found. Fetch it, copy the contents to the caller,
            // and finish with it for the stack.
            trace!("got handle");

            let msg = inner.stack.fetch_message(&handle);

            // OK unwrap, set above and only hit once on Poll::Ready
            let buf = buf.take().unwrap();
            let res = if msg.payload.len() > buf.len() {
                trace!("no space");
                Err(Error::NoSpace)
            } else {
                trace!("good len {}", msg.payload.len());
                let buf = &mut buf[..msg.payload.len()];
                buf.copy_from_slice(msg.payload);
                Ok((buf, msg.source, msg.typ, msg.tag, msg.ic))
            };

            inner.stack.finished_receive(handle);
            Poll::Ready(res)
        })
        .await
    }

    /// Used by traits to send a message, see comment on .send_vectored() methods
    ///
    /// TODO should handle loopback if eid matches local stack's
    async fn app_send_message(
        &self,
        eid: Eid,
        typ: MsgType,
        tag: Option<Tag>,
        tag_expires: bool,
        integrity_check: MsgIC,
        buf: &[&[u8]],
        cookie: Option<AppCookie>,
    ) -> Result<Tag> {
        let mut inner = self.inner.lock().await;

        let Some(p) = inner.lookup.by_eid(eid, None) else {
            debug!("No route for recv {}", eid);
            return Err(Error::TxFailure);
        };

        let Some(top) = self.ports.get(p.0 as usize) else {
            debug!("Bad port ID from lookup");
            return Err(Error::TxFailure);
        };

        let mtu = top.mtu;
        let mut fragmenter = inner
            .stack
            .start_send(
                eid,
                typ,
                tag,
                tag_expires,
                integrity_check,
                Some(mtu),
                cookie,
            )
            .inspect_err(|e| trace!("error fragmenter {}", e))?;
        // release to allow other ports to continue work
        drop(inner);

        top.send_message(&mut fragmenter, buf).await
    }

    /// Only needs to be called for tags allocated with tag_expires=false
    ///
    /// Must only be called for owned tags.
    async fn app_release_tag(&self, eid: Eid, tag: Tag) {
        let Tag::Owned(tv) = tag else { unreachable!() };
        let mut inner = self.inner.lock().await;

        if let Err(e) = inner.stack.cancel_flow(eid, tv) {
            warn!("flow cancel failed {}", e);
        }
    }

    /// Create a `AsyncReqChannel` instance.
    pub fn req(&'r self, eid: Eid) -> RouterAsyncReqChannel<'r> {
        RouterAsyncReqChannel::new(eid, self)
    }

    /// Create a `AsyncListener` instance.
    ///
    /// Will receive incoming messages with the TO bit set for the given `typ`.
    pub fn listener(&'r self, typ: MsgType) -> Result<RouterAsyncListener<'r>> {
        let cookie = self.app_bind(typ)?;
        Ok(RouterAsyncListener {
            cookie,
            router: self,
        })
    }

    /// Retrieve the EID assigned to the local stack
    pub async fn get_eid(&self) -> Eid {
        let inner = self.inner.lock().await;
        inner.stack.own_eid
    }

    /// Set the EID assigned to the local stack
    pub async fn set_eid(&self, eid: Eid) -> mctp::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.stack.set_eid(eid.0)
    }
}

/// A request channel.
pub struct RouterAsyncReqChannel<'r> {
    eid: Eid,
    sent_tag: Option<Tag>,
    router: &'r Router<'r>,
    tag_expires: bool,
}

impl<'r> RouterAsyncReqChannel<'r> {
    fn new(eid: Eid, router: &'r Router<'r>) -> Self {
        RouterAsyncReqChannel {
            eid,
            sent_tag: None,
            tag_expires: true,
            router,
        }
    }

    /// Set the tag to not expire. That allows multiple calls to `send()`.
    ///
    /// `async_drop` must be called prior to drop.
    pub fn tag_noexpire(&mut self) -> Result<()> {
        if self.sent_tag.is_some() {
            return Err(Error::BadArgument);
        }
        self.tag_expires = false;
        Ok(())
    }

    /// This must be called prior to drop whenever `tag_noexpire()` is used.
    ///
    /// A workaround until async drop is implemented in Rust itself.
    /// <https://github.com/rust-lang/rust/issues/126482>
    pub async fn async_drop(self) {
        if !self.tag_expires {
            if let Some(tag) = self.sent_tag {
                self.router.app_release_tag(self.eid, tag).await;
            }
        }
    }
}

impl Drop for RouterAsyncReqChannel<'_> {
    fn drop(&mut self) {
        if !self.tag_expires && self.sent_tag.is_some() {
            warn!("Didn't call async_drop()");
        }
    }
}

/// A request channel
///
/// Created with [`Router::req()`](Router::req).
impl mctp::AsyncReqChannel for RouterAsyncReqChannel<'_> {
    /// Send a message.
    ///
    /// This will async block until the message has been enqueued to the physical port.
    /// Note that it will return failure immediately if the MCTP stack has no available tags,
    /// that behaviour may need changing in future.
    ///
    /// Subsequent calls will fail unless tag_noexpire() was performed.
    async fn send_vectored(
        &mut self,
        typ: MsgType,
        integrity_check: MsgIC,
        bufs: &[&[u8]],
    ) -> Result<()> {
        // For the first call, we pass a None tag, get an Owned one allocated.
        // Subsequent calls will fail unless tag_noexpire() was performed.
        let tag = self
            .router
            .app_send_message(
                self.eid,
                typ,
                self.sent_tag,
                self.tag_expires,
                integrity_check,
                bufs,
                None,
            )
            .await?;
        debug_assert!(matches!(tag, Tag::Owned(_)));
        self.sent_tag = Some(tag);
        Ok(())
    }

    async fn recv<'f>(
        &mut self,
        buf: &'f mut [u8],
    ) -> Result<(MsgType, MsgIC, &'f mut [u8])> {
        let Some(Tag::Owned(tv)) = self.sent_tag else {
            debug!("recv without send");
            return Err(Error::BadArgument);
        };
        let recv_tag = Tag::Unowned(tv);
        let (buf, eid, typ, tag, ic) = self
            .router
            .app_recv_message(None, Some((recv_tag, self.eid)), buf)
            .await?;
        debug_assert_eq!(tag, recv_tag);
        debug_assert_eq!(eid, self.eid);
        Ok((typ, ic, buf))
    }

    fn remote_eid(&self) -> Eid {
        self.eid
    }
}

/// A response channel.
///
/// Returned by [`RouterAsyncListener::recv`](mctp::AsyncListener::recv).
pub struct RouterAsyncRespChannel<'r> {
    eid: Eid,
    tv: TagValue,
    router: &'r Router<'r>,
    typ: MsgType,
}

impl<'r> mctp::AsyncRespChannel for RouterAsyncRespChannel<'r> {
    type ReqChannel<'a>
        = RouterAsyncReqChannel<'r>
    where
        Self: 'a;

    /// Send a message.
    ///
    /// See description of `RouterAsyncReqChannel::send_vectored()`.
    async fn send_vectored(
        &mut self,
        integrity_check: MsgIC,
        bufs: &[&[u8]],
    ) -> Result<()> {
        let tag = Some(Tag::Unowned(self.tv));
        self.router
            .app_send_message(
                self.eid,
                self.typ,
                tag,
                false,
                integrity_check,
                bufs,
                None,
            )
            .await?;
        Ok(())
    }

    fn remote_eid(&self) -> Eid {
        self.eid
    }

    fn req_channel(&self) -> mctp::Result<Self::ReqChannel<'_>> {
        Ok(RouterAsyncReqChannel::new(self.eid, self.router))
    }
}

/// A listener.
///
/// Created with [`Router::listener()`](Router::listener).
pub struct RouterAsyncListener<'r> {
    router: &'r Router<'r>,
    cookie: AppCookie,
}

impl<'r> mctp::AsyncListener for RouterAsyncListener<'r> {
    // type RespChannel<'a> = RouterAsyncRespChannel<'a> where Self: 'a;
    type RespChannel<'a>
        = RouterAsyncRespChannel<'r>
    where
        Self: 'a;

    async fn recv<'f>(
        &mut self,
        buf: &'f mut [u8],
    ) -> mctp::Result<(MsgType, MsgIC, &'f mut [u8], Self::RespChannel<'_>)>
    {
        let (msg, eid, typ, tag, ic) = self
            .router
            .app_recv_message(Some(self.cookie), None, buf)
            .await?;

        let Tag::Owned(tv) = tag else {
            debug_assert!(false, "listeners only accept owned tags");
            return Err(Error::InternalError);
        };

        let resp = RouterAsyncRespChannel {
            eid,
            tv,
            router: self.router,
            typ,
        };
        Ok((typ, ic, msg, resp))
    }
}

impl Drop for RouterAsyncListener<'_> {
    fn drop(&mut self) {
        if self.router.app_unbind(self.cookie).is_err() {
            // should be infallible, cookie should be valid.
            debug_assert!(false, "bad unbind");
        }
    }
}

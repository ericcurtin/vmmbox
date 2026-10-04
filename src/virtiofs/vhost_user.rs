//! The vhost-user protocol, from the device's side: what QEMU sends to set a
//! device up, and the state it builds.
//!
//! Written for macOS, where the usual Rust implementation (rust-vmm's
//! `vhost-user-backend`) does not build: it is made of `epoll` and `eventfd`.
//! Here the kick and call descriptors are whatever QEMU gives us (pipes on
//! macOS, eventfds on Linux), read and written as plain byte streams.
//!
//! The protocol is documented at
//! <https://qemu.readthedocs.io/en/latest/interop/vhost-user.html>.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::num::Wrapping;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use vm_memory::{FileOffset, GuestAddress, GuestMemoryMmap};

use super::queue::Queue;

/// Header flags: protocol version 1, in the low two bits.
const VERSION: u32 = 0x1;
const REPLY: u32 = 0x4;
const NEED_REPLY: u32 = 0x8;

/// The messages vmmbox understands. Anything else gets no reply and is ignored,
/// which QEMU tolerates for features that were not negotiated.
mod req {
    pub const GET_FEATURES: u32 = 1;
    pub const SET_FEATURES: u32 = 2;
    pub const SET_OWNER: u32 = 3;
    pub const RESET_OWNER: u32 = 4;
    pub const SET_MEM_TABLE: u32 = 5;
    pub const SET_VRING_NUM: u32 = 8;
    pub const SET_VRING_ADDR: u32 = 9;
    pub const SET_VRING_BASE: u32 = 10;
    pub const GET_VRING_BASE: u32 = 11;
    pub const SET_VRING_KICK: u32 = 12;
    pub const SET_VRING_CALL: u32 = 13;
    pub const SET_VRING_ERR: u32 = 14;
    pub const GET_PROTOCOL_FEATURES: u32 = 15;
    pub const SET_PROTOCOL_FEATURES: u32 = 16;
    pub const GET_QUEUE_NUM: u32 = 17;
    pub const SET_VRING_ENABLE: u32 = 18;
}

/// Virtio feature bits offered to the guest. Indirect descriptors are not
/// among them: the queue code from libkrun only follows plain chains.
const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;
const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Protocol features offered to QEMU.
const PROTOCOL_F_MQ: u64 = 1 << 0;
const PROTOCOL_F_REPLY_ACK: u64 = 1 << 3;

/// One request queue and the high-priority queue (FORGETs).
pub const NUM_QUEUES: usize = 2;

/// Descriptors QEMU may pass with one message.
const MAX_FDS: usize = 8;
/// The most any message of the kinds we handle can carry.
const MAX_PAYLOAD: usize = 1024;

pub struct Message {
    pub request: u32,
    pub flags: u32,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

impl Message {
    fn needs_reply(&self) -> bool {
        self.flags & NEED_REPLY != 0
    }
}

/// Read the next message. `Ok(None)` is a clean hang-up by QEMU.
pub fn recv(sock: &UnixStream) -> io::Result<Option<Message>> {
    let mut header = [0u8; 12];
    let (n, fds) = recv_with_fds(sock.as_raw_fd(), &mut header)?;
    if n == 0 {
        return Ok(None);
    }
    let mut got = n;
    // A short first read is possible; the rest of the header follows plainly.
    let mut stream = sock;
    while got < header.len() {
        let m = stream.read(&mut header[got..])?;
        if m == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += m;
    }
    let word = |i: usize| u32::from_ne_bytes(header[i..i + 4].try_into().unwrap());
    let (request, flags, size) = (word(0), word(4), word(8) as usize);
    if flags & 0x3 != VERSION {
        return Err(io::Error::other(format!(
            "unsupported vhost-user version in flags {flags:#x}"
        )));
    }
    if size > MAX_PAYLOAD {
        return Err(io::Error::other(format!(
            "vhost-user message {request} claims {size} bytes of payload"
        )));
    }
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload)?;
    Ok(Some(Message {
        request,
        flags,
        payload,
        fds,
    }))
}

/// `recvmsg` into `buf`, collecting any descriptors passed with it.
fn recv_with_fds(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    // Room for MAX_FDS descriptors, aligned for cmsghdr.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;

    let n = loop {
        let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };

    let mut fds = Vec::new();
    // SAFETY: the control buffer was filled in by the kernel and is walked with
    // the libc accessors; each descriptor it carries is ours to own.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                let len = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                for i in 0..len / size_of::<RawFd>() {
                    let raw = data.add(i).read_unaligned();
                    // Never let a descriptor leak into a child process.
                    libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n, fds))
}

fn send_reply(mut sock: &UnixStream, request: u32, payload: &[u8]) -> io::Result<()> {
    debug!("  reply to {request}: {payload:02x?}");
    let mut out = Vec::with_capacity(12 + payload.len());
    out.extend_from_slice(&request.to_ne_bytes());
    out.extend_from_slice(&(VERSION | REPLY).to_ne_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(payload);
    sock.write_all(&out)
}

/// A region of guest RAM that QEMU shares with us, as it described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserRegion {
    pub guest_addr: u64,
    pub size: u64,
    /// The address of the region in QEMU's own address space. The ring
    /// addresses QEMU sends are in these terms.
    pub user_addr: u64,
}

impl UserRegion {
    /// Guest physical address of a QEMU virtual address inside this region.
    fn guest_for(&self, user: u64) -> Option<u64> {
        let off = user.checked_sub(self.user_addr)?;
        (off < self.size).then(|| self.guest_addr + off)
    }
}

pub struct Vring {
    pub queue: Queue,
    /// Readable when the guest has added buffers.
    pub kick: Option<File>,
    /// Written to tell the guest buffers were used.
    pub call: Option<File>,
    enabled: bool,
    addr_set: bool,
}

impl Vring {
    fn new() -> Self {
        Self {
            queue: Queue::new(super::QUEUE_SIZE),
            kick: None,
            call: None,
            enabled: false,
            addr_set: false,
        }
    }

    /// Whether the guest can use this queue: configured, with somewhere to be
    /// told about new buffers, and not disabled.
    pub fn running(&self) -> bool {
        self.enabled && self.addr_set && self.kick.is_some() && self.queue.size > 0
    }

    /// Tell the guest buffers were used.
    pub fn signal(&self) {
        if let Some(call) = &self.call {
            // Eight bytes suits an eventfd and a pipe alike. A full pipe means a
            // notification is already pending, which is as good.
            let _ = (&*call).write(&1u64.to_ne_bytes());
        }
    }
}

/// Everything QEMU has told us about the device.
pub struct Backend {
    pub mem: Option<GuestMemoryMmap>,
    regions: Vec<UserRegion>,
    features: u64,
    pub vrings: Vec<Vring>,
}

/// What to do after a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    /// QEMU asked for the device to be dropped.
    Stop,
}

impl Backend {
    pub fn new() -> Self {
        Self {
            mem: None,
            regions: Vec::new(),
            features: 0,
            vrings: (0..NUM_QUEUES).map(|_| Vring::new()).collect(),
        }
    }

    fn offered_features() -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_RING_F_EVENT_IDX | VHOST_USER_F_PROTOCOL_FEATURES
    }

    /// Handle one message from QEMU, replying on `sock` where the protocol says to.
    pub fn handle(&mut self, sock: &UnixStream, msg: Message) -> io::Result<Outcome> {
        debug!(
            "vhost-user request {} flags {:#x} payload {}B fds {}",
            msg.request,
            msg.flags,
            msg.payload.len(),
            msg.fds.len()
        );
        let reply_u64 = |value: u64| send_reply(sock, msg.request, &value.to_ne_bytes());
        // Messages that carry no reply of their own are acknowledged with a
        // status when the sender asked: 0 for success.
        let mut ack = msg.needs_reply();
        let mut outcome = Outcome::Continue;

        match msg.request {
            req::GET_FEATURES => {
                reply_u64(Self::offered_features())?;
                ack = false;
            }
            req::SET_FEATURES => {
                self.features = read_u64(&msg.payload, 0)? & Self::offered_features();
                let event_idx = self.features & VIRTIO_RING_F_EVENT_IDX != 0;
                for v in &mut self.vrings {
                    v.queue.set_event_idx(event_idx);
                }
            }
            req::SET_OWNER => {}
            req::RESET_OWNER => outcome = Outcome::Stop,
            req::GET_PROTOCOL_FEATURES => {
                reply_u64(PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK)?;
                ack = false;
            }
            req::SET_PROTOCOL_FEATURES => {}
            req::GET_QUEUE_NUM => {
                reply_u64(NUM_QUEUES as u64)?;
                ack = false;
            }
            req::SET_MEM_TABLE => self.set_mem_table(&msg)?,
            req::SET_VRING_NUM => {
                let (index, num) = vring_state(&msg.payload)?;
                debug!("  queue {index} size {num}");
                let v = self.vring(index)?;
                v.queue.size = num as u16;
            }
            req::SET_VRING_ADDR => self.set_vring_addr(&msg.payload)?,
            req::SET_VRING_BASE => {
                let (index, num) = vring_state(&msg.payload)?;
                let q = &mut self.vring(index)?.queue;
                q.next_avail = Wrapping(num as u16);
                q.next_used = Wrapping(num as u16);
            }
            req::GET_VRING_BASE => {
                let (index, _) = vring_state(&msg.payload)?;
                let v = self.vring(index)?;
                let base = v.queue.next_avail.0 as u32;
                // Asking for the base is how QEMU stops a ring.
                v.enabled = false;
                v.kick = None;
                v.call = None;
                let mut out = [0u8; 8];
                out[..4].copy_from_slice(&index.to_ne_bytes());
                out[4..].copy_from_slice(&base.to_ne_bytes());
                send_reply(sock, msg.request, &out)?;
                ack = false;
            }
            req::SET_VRING_KICK | req::SET_VRING_CALL | req::SET_VRING_ERR => {
                debug!(
                    "  vring fd message: word {:#x}",
                    read_u64(&msg.payload, 0).unwrap_or(0)
                );
                self.set_vring_fd(&msg)?;
            }
            req::SET_VRING_ENABLE => {
                let (index, num) = vring_state(&msg.payload)?;
                debug!("  queue {index} enable {num}");
                let v = self.vring(index)?;
                v.enabled = num != 0;
                v.queue.ready = v.enabled;
            }
            other => {
                debug!("ignoring vhost-user request {other}");
                // An unknown request that wanted an answer gets a failure, not
                // silence, so QEMU does not wait on it.
                if ack {
                    send_reply(sock, other, &1u64.to_ne_bytes())?;
                    ack = false;
                }
            }
        }
        if ack {
            send_reply(sock, msg.request, &0u64.to_ne_bytes())?;
        }
        Ok(outcome)
    }

    fn vring(&mut self, index: u32) -> io::Result<&mut Vring> {
        self.vrings
            .get_mut(index as usize)
            .ok_or_else(|| io::Error::other(format!("no such queue {index}")))
    }

    fn set_mem_table(&mut self, msg: &Message) -> io::Result<()> {
        let p = &msg.payload;
        let nregions = read_u32(p, 0)? as usize;
        if nregions != msg.fds.len() || p.len() < 8 + nregions * 32 {
            return Err(io::Error::other(format!(
                "memory table of {nregions} regions with {} descriptors in {} bytes",
                msg.fds.len(),
                p.len()
            )));
        }
        let mut regions = Vec::new();
        let mut ranges = Vec::new();
        for (i, fd) in msg.fds.iter().enumerate() {
            let at = 8 + i * 32;
            let (guest_addr, size, user_addr, offset) = (
                read_u64(p, at)?,
                read_u64(p, at + 8)?,
                read_u64(p, at + 16)?,
                read_u64(p, at + 24)?,
            );
            // Our own handle on the memory: mapping must not consume the
            // descriptor QEMU passed, which the message still owns.
            let file = File::from(fd.try_clone()?);
            ranges.push((
                GuestAddress(guest_addr),
                size as usize,
                Some(FileOffset::new(file, offset)),
            ));
            regions.push(UserRegion {
                guest_addr,
                size,
                user_addr,
            });
        }
        let mem = GuestMemoryMmap::from_ranges_with_files(ranges)
            .map_err(|e| io::Error::other(format!("mapping guest memory: {e}")))?;
        debug!("guest memory: {} region(s) mapped", regions.len());
        self.regions = regions;
        self.mem = Some(mem);
        Ok(())
    }

    /// Ring addresses arrive as QEMU virtual addresses; the queue works in
    /// guest physical ones.
    fn guest_addr(&self, user: u64) -> io::Result<GuestAddress> {
        self.regions
            .iter()
            .find_map(|r| r.guest_for(user))
            .map(GuestAddress)
            .ok_or_else(|| {
                io::Error::other(format!("ring address {user:#x} is outside guest memory"))
            })
    }

    fn set_vring_addr(&mut self, p: &[u8]) -> io::Result<()> {
        let index = read_u32(p, 0)?;
        let desc = self.guest_addr(read_u64(p, 8)?)?;
        let used = self.guest_addr(read_u64(p, 16)?)?;
        let avail = self.guest_addr(read_u64(p, 24)?)?;
        let v = self.vring(index)?;
        v.queue.desc_table = desc;
        v.queue.used_ring = used;
        v.queue.avail_ring = avail;
        v.addr_set = true;
        Ok(())
    }

    fn set_vring_fd(&mut self, msg: &Message) -> io::Result<()> {
        let word = read_u64(&msg.payload, 0)?;
        let index = (word & 0xff) as u32;
        // Bit 8: no descriptor accompanies this message.
        let no_fd = word & 0x100 != 0;
        let fd = if no_fd {
            None
        } else {
            msg.fds.first().map(|fd| fd.try_clone()).transpose()?
        };
        let file = fd.map(File::from);
        if let Some(f) = &file {
            // Never block in the event loop on a descriptor of QEMU's.
            unsafe {
                let fl = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(f.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
        }
        let v = self.vring(index)?;
        match msg.request {
            req::SET_VRING_KICK => v.kick = file,
            req::SET_VRING_CALL => v.call = file,
            _ => {}
        }
        Ok(())
    }
}

fn read_u32(p: &[u8], at: usize) -> io::Result<u32> {
    p.get(at..at + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
        .ok_or_else(|| io::Error::other("short vhost-user payload"))
}

fn read_u64(p: &[u8], at: usize) -> io::Result<u64> {
    p.get(at..at + 8)
        .map(|b| u64::from_ne_bytes(b.try_into().unwrap()))
        .ok_or_else(|| io::Error::other("short vhost-user payload"))
}

/// `{ index, num }`, the payload of several messages.
fn vring_state(p: &[u8]) -> io::Result<(u32, u32)> {
    Ok((read_u32(p, 0)?, read_u32(p, 4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(request: u32, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&request.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        m.extend_from_slice(payload);
        m
    }

    /// Read one reply from the frontend's end of the socket.
    fn read_reply(mut sock: &UnixStream) -> (u32, u32, Vec<u8>) {
        let mut h = [0u8; 12];
        sock.read_exact(&mut h).unwrap();
        let w = |i: usize| u32::from_ne_bytes(h[i..i + 4].try_into().unwrap());
        let mut payload = vec![0u8; w(8) as usize];
        sock.read_exact(&mut payload).unwrap();
        (w(0), w(4), payload)
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn features_are_offered_and_masked_to_what_we_support() {
        let (qemu, ours) = pair();
        let mut b = Backend::new();
        (&qemu)
            .write_all(&frame(req::GET_FEATURES, VERSION, &[]))
            .unwrap();
        let msg = recv(&ours).unwrap().unwrap();
        assert_eq!(b.handle(&ours, msg).unwrap(), Outcome::Continue);
        let (request, flags, payload) = read_reply(&qemu);
        assert_eq!(request, req::GET_FEATURES);
        assert_eq!(flags, VERSION | REPLY);
        let features = u64::from_ne_bytes(payload.try_into().unwrap());
        assert_eq!(features, Backend::offered_features());
        assert!(features & VIRTIO_F_VERSION_1 != 0);
        assert!(features & VHOST_USER_F_PROTOCOL_FEATURES != 0);

        // Acknowledging more than was offered is ignored, not trusted.
        (&qemu)
            .write_all(&frame(req::SET_FEATURES, VERSION, &u64::MAX.to_ne_bytes()))
            .unwrap();
        let msg = recv(&ours).unwrap().unwrap();
        b.handle(&ours, msg).unwrap();
        assert_eq!(b.features, Backend::offered_features());
    }

    #[test]
    fn protocol_features_and_queue_count() {
        let (qemu, ours) = pair();
        let mut b = Backend::new();
        for (request, expect) in [
            (
                req::GET_PROTOCOL_FEATURES,
                PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK,
            ),
            (req::GET_QUEUE_NUM, NUM_QUEUES as u64),
        ] {
            (&qemu).write_all(&frame(request, VERSION, &[])).unwrap();
            let msg = recv(&ours).unwrap().unwrap();
            b.handle(&ours, msg).unwrap();
            let (r, _, payload) = read_reply(&qemu);
            assert_eq!(r, request);
            assert_eq!(u64::from_ne_bytes(payload.try_into().unwrap()), expect);
        }
    }

    #[test]
    fn a_reply_is_asked_for_and_given_for_messages_that_have_none() {
        let (qemu, ours) = pair();
        let mut b = Backend::new();
        (&qemu)
            .write_all(&frame(req::SET_OWNER, VERSION | NEED_REPLY, &[]))
            .unwrap();
        let msg = recv(&ours).unwrap().unwrap();
        b.handle(&ours, msg).unwrap();
        let (request, _, payload) = read_reply(&qemu);
        assert_eq!(request, req::SET_OWNER);
        assert_eq!(u64::from_ne_bytes(payload.try_into().unwrap()), 0);

        // Without the flag, no reply: the next thing written is the next reply.
        (&qemu)
            .write_all(&frame(req::SET_OWNER, VERSION, &[]))
            .unwrap();
        (&qemu)
            .write_all(&frame(req::GET_QUEUE_NUM, VERSION, &[]))
            .unwrap();
        for _ in 0..2 {
            let msg = recv(&ours).unwrap().unwrap();
            b.handle(&ours, msg).unwrap();
        }
        assert_eq!(read_reply(&qemu).0, req::GET_QUEUE_NUM);
    }

    #[test]
    fn an_unknown_request_that_wants_an_answer_gets_a_failure() {
        let (qemu, ours) = pair();
        let mut b = Backend::new();
        (&qemu)
            .write_all(&frame(9999, VERSION | NEED_REPLY, &[]))
            .unwrap();
        let msg = recv(&ours).unwrap().unwrap();
        b.handle(&ours, msg).unwrap();
        let (request, _, payload) = read_reply(&qemu);
        assert_eq!(request, 9999);
        assert_eq!(u64::from_ne_bytes(payload.try_into().unwrap()), 1);
    }

    #[test]
    fn user_addresses_translate_to_guest_addresses() {
        let r = UserRegion {
            guest_addr: 0x4000_0000,
            size: 0x1000,
            user_addr: 0x7000_0000_0000,
        };
        assert_eq!(r.guest_for(0x7000_0000_0000), Some(0x4000_0000));
        assert_eq!(r.guest_for(0x7000_0000_0fff), Some(0x4000_0fff));
        assert_eq!(r.guest_for(0x7000_0000_1000), None, "one past the end");
        assert_eq!(r.guest_for(0x6fff_ffff_ffff), None, "before the start");
    }

    #[test]
    fn a_vring_runs_only_when_fully_set_up() {
        let mut v = Vring::new();
        assert!(!v.running());
        v.queue.size = 256;
        v.addr_set = true;
        v.enabled = true;
        assert!(!v.running(), "no kick descriptor yet");
        let (a, _b) = pair();
        v.kick = Some(File::from(OwnedFd::from(a)));
        assert!(v.running());
        v.enabled = false;
        assert!(!v.running(), "disabled");
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        let (qemu, ours) = pair();
        // A payload that is claimed larger than any message we handle.
        (&qemu)
            .write_all(
                &frame(req::SET_FEATURES, VERSION, &[])
                    .iter()
                    .copied()
                    .take(8)
                    .chain((MAX_PAYLOAD as u32 + 1).to_ne_bytes())
                    .collect::<Vec<u8>>(),
            )
            .unwrap();
        assert!(recv(&ours).is_err());

        let (qemu, ours) = pair();
        (&qemu)
            .write_all(&frame(req::SET_FEATURES, 0x2, &[0; 8]))
            .unwrap();
        assert!(recv(&ours).is_err(), "wrong protocol version");

        // A vring index that does not exist.
        let mut b = Backend::new();
        let (qemu, ours) = pair();
        let mut p = Vec::new();
        p.extend_from_slice(&7u32.to_ne_bytes());
        p.extend_from_slice(&16u32.to_ne_bytes());
        (&qemu)
            .write_all(&frame(req::SET_VRING_NUM, VERSION, &p))
            .unwrap();
        let msg = recv(&ours).unwrap().unwrap();
        assert!(b.handle(&ours, msg).is_err());
    }

    #[test]
    fn a_hang_up_is_not_an_error() {
        let (qemu, ours) = pair();
        drop(qemu);
        assert!(recv(&ours).unwrap().is_none());
    }
}

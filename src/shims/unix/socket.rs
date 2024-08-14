use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::io::{Error, ErrorKind, Read};
use std::rc::{Rc, Weak};

use crate::shims::unix::fd::{FdId, WeakFileDescriptionRef};
use crate::shims::unix::linux::epoll::EpollReadyEvents;
use crate::shims::unix::*;
use crate::{concurrency::VClock, *};

/// The maximum capacity of the socketpair buffer in bytes.
/// This number is arbitrary as the value can always
/// be configured in the real system.
const MAX_SOCKETPAIR_BUFFER_CAPACITY: usize = 212992;

/// Pair of connected sockets.
#[derive(Debug)]
struct SocketPair {
    // By making the write link weak, a `write` can detect when all readers are
    // gone, and trigger EPIPE as appropriate.
    writebuf: Weak<RefCell<Buffer>>,
    readbuf: Rc<RefCell<Buffer>>,
    /// When a socketpair instance is created, two socketpair file descriptions are generated.
    /// The peer_fd field holds a weak reference to the file description of peer socketpair.
    // TODO: It might be possible to retrieve writebuf from peer_fd and remove the writebuf
    // field above.
    peer_fd: WeakFileDescriptionRef,
    is_nonblock: bool,
}

#[derive(Debug)]
struct Buffer {
    buf: VecDeque<u8>,
    clock: VClock,
    /// Indicates if there is at least one active writer to this buffer.
    /// If all writers of this buffer are dropped, buf_has_writer becomes false and we
    /// indicate EOF instead of blocking.
    buf_has_writer: bool,
}

impl FileDescription for SocketPair {
    fn name(&self) -> &'static str {
        "socketpair"
    }

    fn get_epoll_ready_events<'tcx>(&self) -> InterpResult<'tcx, EpollReadyEvents> {
        // We only check the status of EPOLLIN, EPOLLOUT and EPOLLRDHUP flags. If other event flags
        // need to be supported in the future, the check should be added here.

        let mut epoll_ready_events = EpollReadyEvents::new();
        let readbuf = self.readbuf.borrow();

        // Check if it is readable.
        if !readbuf.buf.is_empty() {
            epoll_ready_events.epollin = true;
        }

        // Check if is writable.
        if let Some(writebuf) = self.writebuf.upgrade() {
            let writebuf = writebuf.borrow();
            let data_size = writebuf.buf.len();
            let available_space = MAX_SOCKETPAIR_BUFFER_CAPACITY.strict_sub(data_size);
            if available_space != 0 {
                epoll_ready_events.epollout = true;
            }
        }

        // Check if the peer_fd closed
        if self.peer_fd.upgrade().is_none() {
            epoll_ready_events.epollrdhup = true;
            // This is an edge case. Whenever epollrdhup is triggered, epollin will be added
            // even though there is no data in the buffer.
            epoll_ready_events.epollin = true;
        }
        Ok(epoll_ready_events)
    }

    fn close<'tcx>(
        self: Box<Self>,
        _communicate_allowed: bool,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<()>> {
        // This is used to signal socketfd of other side that there is no writer to its readbuf.
        // If the upgrade fails, there is no need to update as all read ends have been dropped.
        if let Some(writebuf) = self.writebuf.upgrade() {
            writebuf.borrow_mut().buf_has_writer = false;
        };

        // Notify peer fd that closed has happened.
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            // When any of the event happened, we check and update the status of all supported events
            // types of peer fd.
            peer_fd.check_and_update_readiness(ecx)?;
        }
        Ok(Ok(()))
    }

    fn read<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        _fd_id: FdId,
        bytes: &mut [u8],
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        let request_byte_size = bytes.len();
        let mut readbuf = self.readbuf.borrow_mut();

        // Always succeed on read size 0.
        if request_byte_size == 0 {
            return Ok(Ok(0));
        }

        if readbuf.buf.is_empty() {
            if !readbuf.buf_has_writer {
                // Socketpair with no writer and empty buffer.
                // 0 bytes successfully read indicates end-of-file.
                return Ok(Ok(0));
            } else {
                if self.is_nonblock {
                    // Non-blocking socketpair with writer and empty buffer.
                    // https://linux.die.net/man/2/read
                    // EAGAIN or EWOULDBLOCK can be returned for socket,
                    // POSIX.1-2001 allows either error to be returned for this case.
                    // Since there is no ErrorKind for EAGAIN, WouldBlock is used.
                    return Ok(Err(Error::from(ErrorKind::WouldBlock)));
                } else {
                    // Blocking socketpair with writer and empty buffer.
                    // FIXME: blocking is currently not supported
                    throw_unsup_format!("socketpair read: blocking isn't supported yet");
                }
            }
        }

        // Synchronize with all previous writes to this buffer.
        // FIXME: this over-synchronizes; a more precise approach would be to
        // only sync with the writes whose data we will read.
        ecx.acquire_clock(&readbuf.clock);

        // Do full read / partial read based on the space available.
        // Conveniently, `read` exists on `VecDeque` and has exactly the desired behavior.
        let actual_read_size = readbuf.buf.read(bytes).unwrap();

        // The readbuf needs to be explicitly dropped because it will cause panic when
        // check_and_update_readiness borrows it again.
        drop(readbuf);

        // A notification should be provided for the peer file description even when it can
        // only write 1 byte. This implementation is not compliant with the actual Linux kernel
        // implementation. For optimization reasons, the kernel will only mark the file description
        // as "writable" when it can write more than a certain number of bytes. Since we
        // don't know what that *certain number* is, we will provide a notification every time
        // a read is successful. This might result in our epoll emulation providing more
        // notifications than the real system.
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            peer_fd.check_and_update_readiness(ecx)?;
        }

        return Ok(Ok(actual_read_size));
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        _fd_id: FdId,
        bytes: &[u8],
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        let write_size = bytes.len();
        // Always succeed on write size 0.
        // ("If count is zero and fd refers to a file other than a regular file, the results are not specified.")
        if write_size == 0 {
            return Ok(Ok(0));
        }

        let Some(writebuf) = self.writebuf.upgrade() else {
            // If the upgrade from Weak to Rc fails, it indicates that all read ends have been
            // closed.
            return Ok(Err(Error::from(ErrorKind::BrokenPipe)));
        };
        let mut writebuf = writebuf.borrow_mut();
        let data_size = writebuf.buf.len();
        let available_space = MAX_SOCKETPAIR_BUFFER_CAPACITY.strict_sub(data_size);
        if available_space == 0 {
            if self.is_nonblock {
                // Non-blocking socketpair with a full buffer.
                return Ok(Err(Error::from(ErrorKind::WouldBlock)));
            } else {
                // Blocking socketpair with a full buffer.
                throw_unsup_format!("socketpair write: blocking isn't supported yet");
            }
        }
        // Remember this clock so `read` can synchronize with us.
        if let Some(clock) = &ecx.release_clock() {
            writebuf.clock.join(clock);
        }
        // Do full write / partial write based on the space available.
        let actual_write_size = write_size.min(available_space);
        writebuf.buf.extend(&bytes[..actual_write_size]);

        // The writebuf needs to be explicitly dropped because it will cause panic when
        // check_and_update_readiness borrows it again.
        drop(writebuf);
        // Notification should be provided for peer fd as it became readable.
        if let Some(peer_fd) = self.peer_fd.upgrade() {
            peer_fd.check_and_update_readiness(ecx)?;
        }
        return Ok(Ok(actual_write_size));
    }
}

impl<'tcx> EvalContextExt<'tcx> for crate::MiriInterpCx<'tcx> {}
pub trait EvalContextExt<'tcx>: crate::MiriInterpCxExt<'tcx> {
    /// For more information on the arguments see the socketpair manpage:
    /// <https://linux.die.net/man/2/socketpair>
    fn socketpair(
        &mut self,
        domain: &OpTy<'tcx>,
        type_: &OpTy<'tcx>,
        protocol: &OpTy<'tcx>,
        sv: &OpTy<'tcx>,
    ) -> InterpResult<'tcx, Scalar> {
        let this = self.eval_context_mut();

        let domain = this.read_scalar(domain)?.to_i32()?;
        let mut type_ = this.read_scalar(type_)?.to_i32()?;
        let protocol = this.read_scalar(protocol)?.to_i32()?;
        let sv = this.deref_pointer(sv)?;

        let mut is_sock_nonblock = false;

        // Parse and remove the type flags that we support. If type != 0 after removing,
        // unsupported flags are used.
        if type_ & this.eval_libc_i32("SOCK_STREAM") == this.eval_libc_i32("SOCK_STREAM") {
            type_ &= !(this.eval_libc_i32("SOCK_STREAM"));
        }

        // SOCK_NONBLOCK only exists on Linux.
        if this.tcx.sess.target.os == "linux" {
            if type_ & this.eval_libc_i32("SOCK_NONBLOCK") == this.eval_libc_i32("SOCK_NONBLOCK") {
                is_sock_nonblock = true;
                type_ &= !(this.eval_libc_i32("SOCK_NONBLOCK"));
            }
            if type_ & this.eval_libc_i32("SOCK_CLOEXEC") == this.eval_libc_i32("SOCK_CLOEXEC") {
                type_ &= !(this.eval_libc_i32("SOCK_CLOEXEC"));
            }
        }

        // Fail on unsupported input.
        // AF_UNIX and AF_LOCAL are synonyms, so we accept both in case
        // their values differ.
        if domain != this.eval_libc_i32("AF_UNIX") && domain != this.eval_libc_i32("AF_LOCAL") {
            throw_unsup_format!(
                "socketpair: domain {:#x} is unsupported, only AF_UNIX \
                                 and AF_LOCAL are allowed",
                domain
            );
        } else if type_ != 0 {
            throw_unsup_format!(
                "socketpair: type {:#x} is unsupported, only SOCK_STREAM, \
                                 SOCK_CLOEXEC and SOCK_NONBLOCK are allowed",
                type_
            );
        } else if protocol != 0 {
            throw_unsup_format!(
                "socketpair: socket protocol {protocol} is unsupported, \
                                 only 0 is allowed",
            );
        }

        let buffer1 = Rc::new(RefCell::new(Buffer {
            buf: VecDeque::new(),
            clock: VClock::default(),
            buf_has_writer: true,
        }));

        let buffer2 = Rc::new(RefCell::new(Buffer {
            buf: VecDeque::new(),
            clock: VClock::default(),
            buf_has_writer: true,
        }));

        let socketpair_0 = SocketPair {
            writebuf: Rc::downgrade(&buffer1),
            readbuf: Rc::clone(&buffer2),
            peer_fd: WeakFileDescriptionRef::default(),
            is_nonblock: is_sock_nonblock,
        };
        let socketpair_1 = SocketPair {
            writebuf: Rc::downgrade(&buffer2),
            readbuf: Rc::clone(&buffer1),
            peer_fd: WeakFileDescriptionRef::default(),
            is_nonblock: is_sock_nonblock,
        };

        // Insert the file description to the fd table.
        let fds = &mut this.machine.fds;
        let sv0 = fds.insert_new(socketpair_0);
        let sv1 = fds.insert_new(socketpair_1);

        // Get weak file descriptor and file description id value.
        let fd_ref0 = fds.get_ref(sv0).unwrap();
        let fd_ref1 = fds.get_ref(sv1).unwrap();
        let weak_fd_ref0 = fd_ref0.downgrade();
        let weak_fd_ref1 = fd_ref1.downgrade();

        // Update peer_fd and id field.
        fd_ref1.borrow_mut().downcast_mut::<SocketPair>().unwrap().peer_fd = weak_fd_ref0;

        fd_ref0.borrow_mut().downcast_mut::<SocketPair>().unwrap().peer_fd = weak_fd_ref1;

        // Return socketpair file description value to the caller.
        let sv0 = Scalar::from_int(sv0, sv.layout.size);
        let sv1 = Scalar::from_int(sv1, sv.layout.size);

        this.write_scalar(sv0, &sv)?;
        this.write_scalar(sv1, &sv.offset(sv.layout.size, sv.layout, this)?)?;

        Ok(Scalar::from_i32(0))
    }
}

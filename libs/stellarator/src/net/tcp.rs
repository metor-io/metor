use crate::buf::{IoBuf, IoBufMut};
use crate::io::{AsyncRead, AsyncWrite};
use crate::os::BorrowedHandle;
use crate::reactor::{Completion, ops};
use crate::{BufResult, Error};
use socket2::{SockAddr, Socket};
use std::io::{self};
use std::net::{SocketAddr, ToSocketAddrs};

use super::SockAddrRaw;

pub struct TcpStream {
    socket: Socket,
}

impl TcpStream {
    pub async fn connect(addr: SocketAddr) -> Result<TcpStream, Error> {
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        #[cfg(not(target_os = "windows"))]
        socket.set_cloexec(true)?;
        socket.set_nonblocking(!cfg!(target_os = "linux"))?;
        let addr: SockAddr = addr.into();
        Completion::run(ops::Connect::new(&socket, Box::new(addr.into()))?).await?;

        Ok(TcpStream { socket })
    }

    pub async fn read<B: IoBufMut>(&self, buf: B) -> BufResult<usize, B> {
        Completion::run(ops::Read::new(self.as_handle(), buf, None)).await
    }

    pub async fn write<B: IoBuf>(&self, buf: B) -> BufResult<usize, B> {
        Completion::run(ops::Write::new(self.as_handle(), buf, None)).await
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.socket
            .peer_addr()
            .map(|addr| addr.as_socket().unwrap())
    }

    fn as_handle(&self) -> BorrowedHandle<'_> {
        BorrowedHandle::Socket(&self.socket)
    }
}

impl AsyncRead for TcpStream {
    fn read<B: IoBufMut>(&self, buf: B) -> impl std::future::Future<Output = BufResult<usize, B>> {
        self.read(buf)
    }
}

impl AsyncWrite for TcpStream {
    fn write<B: IoBuf>(&self, buf: B) -> impl std::future::Future<Output = BufResult<usize, B>> {
        self.write(buf)
    }
}

pub struct TcpListener {
    socket: socket2::Socket,
}

impl TcpListener {
    pub fn bind(addrs: impl ToSocketAddrs) -> io::Result<TcpListener> {
        let addr = addrs.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any addresses",
            )
        })?;
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;

        #[cfg(not(target_os = "windows"))]
        socket.set_cloexec(true)?;
        socket.set_reuse_address(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        socket.set_nonblocking(!cfg!(target_os = "linux"))?;

        Ok(TcpListener { socket })
    }

    pub async fn accept(&self) -> Result<TcpStream, Error> {
        let op = ops::Accept::new(&self.socket, Box::new(SockAddrRaw::zeroed()));
        let socket = Completion::run(op).await.0?;
        #[cfg(not(target_os = "windows"))]
        socket.set_cloexec(true)?;
        Ok(TcpStream { socket })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket
            .local_addr()
            .map(|addr| addr.as_socket().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rent, test};
    use std::time::Duration;

    #[test]
    async fn test_echo_server() {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = crate::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 128];
            let n = rent!(stream.read(buf).await, buf).unwrap();
            buf.truncate(n);
            stream.write(buf).await.0.unwrap();
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        stream.write(b"foo").await.0.unwrap();
        let mut buf = vec![0; 128];
        let n = rent!(stream.read(buf).await, buf).unwrap();
        assert_eq!(&buf[..n], b"foo");
        handle.await.unwrap();
    }

    /// A write parked on a full socket must still complete after a read on
    /// the same (split) socket registers its own interest: the reactor has
    /// to keep both filters armed, not let the later registration replace
    /// the earlier one.
    #[test]
    async fn blocked_write_survives_concurrent_read_registration() {
        use crate::io::{AsyncWrite, SplitExt};
        use std::io::Read as _;

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut peer = std::net::TcpStream::connect(addr).unwrap();
        let stream = listener.accept().await.unwrap();
        let (rx, tx) = stream.split();

        // Far more than the socket buffers hold, so the write parks.
        const LEN: usize = 16 << 20;
        let write = crate::spawn(async move { tx.write_all(vec![0xABu8; LEN]).await.0 });
        crate::sleep(Duration::from_millis(50)).await;
        // Now a read registers on the same fd while the write is parked.
        let read = crate::spawn(async move { rx.read(vec![0u8; 16]).await });
        crate::sleep(Duration::from_millis(50)).await;

        let drain = std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 << 10];
            let mut total = 0;
            while total < LEN {
                match peer.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
            }
            (peer, total)
        });

        let timeout = async {
            crate::sleep(Duration::from_secs(5)).await;
            panic!("write never completed: writable interest was lost");
        };
        futures_lite::future::race(async { write.await.unwrap().unwrap() }, timeout).await;
        let (_peer, total) = drain.join().unwrap();
        assert_eq!(total, LEN);
        drop(read);
    }
}

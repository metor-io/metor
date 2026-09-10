use std::{
    marker::PhantomData,
    net::SocketAddr,
    ops::{Deref, DerefMut},
    time::Duration,
};

use metor_proto::types::{
    IntoLenPacket, LenPacket, Msg, OwnedPacket, Request, RequestId, TryFromPacket,
};
use metor_proto_wkt::{DbInfoResp, ErrorResponse, GetDbInfo, LinkInfo};
use stellarator::{
    BufResult,
    buf::{IoBufMut, Slice},
    io::{AsyncRead, AsyncWrite, GrowableBuf, LengthDelReader, OwnedReader, OwnedWriter, SplitExt},
    net::TcpStream,
};

#[cfg(feature = "queue")]
pub mod queue;

pub struct PacketStream<R: AsyncRead> {
    reader: LengthDelReader<R>,
}

impl<R: AsyncRead> PacketStream<R> {
    pub fn new(reader: R) -> Self {
        let reader = LengthDelReader::new(reader);
        Self::from_reader(reader)
    }
    pub fn from_reader(reader: LengthDelReader<R>) -> Self {
        Self { reader }
    }

    pub async fn next<B: IoBufMut>(&mut self, buf: B) -> Result<OwnedPacket<Slice<B>>, Error> {
        let packet_buf = self.reader.recv(buf).await?;
        OwnedPacket::parse(packet_buf).map_err(Error::from)
    }

    pub async fn next_grow<B: IoBufMut + GrowableBuf>(
        &mut self,
        buf: B,
    ) -> Result<OwnedPacket<Slice<B>>, Error> {
        let packet_buf = self.reader.recv_growable(buf).await?;
        OwnedPacket::parse(packet_buf).map_err(Error::from)
    }
}

pub struct PacketSink<W: AsyncWrite> {
    writer: W,
}

impl<W: AsyncWrite> PacketSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub async fn send(&self, packet: impl IntoLenPacket) -> BufResult<(), LenPacket> {
        let packet = packet.into_len_packet();
        let (res, inner) = self.writer.write_all(packet.inner).await;
        (res, LenPacket { inner })
    }
}

pub struct Client {
    resp_buf: Option<Vec<u8>>,
    pub tx: PacketSink<OwnedWriter<TcpStream>>,
    pub rx: PacketStream<OwnedReader<TcpStream>>,
    next_req_id: u8,
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).await?;
        let (rx, tx) = stream.split();
        let tx = PacketSink::new(tx);
        let rx = PacketStream::new(rx);
        Ok(Client {
            tx,
            rx,
            next_req_id: 0,
            resp_buf: Some(vec![0u8; 256]),
        })
    }

    pub async fn send(&mut self, packet: impl IntoLenPacket) -> BufResult<(), LenPacket> {
        let len_pkt = packet.into_len_packet();
        self.tx.send(len_pkt).await
    }

    pub async fn request<R: Request + IntoLenPacket>(
        &mut self,
        req: R,
    ) -> Result<R::Reply<Slice<Vec<u8>>>, Error> {
        let req_id = self.next_req_id.wrapping_add(1);
        self.send(req.with_request_id(req_id)).await.0?;
        self.recv(req_id).await
    }

    pub async fn recv<O: TryFromPacket<Slice<Vec<u8>>>>(
        &mut self,
        req_id: RequestId,
    ) -> Result<O, Error> {
        loop {
            let buf = self.resp_buf.take().unwrap_or(vec![0u8; 256]);
            let pkt = self.rx.next_grow(buf).await?;
            if pkt.req_id() != req_id {
                println!("skipping msg because of mismatched req_id");
                self.resp_buf = Some(pkt.into_buf().into_inner());
                continue;
            }
            let res = match &pkt {
                OwnedPacket::Msg(m) if m.id == ErrorResponse::ID => {
                    match postcard::from_bytes::<ErrorResponse>(&m.buf) {
                        Ok(e) => Err(Error::Response(e)),
                        Err(e) => Err(Error::Postcard(e)),
                    }
                }
                pkt => O::try_from_packet(pkt).map_err(Error::from),
            };

            self.resp_buf = Some(pkt.into_buf().into_inner());
            return res;
        }
    }

    pub async fn stream<R: metor_proto::types::Request + IntoLenPacket>(
        &mut self,
        req: R,
    ) -> Result<SubStream<'_, R::Reply<Slice<Vec<u8>>>>, Error> {
        let req_id = self.next_req_id.wrapping_add(1);
        self.send(req.with_request_id(req_id)).await.0?;
        Ok(SubStream {
            req_id,
            client: self,
            _phantom_data: PhantomData,
        })
    }
}

/// Bound on the identity read: a peer that records unknown request
/// messages as telemetry and never replies would otherwise hang forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// What answered at the far end of one shared wire protocol.
pub enum Peer {
    /// Answered the probe: a metor-db server. The probe connection is
    /// dropped; callers hand off to a db client, which dials and
    /// supervises itself.
    Db(DbInfoResp),
    /// Pushed its identity unprompted: an fsw link server. The connection
    /// is live and the announce replay is already streaming behind the
    /// identity.
    Fsw {
        info: LinkInfo,
        rx: PacketStream<OwnedReader<TcpStream>>,
        tx: PacketSink<OwnedWriter<TcpStream>>,
        /// The recycled receive buffer, mid-flight from the identity read.
        buf: Vec<u8>,
    },
}

/// Dial `addr` and let the first packet say what lives there. Bounded by
/// the handshake timeout; a peer that answers with neither identity (or
/// nothing) is an error naming what was expected.
pub async fn identify(addr: SocketAddr) -> Result<Peer, Error> {
    let Client { tx, mut rx, .. } = Client::connect(addr).await?;
    tx.send((&GetDbInfo).with_request_id(1)).await.0?;
    let pkt = futures_lite::future::or(rx.next_grow(vec![0u8; 64 * 1024]), async {
        stellarator::sleep(HANDSHAKE_TIMEOUT).await;
        Err(stellarator::Error::from(std::io::Error::from(std::io::ErrorKind::TimedOut)).into())
    })
    .await?;
    let OwnedPacket::Msg(m) = &pkt else {
        return Err(identity_err("a table packet"));
    };
    if m.id == LinkInfo::ID {
        let info: LinkInfo =
            postcard::from_bytes(&m.buf).map_err(|_| identity_err("a malformed LinkInfo"))?;
        let buf = pkt.into_buf().into_inner();
        Ok(Peer::Fsw { info, rx, tx, buf })
    } else if m.id == DbInfoResp::ID {
        let resp: DbInfoResp =
            postcard::from_bytes(&m.buf).map_err(|_| identity_err("a malformed DbInfoResp"))?;
        Ok(Peer::Db(resp))
    } else {
        Err(identity_err("an unknown first message"))
    }
}

fn identity_err(got: &str) -> Error {
    Error::Stellar(
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "peer sent {got}; expected a DbInfoResp (metor-db) or LinkInfo (fsw link) identity"
            ),
        )
        .into(),
    )
}

pub struct SubStream<'a, R> {
    req_id: RequestId,
    client: &'a mut Client,
    _phantom_data: PhantomData<R>,
}

impl<R: TryFromPacket<Slice<Vec<u8>>>> SubStream<'_, R> {
    pub async fn next(&mut self) -> Result<R, Error> {
        self.client.recv(self.req_id).await
    }
}

impl<R> Deref for SubStream<'_, R> {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        self.client
    }
}

impl<R> DerefMut for SubStream<'_, R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.client
    }
}

pub struct ReplyStream {}

#[derive(thiserror::Error, Debug, miette::Diagnostic)]
pub enum Error {
    #[error("{0}")]
    Impeller(#[from] metor_proto::error::Error),
    #[error("{0}")]
    Stellar(#[from] stellarator::Error),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("invalid packet type")]
    InvalidPacketType,
    #[error("wait error {0}")]
    Wait(stellarator::sync::wait_map::WaitError),
    #[error("rx handle closed")]
    RxHandleClosed,
    #[error("{0}")]
    Response(ErrorResponse),
}

#[cfg(test)]
mod tests;

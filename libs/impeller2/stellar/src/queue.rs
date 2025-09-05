use bbq2::{queue::ArcBBQueue, traits::storage::BoxedSlice};
use futures_concurrency::future::Race;
use impeller2::types::{LenPacket, Msg};
use impeller2_bbq::*;
use impeller2_wkt::{NewConnection, StreamId};
use miette::{IntoDiagnostic, miette};
use std::net::SocketAddr;
use stellarator::{
    io::{AsyncRead, LengthDelReader, SplitExt},
    net::TcpStream,
};
use thingbuf::mpsc;

use crate::PacketStream;

pub async fn tcp_connect<I>(
    addr: SocketAddr,
    outgoing_packet_rx: &mut mpsc::Receiver<Option<LenPacket>>,
    incoming_packet_tx: &mut AsyncArcQueueTx,
    stream_id: StreamId,
    new_connection_packets: &impl Fn(StreamId) -> I,
    success: impl FnOnce(),
) -> Result<(), miette::Error>
where
    I: Iterator<Item = LenPacket>,
{
    let stream = TcpStream::connect(addr).await.into_diagnostic()?;
    let (rx, tx) = stream.split();
    let tx = crate::PacketSink::new(tx);
    let mut rx = LengthDelReader::<_, u32>::new(rx);

    let len_pkt = LenPacket::new(impeller2::types::PacketTy::Msg, NewConnection::ID, 0);
    let grant = PacketGrantW::new(
        incoming_packet_tx
            .grant(128)
            .map_err(|err| miette!("channel error {err:?}"))?,
    );
    grant.commit_len_pkt(len_pkt);

    for packet in new_connection_packets(stream_id) {
        tx.send(packet).await.0?;
    }
    success();
    let rx = async move {
        loop {
            let grant_r = incoming_packet_tx.wait_grant(512 * 1024).await;
            let grant_r = PacketGrantW::new(grant_r);
            let slice = rx.recv(grant_r).await.into_diagnostic()?;
            let len = slice.range().len();
            slice.into_inner().commit(len + 4);
        }
    };
    let tx = async move {
        while let Some(pkt) = outgoing_packet_rx.recv().await {
            let Some(pkt) = pkt else {
                continue;
            };
            tx.send(pkt).await.0.into_diagnostic()?;
        }
        Ok::<_, miette::Error>(())
    };
    (rx, tx).race().await
}

pub fn spawn_recv<R: AsyncRead + 'static>(
    mut stream: PacketStream<R>,
    queue_len: usize,
) -> AsyncArcQueueRx {
    let queue = ArcBBQueue::new_with_storage(BoxedSlice::new(queue_len));
    let (rx, tx) = queue.framed_split();
    stellarator::spawn(async move {
        loop {
            let grant_r = tx.wait_grant(512 * 1024).await;
            let grant_r = PacketGrantW::new(grant_r);
            let Ok(slice) = stream.reader.recv(grant_r).await else {
                continue;
            };
            let len = slice.range().len();
            slice.into_inner().commit(len + 4);
        }
    });
    rx
}

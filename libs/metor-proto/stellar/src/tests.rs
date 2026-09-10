use super::*;
use metor_proto::types::IntoLenPacket;
use metor_proto::types::{Msg, PacketId};
use postcard::experimental::max_size::MaxSize;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use stellarator::net::{TcpListener, TcpStream};

#[derive(Serialize, Deserialize, MaxSize, PartialEq, Debug)]
struct Foo {
    bar: u32,
}

impl Msg for Foo {
    const ID: PacketId = [0x1, 0x2];
}

#[stellarator::test]
async fn test_packet_echo() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let addr = listener.local_addr().unwrap();
    stellarator::spawn(async move {
        let sink = PacketSink::new(listener.accept().await.unwrap());
        let msg = Foo { bar: 0xBB }.into_len_packet();
        sink.send(msg).await.0.unwrap();
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut stream = PacketStream::new(stream);
    let buf = vec![0; 128];
    let OwnedPacket::Msg(m) = stream.next(buf).await.unwrap() else {
        panic!("non msg pkt");
    };
    assert_eq!(m.id, Foo::ID);
    let foo: Foo = m.parse().unwrap();
    assert_eq!(foo, Foo { bar: 0xBB });
}

const DATA: PacketId = [0x33, 7];

fn identity(command_ids: Vec<PacketId>) -> Vec<u8> {
    (&LinkInfo {
        protocol_version: metor_proto_wkt::LINK_PROTOCOL_VERSION,
        features: 0,
        command_ids,
        namespace: None,
        link: String::new(),
    })
        .into_len_packet()
        .inner
}

/// A hand-written fsw link server: accept one connection, push the
/// identity and one self-describing data msg, then hold the socket open.
/// No fsw dependency — the wire shapes are the contract.
fn fake_fsw(listener: TcpListener, command_ids: Vec<PacketId>) {
    stellarator::struc_con::stellar(move || async move {
        let stream = listener.accept().await.expect("accept");
        let (_rx, tx) = stream.split();
        tx.write_all(identity(command_ids))
            .await
            .0
            .expect("identity");
        let mut data = LenPacket::msg(DATA, 8);
        data.extend_from_slice(b"hello");
        tx.write_all(data.inner).await.0.expect("data");
        std::future::pending::<()>().await;
    });
}

#[stellarator::test]
async fn identify_tells_an_fsw_link() {
    const CMD_A: PacketId = [0x51, 0];
    const CMD_B: PacketId = [0x52, 0];

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    fake_fsw(listener, vec![CMD_A, CMD_B]);
    let peer = identify(addr).await.expect("identify");
    let Peer::Fsw { info, .. } = peer else {
        panic!("expected an fsw peer");
    };
    assert_eq!(
        info.protocol_version,
        metor_proto_wkt::LINK_PROTOCOL_VERSION
    );
    assert_eq!(info.command_ids, vec![CMD_A, CMD_B]);
}

#[stellarator::test]
async fn identify_times_out_on_a_silent_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept and hold the socket without ever writing.
    stellarator::struc_con::stellar(move || async move {
        let _stream = listener.accept().await;
        std::future::pending::<()>().await;
    });
    assert!(identify(addr).await.is_err(), "silence must not hang");
}

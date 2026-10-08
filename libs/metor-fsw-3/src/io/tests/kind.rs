#![cfg(test)]

use crate::io::*;

fn every_kind() -> [RealKind; 8] {
    [
        RealKind::Tty {
            path: "/dev/ttyS0".into(),
            baud: 9600,
        },
        RealKind::Can {
            iface: "can0".into(),
            filters: vec![CanFilter {
                id: 0x100,
                mask: 0x700,
            }],
        },
        RealKind::GpioLine {
            chip: "/dev/gpiochip0".into(),
            line: 4,
            direction: GpioDirection::Output,
        },
        RealKind::Udp {
            bind: "0.0.0.0:9000".into(),
            connect: Some("10.0.0.2:9000".into()),
        },
        RealKind::File {
            path: "/var/log/fsw.bin".into(),
            access: Access::Write,
        },
        RealKind::Dir {
            path: "/var/log".into(),
        },
        RealKind::TcpListen {
            addr: "0.0.0.0:2240".into(),
        },
        RealKind::TcpConnect {
            addr: "ground:2240".into(),
        },
    ]
}

#[test]
fn every_kind_round_trips() {
    for kind in every_kind() {
        let resource = ResourceConfig {
            id: "r".into(),
            backing: Backing::Real(kind),
        };
        let text = serde_json::to_string(&resource).unwrap();
        assert_eq!(
            serde_json::from_str::<ResourceConfig>(&text).unwrap(),
            resource
        );
    }
}

#[test]
fn defaults_fill_optional_fields() {
    let text = r#"{"id":"log","backing":{"real":{"file":{"path":"/tmp/x"}}}}"#;
    let resource: ResourceConfig = serde_json::from_str(text).unwrap();
    let Backing::Real(RealKind::File { access, .. }) = resource.backing else {
        panic!("a file");
    };
    assert_eq!(access, Access::ReadWrite);
}

#[test]
fn unknown_kind_and_missing_baud_fail() {
    let unknown = r#"{"id":"x","backing":{"real":{"spi":{"path":"/dev/spidev0"}}}}"#;
    assert!(serde_json::from_str::<ResourceConfig>(unknown).is_err());
    let no_baud = r#"{"id":"x","backing":{"real":{"tty":{"path":"/dev/ttyS0"}}}}"#;
    assert!(serde_json::from_str::<ResourceConfig>(no_baud).is_err());
}

#[test]
fn each_kind_names_its_interface() {
    let names: Vec<_> = every_kind().iter().map(RealKind::interface).collect();
    assert_eq!(
        names,
        [
            "uart",
            "can",
            "gpio_line",
            "udp",
            "file",
            "dir",
            "listen",
            "dial"
        ]
    );
    assert_eq!(names[0], <Uart as Interface>::NAME);
    assert_eq!(names[7], <Dial as Interface>::NAME);
}

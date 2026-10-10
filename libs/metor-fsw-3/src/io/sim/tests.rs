use crate::io::*;

#[cfg(test)]
mod unit {
    use metor_proto::types::Timestamp;

    use super::*;
    use crate::io::test::{ScriptedUart, TestDevice, device, device_shared};
    use crate::io::tests::allocations;

    const NOW: Timestamp = Timestamp(0);

    #[derive(Default)]
    struct Line {
        level: bool,
        ticks: u32,
    }

    impl GpioModel for Line {
        fn get(&mut self, _now: Timestamp) -> Result<bool, IoError> {
            Ok(self.level)
        }

        fn set(&mut self, _now: Timestamp, level: bool) -> Result<(), IoError> {
            self.level = level;
            Ok(())
        }

        fn tick(&mut self, _now: Timestamp) {
            self.ticks += 1;
        }
    }

    /// A `ScriptedUart` that waits `latency` serves before it sees an op.
    struct Slow {
        uart: ScriptedUart,
        latency: u32,
    }

    impl UartModel for Slow {
        fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize> {
            self.uart.rx(now, buf)
        }

        fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError> {
            self.uart.tx(now, bytes)
        }

        fn configure(&mut self, now: Timestamp, cfg: &TtyConfig) -> Result<(), IoError> {
            self.uart.configure(now, cfg)
        }

        fn tick(&mut self, _now: Timestamp) {}

        fn latency(&self, _kind: OpKind) -> u32 {
            self.latency
        }
    }

    fn read(driver: &mut TestDevice) -> Token {
        driver.request(Op::read(BufRef::new(0), None), b"").unwrap()
    }

    fn write(driver: &mut TestDevice, bytes: &[u8]) -> Token {
        driver
            .request(Op::write(BufRef::new(0), None), bytes)
            .unwrap()
    }

    fn results(driver: &mut TestDevice) -> Vec<(Token, Result<(), IoError>, Out)> {
        let replies = driver.replies().into_iter();
        replies
            .map(|c| (c.token, c.status.result(), c.out))
            .collect()
    }

    #[test]
    fn read_fills_the_driver_buffer() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::scripted([b"$GPGGA".to_vec()]);
        let token = read(&mut driver);
        dev.serve(NOW, &mut uart);
        assert_eq!(results(&mut driver), [(token, Ok(()), Out::len(6))]);
        assert_eq!(driver.driver_bytes(token), b"$GPGGA");
    }

    #[test]
    fn write_reaches_tx_once() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let token = write(&mut driver, b"$PMTK");
        dev.serve(NOW, &mut uart);
        dev.serve(NOW, &mut uart);
        assert_eq!(results(&mut driver), [(token, Ok(()), Out::len(5))]);
        assert_eq!(uart.written(), [b"$PMTK".to_vec()]);
    }

    #[test]
    fn gpio_get_answers_the_level() {
        let (mut dev, mut driver) = device::<GpioLine>();
        let mut line = Line {
            level: true,
            ..Line::default()
        };
        let get = driver.request(Op::gpio(GpioOp::GET), b"").unwrap();
        let set = driver.request(Op::gpio(GpioOp::set(0)), b"").unwrap();
        dev.serve(NOW, &mut line);
        let replies = results(&mut driver);
        assert_eq!(replies[0], (get, Ok(()), Out::len(1)));
        assert_eq!(replies[1], (set, Ok(()), Out::UNIT));
        assert!(!line.level);
        assert_eq!(line.ticks, 1);
    }

    #[test]
    fn latency_waits_that_many_serves() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = Slow {
            uart: ScriptedUart::scripted([b"x".to_vec()]),
            latency: 3,
        };
        let token = read(&mut driver);
        for _ in 0..3 {
            dev.serve(NOW, &mut uart);
            assert!(driver.replies().is_empty());
        }
        dev.serve(NOW, &mut uart);
        assert_eq!(results(&mut driver), [(token, Ok(()), Out::len(1))]);
    }

    #[test]
    fn empty_read_holds_until_bytes_arrive() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let token = read(&mut driver);
        for _ in 0..5 {
            dev.serve(NOW, &mut uart);
            assert!(driver.replies().is_empty());
        }
        uart.push(b"late");
        dev.serve(NOW, &mut uart);
        assert_eq!(results(&mut driver), [(token, Ok(()), Out::len(4))]);
    }

    #[test]
    fn ops_answer_in_arrival_order_across_handles() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let first = write(&mut driver, b"a");
        let config = Token::new(100);
        let tty = Op::tty(TtyOp::Configure(TtyConfig::new(9600)));
        driver.send(Sqe::new(config, RawHandle::new(1, 0), tty));
        let last = write(&mut driver, b"b");
        dev.serve(NOW, &mut uart);
        let tokens: Vec<Token> = driver.replies().iter().map(|c| c.token).collect();
        assert_eq!(tokens, [first, config, last]);
    }

    #[test]
    fn full_reply_port_loses_nothing() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let flush = Op::tty(TtyOp::flush(2));
        let mut sent = Vec::new();
        while dev.outbox.is_empty() {
            for _ in 0..MAX_OPS {
                let token = Token::new(sent.len() as u32);
                driver.send(Sqe::new(token, RawHandle::new(0, 0), flush));
                sent.push(token);
            }
            dev.serve(NOW, &mut uart);
        }
        let mut answered: Vec<Token> = Vec::new();
        for _ in 0..4 {
            answered.extend(driver.replies().iter().map(|c| c.token));
            dev.serve(NOW, &mut uart);
        }
        answered.extend(driver.replies().iter().map(|c| c.token));
        assert_eq!(answered, sent);
    }

    /// Cancels and refusals wait for room like any reply.
    #[test]
    fn full_reply_port_keeps_cancels_and_refusals() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let flush = Op::tty(TtyOp::flush(2));
        // Clear of the tokens `request` hands out.
        let mut next = 10_000;
        let mut send = |driver: &mut TestDevice, op: Op| {
            next += 1;
            driver.send(Sqe::new(Token::new(next), RawHandle::new(0, 0), op));
            Token::new(next)
        };
        while dev.outbox.is_empty() {
            (0..MAX_OPS).for_each(|_| {
                send(&mut driver, flush);
            });
            dev.serve(NOW, &mut uart);
        }
        let held: Vec<Token> = (0..MAX_OPS).map(|_| read(&mut driver)).collect();
        let refused = read(&mut driver);
        let cancel = send(&mut driver, Op::cancel(held[0]));
        dev.serve(NOW, &mut uart);
        let mut replies = Vec::new();
        for _ in 0..8 {
            replies.extend(results(&mut driver));
            dev.serve(NOW, &mut uart);
        }
        let find = |t: Token| replies.iter().find(|r| r.0 == t).map(|r| r.1);
        assert_eq!(find(refused), Some(Err(IoError::QueueFull)));
        assert_eq!(find(held[0]), Some(Err(IoError::Canceled)));
        assert_eq!(find(cancel), Some(Ok(())));
    }

    #[test]
    fn failed_tx_fails_the_op() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent().fail_write(Errno::EIO);
        let token = write(&mut driver, b"cmd");
        dev.serve(NOW, &mut uart);
        let failed = Err(IoError::Os(Errno::EIO));
        assert_eq!(results(&mut driver), [(token, failed, Out::UNIT)]);
    }

    #[test]
    fn full_held_refuses_but_still_cancels() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let held: Vec<Token> = (0..MAX_OPS).map(|_| read(&mut driver)).collect();
        let refused = read(&mut driver);
        let cancel = Token::new(500);
        driver.send(Sqe::new(cancel, RawHandle::new(0, 0), Op::cancel(held[0])));
        dev.serve(NOW, &mut uart);
        let replies = results(&mut driver);
        assert_eq!(replies[0], (refused, Err(IoError::QueueFull), Out::UNIT));
        assert_eq!(replies[1], (held[0], Err(IoError::Canceled), Out::UNIT));
        assert_eq!(replies[2], (cancel, Ok(()), Out::UNIT));
        assert_eq!(dev.held.len(), MAX_OPS - 1);
    }

    #[test]
    fn two_host_grants_hold_twice_as_many() {
        let (mut dev, mut driver) = device_shared::<Uart>(2);
        let mut uart = ScriptedUart::silent();
        (0..2 * MAX_OPS).for_each(|_| {
            read(&mut driver);
        });
        let refused = read(&mut driver);
        dev.serve(NOW, &mut uart);
        assert_eq!(dev.held.cap(), 2 * MAX_OPS);
        assert_eq!(dev.held.len(), 2 * MAX_OPS);
        assert_eq!(
            results(&mut driver),
            [(refused, Err(IoError::QueueFull), Out::UNIT)]
        );
    }

    #[test]
    fn gpio_op_on_a_uart_is_unsupported() {
        let (mut dev, mut driver) = device::<Uart>();
        let token = driver.request(Op::gpio(GpioOp::set(1)), b"").unwrap();
        dev.serve(NOW, &mut ScriptedUart::silent());
        let unsupported = Err(IoError::Unsupported);
        assert_eq!(results(&mut driver), [(token, unsupported, Out::UNIT)]);
    }

    #[test]
    fn cancel_of_a_held_read_lands_in_the_same_serve() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let token = read(&mut driver);
        dev.serve(NOW, &mut uart);
        let cancel = Token::new(500);
        driver.send(Sqe::new(cancel, RawHandle::new(0, 0), Op::cancel(token)));
        dev.serve(NOW, &mut uart);
        let replies = results(&mut driver);
        assert_eq!(replies[0], (token, Err(IoError::Canceled), Out::UNIT));
        assert_eq!(replies[1], (cancel, Ok(()), Out::UNIT));
        assert_eq!(dev.held.len(), 0);
    }

    #[test]
    fn cancel_of_an_unknown_op_is_enoent() {
        let (mut dev, mut driver) = device::<Uart>();
        let cancel = Token::new(500);
        driver.send(Sqe::new(
            cancel,
            RawHandle::new(0, 0),
            Op::cancel(Token::new(9)),
        ));
        dev.serve(NOW, &mut ScriptedUart::silent());
        let enoent = Err(IoError::Os(Errno::ENOENT));
        assert_eq!(results(&mut driver), [(cancel, enoent, Out::UNIT)]);
    }

    #[test]
    fn unresolved_buffer_is_bad_buf_and_skips_the_model() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::silent();
        let mut stray = BufRef::new(0);
        stray.stamp(TableId(9));
        let token = Token::new(900);
        driver.send(Sqe::new(
            token,
            RawHandle::new(0, 0),
            Op::write(stray, None),
        ));
        dev.serve(NOW, &mut uart);
        let bad = Err(IoError::BadBuf);
        assert_eq!(results(&mut driver), [(token, bad, Out::UNIT)]);
        assert!(uart.written().is_empty());
    }

    #[test]
    fn serve_allocates_nothing_after_bind() {
        let (mut dev, mut driver) = device::<Uart>();
        let mut uart = ScriptedUart::scripted([b"abcdefgh".to_vec()]);
        let reads: Vec<Token> = (0..4).map(|_| read(&mut driver)).collect();
        let cancel = Token::new(500);
        driver.send(Sqe::new(cancel, RawHandle::new(0, 0), Op::cancel(reads[3])));
        let n = allocations(|| {
            for _ in 0..3 {
                dev.serve(NOW, &mut uart);
            }
        });
        assert_eq!(n, 0);
        assert_eq!(driver.replies().len(), 3);
    }
}

#[cfg(kani)]
mod proofs {
    use super::super::Held;
    use super::*;

    /// No sequence of pushes and removals holds more than the capacity.
    #[kani::proof]
    #[kani::unwind(6)]
    fn held_never_exceeds_capacity() {
        let mut held = Held::new(2);
        for _ in 0..3 {
            let token = Token::new(u32::from(kani::any::<bool>()));
            match kani::any::<u8>() % 3 {
                0 => {
                    let sqe = Sqe::new(token, RawHandle::new(0, 0), Op::CLOSE);
                    let _ = held.push(sqe, kani::any());
                }
                1 => {
                    let _ = held.remove(token);
                }
                _ => held.remove_at(usize::from(kani::any::<bool>())),
            }
            assert!(held.len() <= held.cap());
        }
    }
}

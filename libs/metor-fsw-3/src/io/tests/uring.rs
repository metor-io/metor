use std::rc::Rc;

use crate::io::*;

fn table(tables: &Tables) -> Rc<BufTable> {
    let t = Rc::new(BufTable::new(1));
    tables.add(&t);
    t.alloc_from(b"fixed?").unwrap();
    t.freeze();
    t
}

/// Run on Linux: a table frozen at construction is a fixed buffer, and one
/// added later is not; reads into both land.
#[test]
fn fixed_buffers_only_for_tables_at_construction() {
    let tables = Tables::new();
    let early = table(&tables);
    let limits = Limits { ops: 4, sources: 2 };
    let mut os = UringOs::new(&limits, tables.clone(), &[]).unwrap();
    let late = table(&tables);
    assert!(os.fixed_index(early.id().unwrap()).is_some());
    assert!(os.fixed_index(late.id().unwrap()).is_none());
    let file = tempfile::tempfile().unwrap();
    let h = os.register(file.into(), FdClass::File);
    let mut reactor = Reactor::new(Box::new(os), limits);
    let sources = [reactor.source(early.clone()), reactor.source(late.clone())];
    for (i, src) in sources.into_iter().enumerate() {
        let sqe = Sqe::new(Token::new(i as u32), h, Op::write(BufRef::new(0), Some(0)));
        reactor.push(src, sqe);
    }
    reactor.submit();
    let mut done = Vec::new();
    for _ in 0..1000 {
        reactor.reap(&mut done);
        if done.len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(done.len(), 2);
    assert!(done.iter().all(|(_, c)| c.out.as_len() == Some(6)));
}

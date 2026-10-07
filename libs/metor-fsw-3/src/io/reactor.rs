use std::rc::Rc;

use super::{BufTable, Cqe, Errno, IoError, Op, Os, RawHandle, Sqe, Token};

/// Marks a reactor-own token.
const OWN: u32 = 0x8000_0000;
/// Submit and reap rounds `Drop` runs before it leaks what is left.
const TEARDOWN_ROUNDS: usize = 3;

/// A submitter: one system. Issued in order from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u16);

/// Fixed sizes for a reactor's tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Systems' ops in flight, across every source.
    pub ops: usize,
    pub sources: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    src: SourceId,
    token: Token,
}

/// Reactor tokens: a slab for systems' ops and one twice as long for the
/// reactor's own.
struct Tokens {
    ops: Vec<Option<Entry>>,
    free: Vec<u32>,
    own: Vec<bool>,
    own_free: Vec<u32>,
}

impl Tokens {
    fn new(ops: usize) -> Self {
        let own = 2 * ops;
        Self {
            ops: vec![None; ops],
            free: (0..ops as u32).rev().collect(),
            own: vec![false; own],
            own_free: (0..own as u32).rev().collect(),
        }
    }

    fn map(&mut self, src: SourceId, token: Token) -> Option<Token> {
        let rt = self.free.pop()?;
        self.ops[rt as usize] = Some(Entry { src, token });
        Some(Token::new(rt))
    }

    fn unmap(&mut self, rt: u32) -> Option<Entry> {
        let entry = self.ops.get_mut(rt as usize)?.take()?;
        self.free.push(rt);
        Some(entry)
    }

    fn find(&self, src: SourceId, token: Token) -> Option<u32> {
        let want = Some(Entry { src, token });
        self.ops.iter().position(|e| *e == want).map(|i| i as u32)
    }

    fn owner(&self, rt: u32) -> Option<SourceId> {
        self.ops.get(rt as usize)?.map(|e| e.src)
    }

    fn live(&self, src: SourceId) -> usize {
        self.ops.iter().flatten().filter(|e| e.src == src).count()
    }

    fn take_own(&mut self) -> Option<Token> {
        let i = self.own_free.pop()?;
        self.own[i as usize] = true;
        Some(Token::new(OWN | i))
    }

    fn free_own(&mut self, rt: u32) {
        let i = rt & !OWN;
        if let Some(taken) = self.own.get_mut(i as usize)
            && *taken
        {
            *taken = false;
            self.own_free.push(i);
        }
    }

    fn idle(&self) -> bool {
        self.free.len() == self.ops.len() && self.own_free.len() == self.own.len()
    }
}

/// Takes systems' ops to an `Os` and brings their completions back.
///
/// Each source's tokens map to reactor tokens on the way down and back on the
/// way up, and each source's `BufRef`s are stamped with its table.
pub struct Reactor {
    os: Box<dyn Os>,
    tokens: Tokens,
    sources: Vec<Rc<BufTable>>,
    queued: Vec<Sqe>,
    refused: Vec<(SourceId, Cqe)>,
    raw: Vec<Cqe>,
}

impl Reactor {
    pub fn new(os: Box<dyn Os>, limits: Limits) -> Self {
        let all = 3 * limits.ops;
        Self {
            os,
            tokens: Tokens::new(limits.ops),
            sources: Vec::with_capacity(limits.sources),
            queued: Vec::with_capacity(all),
            refused: Vec::with_capacity(limits.ops),
            raw: Vec::with_capacity(all),
        }
    }

    /// Adds a submitter whose `BufRef`s name `table`. Init only.
    pub fn source(&mut self, table: Rc<BufTable>) -> SourceId {
        // PANIC Safety: sources are bounded by a target's systems, far below 65536.
        let id = SourceId(u16::try_from(self.sources.len()).expect("source count fits u16"));
        self.sources.push(table);
        id
    }

    /// Maps the op's token, stamps its table, and queues it. A refused op
    /// completes on the next `reap`.
    pub fn push(&mut self, src: SourceId, sqe: Sqe) {
        match self.lower(src, sqe) {
            Ok(sqe) => self.queued.push(sqe),
            Err(e) => self.refused.push((src, Cqe::err(sqe.token, e))),
        }
    }

    fn lower(&mut self, src: SourceId, mut sqe: Sqe) -> Result<Sqe, IoError> {
        let table = self
            .sources
            .get(usize::from(src.0))
            .ok_or(IoError::Refused)?;
        if let Op::Cancel { target, .. } = sqe.op {
            let rt = self.tokens.find(src, target);
            let rt = rt.ok_or(IoError::Os(Errno::ENOENT))?;
            sqe.op = Op::cancel(Token::new(rt));
        }
        if let Some(buf) = sqe.op.buf_mut() {
            let id = table.id().ok_or(IoError::BadBuf)?;
            table.raw(buf.slot()).ok_or(IoError::BadBuf)?;
            buf.stamp(id);
        }
        sqe.token = self.tokens.map(src, sqe.token).ok_or(IoError::QueueFull)?;
        Ok(sqe)
    }

    /// Hands the queued ops to the `Os`.
    pub fn submit(&mut self) {
        for sqe in self.queued.drain(..) {
            self.os.push(sqe);
        }
        self.os.submit();
    }

    /// Collects what landed and maps each token back to its source's.
    pub fn reap(&mut self, done: &mut Vec<(SourceId, Cqe)>) {
        done.append(&mut self.refused);
        self.os.reap(&mut self.raw);
        for cqe in self.raw.drain(..) {
            if let Some(c) = raise(&mut self.tokens, cqe) {
                done.push(c);
            }
        }
    }

    /// Cancels every op `src` has in flight. Their completions still come back
    /// from `reap`.
    pub fn cancel_all(&mut self, src: SourceId) {
        for rt in 0..self.tokens.ops.len() as u32 {
            if self.tokens.owner(rt) == Some(src) {
                self.push_own(Sqe::new(
                    Token::new(0),
                    RawHandle::new(0, 0),
                    Op::cancel(Token::new(rt)),
                ));
            }
        }
    }

    /// Queues an op under a reactor-own token. `reap` drops its completion.
    pub fn push_own(&mut self, mut sqe: Sqe) {
        let Some(token) = self.tokens.take_own() else {
            tracing::error!(op = ?sqe.op.kind(), "reactor-own tokens exhausted");
            return;
        };
        sqe.token = token;
        self.queued.push(sqe);
    }

    /// Ops of `src` not yet returned by `reap`.
    pub fn in_flight(&self, src: SourceId) -> usize {
        self.tokens.live(src)
    }
}

/// Maps a completion back to its source, freeing its token.
fn raise(tokens: &mut Tokens, mut cqe: Cqe) -> Option<(SourceId, Cqe)> {
    let rt = cqe.token.get();
    if rt & OWN != 0 {
        tokens.free_own(rt);
        return None;
    }
    let entry = tokens.unmap(rt)?;
    cqe.token = entry.token;
    Some((entry.src, cqe))
}

impl Drop for Reactor {
    /// Cancels every op, reaps for a few rounds, and leaks the table of any
    /// source with ops still in flight, so the kernel never writes freed memory.
    fn drop(&mut self) {
        for sqe in core::mem::take(&mut self.queued) {
            raise(&mut self.tokens, Cqe::err(sqe.token, IoError::Canceled));
        }
        for i in 0..self.sources.len() {
            self.cancel_all(SourceId(i as u16));
        }
        let mut done = Vec::new();
        for _ in 0..TEARDOWN_ROUNDS {
            if self.tokens.idle() {
                break;
            }
            self.submit();
            self.reap(&mut done);
            done.clear();
        }
        for (i, table) in self.sources.iter().enumerate() {
            if self.tokens.live(SourceId(i as u16)) > 0 {
                core::mem::forget(table.clone());
            }
        }
    }
}

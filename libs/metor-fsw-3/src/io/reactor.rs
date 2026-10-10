use std::rc::Rc;

use super::{BufTable, Cqe, Errno, IoError, Op, Os, RawHandle, Sqe, Token};

/// The high bit set on the reactor's own tokens.
const OWN: u32 = 0x8000_0000;
/// How many submit and reap rounds `Drop` runs before it leaks the rest.
const TEARDOWN_ROUNDS: usize = 3;

/// A system that submits ops. Ids count up from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u16);

/// Fixed sizes for a reactor's tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most system ops in flight across all sources.
    pub ops: usize,
    pub sources: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    src: SourceId,
    token: Token,
    /// `cancel_all` already sent a cancel for this op.
    canceling: bool,
    /// The resource whose model answers the op, for a sim op.
    route: Option<u16>,
}

/// Allocates reactor tokens. System ops draw from one slab and the reactor's
/// own ops from a second slab twice as long.
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
        self.ops[rt as usize] = Some(Entry {
            src,
            token,
            canceling: false,
            route: None,
        });
        Some(Token::new(rt))
    }

    fn unmap(&mut self, rt: u32) -> Option<Entry> {
        let entry = self.ops.get_mut(rt as usize)?.take()?;
        self.free.push(rt);
        Some(entry)
    }

    fn find(&self, src: SourceId, token: Token) -> Option<u32> {
        let hit = |e: &Option<Entry>| e.is_some_and(|e| e.src == src && e.token == token);
        self.ops.iter().position(hit).map(|i| i as u32)
    }

    fn route(&mut self, rt: u32, route: u16) {
        if let Some(Some(e)) = self.ops.get_mut(rt as usize) {
            e.route = Some(route);
        }
    }

    /// Marks `src`'s op `rt` as canceling, returning its entry. `None` if it
    /// is not `src`'s or already canceling.
    fn mark_canceling(&mut self, src: SourceId, rt: u32) -> Option<Entry> {
        match self.ops.get_mut(rt as usize) {
            Some(Some(e)) if e.src == src && !e.canceling => {
                e.canceling = true;
                Some(*e)
            }
            _ => None,
        }
    }

    fn live(&self, src: SourceId) -> usize {
        self.ops.iter().flatten().filter(|e| e.src == src).count()
    }

    /// `src`'s ops in flight in the `Os`, not with a model.
    fn live_real(&self, src: SourceId) -> usize {
        let real = |e: &&Entry| e.src == src && e.route.is_none();
        self.ops.iter().flatten().filter(real).count()
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

/// Multiplexes the ops of many systems onto one `Os`.
///
/// The reactor swaps each op's token for a reactor-wide one on submit and
/// swaps it back on completion. It also fills in each `BufRef`'s table.
///
/// A sim op takes a token the same way but goes to a model instead of the
/// `Os`: the caller writes it to the model's device end and passes the reply
/// back through `raise`.
pub struct Reactor {
    os: Box<dyn Os>,
    tokens: Tokens,
    sources: Vec<Rc<BufTable>>,
    queued: Vec<Sqe>,
    refused: Vec<(SourceId, Cqe)>,
    raw: Vec<Cqe>,
    /// Cancels for sim ops, with the resource whose model takes each.
    sim_cancels: Vec<(u16, Sqe)>,
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
            sim_cancels: Vec::with_capacity(2 * limits.ops),
        }
    }

    /// Adds a source whose buffers live in `table`. Call only at init.
    pub fn source(&mut self, table: Rc<BufTable>) -> SourceId {
        // PANIC Safety: sources are bounded by a target's systems, far below 65536.
        let id = SourceId(u16::try_from(self.sources.len()).expect("source count fits u16"));
        self.sources.push(table);
        id
    }

    /// Queues an op. A refused op completes with an error on the next `reap`.
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

    /// Lowers a sim op for the model of resource `route`. A refused op
    /// completes with an error on the next `reap`.
    pub fn lower_routed(&mut self, src: SourceId, route: u16, sqe: Sqe) -> Option<Sqe> {
        match self.lower(src, sqe) {
            Ok(lowered) => {
                self.tokens.route(lowered.token.get(), route);
                Some(lowered)
            }
            Err(e) => {
                self.refused.push((src, Cqe::err(sqe.token, e)));
                None
            }
        }
    }

    /// Fails a lowered sim op that never reached its model. It completes with
    /// `e` on the next `reap`.
    pub fn unroute(&mut self, sqe: Sqe, e: IoError) {
        if let Some(done) = raise(&mut self.tokens, Cqe::err(sqe.token, e)) {
            self.refused.push(done);
        }
    }

    /// Restores a model reply's source token. `None` for an unknown or
    /// reactor-own token.
    pub fn raise(&mut self, cqe: Cqe) -> Option<(SourceId, Cqe)> {
        raise(&mut self.tokens, cqe)
    }

    /// Moves the cancels `cancel_all` queued for sim ops into `out`.
    pub fn sim_cancels(&mut self, out: &mut Vec<(u16, Sqe)>) {
        out.append(&mut self.sim_cancels);
    }

    /// Queues a sim cancel again, after its model's port refused it.
    pub fn requeue_sim_cancel(&mut self, route: u16, sqe: Sqe) {
        self.sim_cancels.push((route, sqe));
    }

    /// Submits the queued ops to the `Os`.
    pub fn submit(&mut self) {
        for sqe in self.queued.drain(..) {
            self.os.push(sqe);
        }
        self.os.submit();
    }

    /// Moves finished completions into `done` and restores each source's token.
    pub fn reap(&mut self, done: &mut Vec<(SourceId, Cqe)>) {
        done.append(&mut self.refused);
        self.os.reap(&mut self.raw);
        for cqe in self.raw.drain(..) {
            if let Some(c) = raise(&mut self.tokens, cqe) {
                done.push(c);
            }
        }
    }

    /// Cancels every op `src` has in flight, once each. `reap` still returns
    /// their completions. A sim op's cancel goes to `sim_cancels`, and its
    /// model's reply returns the completion.
    pub fn cancel_all(&mut self, src: SourceId) {
        for rt in 0..self.tokens.ops.len() as u32 {
            let Some(entry) = self.tokens.mark_canceling(src, rt) else {
                continue;
            };
            let cancel = Sqe::new(
                Token::new(0),
                RawHandle::new(0, 0),
                Op::cancel(Token::new(rt)),
            );
            match entry.route {
                Some(route) => self.push_own_sim(route, cancel),
                None => self.push_own(cancel),
            }
        }
    }

    /// Queues an op on the reactor's own behalf. `reap` discards its completion.
    pub fn push_own(&mut self, mut sqe: Sqe) {
        if self.own_token(&mut sqe) {
            self.queued.push(sqe);
        }
    }

    fn push_own_sim(&mut self, route: u16, mut sqe: Sqe) {
        if self.own_token(&mut sqe) {
            self.sim_cancels.push((route, sqe));
        }
    }

    fn own_token(&mut self, sqe: &mut Sqe) -> bool {
        let Some(token) = self.tokens.take_own() else {
            tracing::error!(op = ?sqe.op.kind(), "reactor-own tokens exhausted");
            return false;
        };
        sqe.token = token;
        true
    }

    /// The number of `src`'s ops that `reap` has not returned yet.
    pub fn in_flight(&self, src: SourceId) -> usize {
        self.tokens.live(src)
    }
}

/// Restores a completion's source token and frees the reactor token.
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
    /// Cancels every op and reaps for a few rounds. Then leaks the table of any
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
            if self.tokens.live_real(SourceId(i as u16)) > 0 {
                core::mem::forget(table.clone());
            }
        }
    }
}

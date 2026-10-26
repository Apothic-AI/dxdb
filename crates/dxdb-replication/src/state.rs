//! The replication state: queues, event channels, and lifecycle flags.
//!
//! Ports the state half of `src/replication-protocol/index.ts:70-131`
//! (`replicateRxStorageInstance`'s construction), the lifecycle functions at
//! `:133-181`, and the collection layer's start/pause/cancel semantics from
//! `src/plugins/replication/index.ts`.
//!
//! # Why the queues are not locks
//!
//! Upstream serialises each pipeline with a *promise chain*:
//! `state.streamQueue.down = state.streamQueue.down.then(...)`. That gives three
//! properties a mutex would not:
//!
//! 1. Work runs in call order, not in acquire order.
//! 2. "In sync" is a statement about the queue: nothing queued, and it did not grow
//!    while we looked. That is `awaitRxStorageReplicationIdle` (`index.ts:159-181`).
//! 3. Enqueueing never blocks.
//!
//! [`TaskQueue`] reproduces this as a FIFO of pending futures drained in order. There
//! is no lock and no priority, and a task that returns `Pending` is parked at the back
//! rather than blocking its successors — which is what a promise chain does when a
//! `.then` callback awaits.
//!
//! # D3: work that is started but not awaited
//!
//! Upstream fires and forgets in two places — `persistFromMaster` inside the resync
//! loop and `setCheckpoint` at the end of a drain — and relies on the event loop.
//! Rust has no event loop, so those go through the injected
//! [`Spawner`](dxdb_storage::Spawner). Both live *inside* queue tasks here, so the
//! sequential default is not merely correct but the only path exercised: a task that
//! spawns work and keeps going cannot outrun the drain, because the drain owns the
//! poll loop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Wake, Waker};

use dxdb_storage::{
    Clock, DocumentData, DynRxStorageInstance, MonotonicClock, RxStorageInstance, Spawner,
    StorageError,
};

use crate::checkpoint::{Checkpoint, Direction, HashFunction};
use crate::conflict::ConflictHandler;

/// A boxed future, the unit a queue holds.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// How many documents one pull or push request may carry.
///
/// Upstream's default is 100 at the collection layer
/// (`plugins/replication/index.ts:462-463`) and required at the protocol layer, so a
/// caller sets it explicitly; [`Default`] carries the collection default so a caller
/// who wants upstream's behaviour gets it without hard-coding the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchSize(pub usize);

impl BatchSize {
    /// A batch size of `n`.
    pub fn new(n: usize) -> Self {
        BatchSize(n)
    }
}

impl Default for BatchSize {
    /// Upstream's collection-layer default.
    fn default() -> Self {
        BatchSize(100)
    }
}

/// Everything the protocol needs to know about the two instances and the transport.
pub struct ReplicationInput<S: RxStorageInstance> {
    /// A string that uniquely identifies the replication, so checkpoints from two
    /// replications on one collection never mix.
    ///
    /// The collection layer prefixes it: `'rxdbreplication' + replicationIdentifier`
    /// (`plugins/replication/index.ts:72`). That prefix is part of the checkpoint key,
    /// and therefore part of every persisted meta row's revision.
    pub identifier: String,
    /// Documents per pull request.
    pub pull_batch_size: BatchSize,
    /// Documents per push request.
    pub push_batch_size: BatchSize,
    /// The transport.
    pub handler: Box<dyn ReplicationHandler>,
    /// The conflict handler.
    pub conflict_handler: Box<dyn ConflictHandler>,
    /// The local storage: where conflicts are resolved and where revisions chain.
    pub fork: S,
    /// Where assumed-master states and checkpoints live.
    ///
    /// A separate instance on purpose: writing replication metadata must not emit
    /// change events on the fork, or the upstream would re-push its own bookkeeping
    /// (`replication-protocol.d.ts:130-148`).
    pub meta: S,
    /// The database's hash function.
    pub hash_function: Rc<dyn HashFunction>,
    /// When true, the pull side does not store per-document assumed-master states.
    ///
    /// Set for pull-only replications, where nothing is ever pushed and so the assumed
    /// master state is never read. Checkpoints are still written.
    pub skip_storing_pull_meta: bool,
    /// Replicate the master's `_meta` and `_rev` instead of generating local ones.
    ///
    /// Used by migrations, where keeping the master's revisions is what makes an
    /// existing checkpoint still valid.
    pub keep_meta: bool,
    /// A checkpoint to seed a *fresh* replication, per direction. An existing stored
    /// checkpoint always wins.
    pub initial_checkpoint: InitialCheckpoint,
    /// Await this before persisting a fork write, so the caller can defer to idle time.
    ///
    /// Upstream's `waitBeforePersist`; waiting longer risks losing fork writes if the
    /// process dies, so it is opt-in.
    pub wait_before_persist: Option<Rc<dyn Fn() -> BoxFuture<'static, ()>>>,
}

/// Where a fresh replication begins, per direction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InitialCheckpoint {
    /// The push checkpoint to seed when none is stored.
    pub upstream: Option<Checkpoint>,
    /// The pull checkpoint to seed when none is stored.
    pub downstream: Option<Checkpoint>,
}

impl<S: RxStorageInstance> ReplicationInput<S> {
    /// The checkpoint seed for a direction.
    pub fn initial_checkpoint_for(&self, direction: Direction) -> Option<&Checkpoint> {
        match direction {
            Direction::Up => self.initial_checkpoint.upstream.as_ref(),
            Direction::Down => self.initial_checkpoint.downstream.as_ref(),
        }
    }
}

/// The transport: everything the protocol needs from the other side.
///
/// Ports `RxReplicationHandler` (`replication-protocol.d.ts:79-112`). Three members,
/// each with a contract that is easy to violate in a way only the conformance suite
/// catches:
///
/// * [`master_changes_since`](Self::master_changes_since) returns **at most
///   `batch_size`** documents in `(_meta.lwt, primary)` order and a checkpoint that
///   resumes *exactly after the last one returned*. Returning an unchanged checkpoint
///   on a non-empty batch re-delivers documents forever.
/// * [`master_write`](Self::master_write) returns **only conflicts** — the real master
///   state of every row the master refused — and an empty vector otherwise.
/// * [`master_change_stream`](Self::master_change_stream) emits either a live batch or
///   [`PullStreamItem::Resync`]; `Resync` is the only way to say "you may have missed
///   events".
pub trait ReplicationHandler {
    /// Pull documents changed after `checkpoint`.
    fn master_changes_since(
        &self,
        checkpoint: Option<&Checkpoint>,
        batch_size: usize,
    ) -> BoxFuture<'_, Result<DocumentsWithCheckpoint, StorageError>>;

    /// Push rows; return the conflicts only.
    fn master_write(
        &self,
        rows: Vec<WriteToMasterRow>,
    ) -> BoxFuture<'_, Result<Vec<DocumentData>, StorageError>>;

    /// The master's change feed.
    fn master_change_stream(&self) -> PullStream;
}

/// `{ documents, checkpoint }`, what a pull returns.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentsWithCheckpoint {
    /// The documents, at their master state.
    pub documents: Vec<DocumentData>,
    /// Where the next pull resumes. Absent when nothing changed.
    pub checkpoint: Option<Checkpoint>,
}

/// `{ assumedMasterState?, newDocumentState }`, one row in a push.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteToMasterRow {
    /// The master state the fork believes it is writing on top of. `None` means "the fork
    /// believes this document is new".
    pub assumed_master_state: Option<DocumentData>,
    /// The state to write.
    pub new_document_state: DocumentData,
    /// Payloads for the attachments in `new_document_state` that the receiver does not have,
    /// keyed by attachment id.
    ///
    /// Upstream embeds these as `Blob`s in `_attachments[id].data`; a Rust document is ordered
    /// JSON and cannot hold bytes, so they travel beside it. A transport puts them on the wire
    /// in its own encoding and repopulates `_attachments[id].data` on the receiving side; the
    /// protocol never inspects them. Empty when the collection has no attachments or nothing
    /// changed.
    pub attachment_payloads: std::collections::BTreeMap<String, Vec<u8>>,
}

impl WriteToMasterRow {
    /// A row with no attachment payloads.
    pub fn new(
        assumed_master_state: Option<DocumentData>,
        new_document_state: DocumentData,
    ) -> Self {
        WriteToMasterRow {
            assumed_master_state,
            new_document_state,
            attachment_payloads: std::collections::BTreeMap::new(),
        }
    }

    /// A row with payloads attached.
    pub fn with_payloads(
        assumed_master_state: Option<DocumentData>,
        new_document_state: DocumentData,
        attachment_payloads: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Self {
        WriteToMasterRow {
            assumed_master_state,
            new_document_state,
            attachment_payloads,
        }
    }

    /// True when this row carries bytes the receiver needs.
    pub fn has_payloads(&self) -> bool {
        !self.attachment_payloads.is_empty()
    }
}

/// What a pull stream emits.
///
/// Ports `RxReplicationPullStreamItem` (`replication-protocol.d.ts:69`): a batch, or
/// the string `'RESYNC'`. Nothing else, and `Resync` carries no data on purpose —
/// everything queued behind it will be re-derived from the checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub enum PullStreamItem {
    /// A live batch of changes.
    Changes(DocumentsWithCheckpoint),
    /// The master may have missed events (the fork went offline, a connection
    /// dropped). Re-run the checkpoint iteration.
    Resync,
}

/// A stream of [`PullStreamItem`]s.
pub type PullStream = Pin<Box<dyn futures::Stream<Item = PullStreamItem>>>;

/// A channel a transport pushes pull items into, and the protocol drains.
///
/// D2 requires `futures::Stream` *plus* a wake channel. The stream alone is not
/// enough: the downstream's task batching asks "what has arrived so far" and merges
/// it, and an `async next()` loop would await one item at a time and change the
/// batching and the `time < last_time_master_changes_requested` drop rule. So the
/// channel is the protocol's interface and [`Self::stream`] is the transport's view.
#[derive(Clone, Default)]
pub struct PullChannel {
    inner: Rc<RefCell<PullChannelState>>,
}

#[derive(Default)]
struct PullChannelState {
    items: VecDeque<PullStreamItem>,
    waker: Option<Waker>,
}

impl PullChannel {
    /// An empty channel.
    pub fn new() -> Self {
        PullChannel::default()
    }

    /// Queues an item and wakes a waiting stream, if any.
    pub fn push(&self, item: PullStreamItem) {
        let waker = {
            let mut state = self.inner.borrow_mut();
            state.items.push_back(item);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Queues a `RESYNC`.
    pub fn resync(&self) {
        self.push(PullStreamItem::Resync);
    }

    /// Takes the next queued item.
    pub fn pop(&self) -> Option<PullStreamItem> {
        self.inner.borrow_mut().items.pop_front()
    }

    /// Takes everything queued so far.
    ///
    /// This is the call the downstream's batching is written against: one drain, one
    /// merged batch.
    pub fn drain(&self) -> Vec<PullStreamItem> {
        self.inner.borrow_mut().items.drain(..).collect()
    }

    /// True when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().items.is_empty()
    }

    /// A stream view, for a transport exposing `master_change_stream`.
    pub fn stream(&self) -> PullStream {
        Box::pin(PullStreamReader {
            channel: self.clone(),
        })
    }
}

struct PullStreamReader {
    channel: PullChannel,
}

impl futures::Stream for PullStreamReader {
    type Item = PullStreamItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.channel.inner.borrow_mut();
        match state.items.pop_front() {
            Some(item) => Poll::Ready(Some(item)),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TaskQueue
// ---------------------------------------------------------------------------

/// A FIFO of pending work, drained in call order.
///
/// The queue is the port of upstream's promise chain. Its contract:
///
/// * [`Self::enqueue`] appends; work never jumps the queue.
/// * [`Self::drain`] polls until nothing is pending or nothing can make progress. A
///   task that returns `Pending` is parked at the back and retried after its
///   successors, which is what awaiting inside a `.then` callback does.
/// * [`Self::generation`] increments on every enqueue, so a caller can detect that the
///   queue grew while it was draining.
#[derive(Clone, Default)]
pub struct TaskQueue {
    inner: Rc<RefCell<TaskQueueState>>,
}

#[derive(Default)]
struct TaskQueueState {
    generation: u64,
    pending: VecDeque<BoxFuture<'static, ()>>,
    /// Set while a drain is polling, so a task that enqueues during its own run cannot
    /// recurse into a second drain.
    draining: bool,
}

impl TaskQueue {
    /// A drained queue.
    pub fn new() -> Self {
        TaskQueue::default()
    }

    /// The number of tasks enqueued so far.
    pub fn generation(&self) -> u64 {
        self.inner.borrow().generation
    }

    /// How many tasks are queued but unfinished.
    pub fn pending(&self) -> usize {
        self.inner.borrow().pending.len()
    }

    /// True when nothing is queued.
    pub fn is_idle(&self) -> bool {
        self.inner.borrow().pending.is_empty()
    }

    /// Appends work.
    pub fn enqueue<F>(&self, work: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let mut state = self.inner.borrow_mut();
        state.generation += 1;
        state.pending.push_back(Box::pin(work));
    }

    /// Runs queued work until the queue is empty, parking futures that are waiting on
    /// an external event at the back.
    ///
    /// Returns the number of tasks completed. A task that never becomes ready stops the
    /// drain rather than spinning, and stays queued for the next one — that is the
    /// "parked, not lost" case the spawner tests upstream also cover.
    ///
    /// # Reentrancy
    ///
    /// A task that calls `drain` re-entrantly is a no-op and returns `0`: the outer
    /// loop is already polling. This matters because `set_checkpoint` is called from
    /// inside a drain and itself enqueues.
    pub fn drain(&self) -> usize {
        {
            let mut state = self.inner.borrow_mut();
            if state.draining {
                return 0;
            }
            state.draining = true;
        }
        let mut completed = 0;
        let mut idle_passes = 0;
        loop {
            let next = {
                let mut state = self.inner.borrow_mut();
                state.pending.pop_front()
            };
            let Some(mut future) = next else {
                break;
            };
            let parked = match poll_once(future.as_mut()) {
                Poll::Ready(()) => {
                    completed += 1;
                    None
                }
                Poll::Pending => Some(future),
            };
            let mut state = self.inner.borrow_mut();
            match parked {
                Some(future) => {
                    state.pending.push_back(future);
                    drop(state);
                    idle_passes += 1;
                    // Every pending task was tried and made no progress: stop, so a
                    // task waiting on a real event does not spin forever.
                    let live = self.inner.borrow().pending.len();
                    if idle_passes > live {
                        break;
                    }
                }
                None => {
                    idle_passes = 0;
                    drop(state);
                }
            }
        }
        {
            let mut state = self.inner.borrow_mut();
            state.draining = false;
        }
        completed
    }

    /// Drains, then reports whether the queue is empty *and* did not grow.
    ///
    /// The identity check upstream performs (`if (down === state.streamQueue.down && up
    /// === state.streamQueue.up)`) becomes a generation comparison: equal generations
    /// and an empty queue mean nothing arrived while we were working.
    pub fn is_settled(&self, generation_before: u64) -> bool {
        self.is_idle() && self.generation() == generation_before
    }
}

impl fmt::Debug for TaskQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.borrow();
        f.debug_struct("TaskQueue")
            .field("generation", &state.generation)
            .field("pending", &state.pending.len())
            .finish()
    }
}

/// Polls a future once with a no-op waker.
///
/// The queue owns its futures and polls them in a loop, so a wake is only a request to
/// re-poll, which the next drain does anyway. A real executor is the platform
/// binding's business (D3).
fn poll_once(future: Pin<&mut (dyn Future<Output = ()> + 'static)>) -> Poll<()> {
    // `Waker` requires `Arc<W: Wake + Send + Sync>`. `NoopWake` is a zero-sized unit
    // struct, so it is trivially `Send + Sync`; the futures themselves are not, and never
    // cross a thread, but the waker type has no such latitude.
    let waker = Waker::from(std::sync::Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    future.poll(&mut context)
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: std::sync::Arc<Self>) {}
    fn wake_by_ref(self: &std::sync::Arc<Self>) {}
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A callback observing the replication.
pub type EventListener = Rc<dyn Fn(&ReplicationEvent)>;

/// The state a caller can observe, matching the upstream event subjects.
///
/// Ports `RxStorageInstanceReplicationState.events`
/// (`replication-protocol.d.ts:176-235`) without RxJS. Upstream's subjects are push
/// streams; a listener callback is what every consumer of them does (the conformance
/// suite, a UI, a test), and it keeps the crate free of an Observable implementation
/// when `dxdb-reactivity` owns that.
#[derive(Clone, Default)]
pub struct ReplicationEvents {
    inner: Rc<RefCell<EventState>>,
}

#[derive(Default)]
struct EventState {
    cancelled: bool,
    paused: bool,
    active_up: bool,
    active_down: bool,
    first_sync_down: bool,
    first_sync_up: bool,
    processed_down: Vec<DocumentData>,
    processed_up: Vec<WriteToMasterRow>,
    resolved_conflicts: Vec<ResolvedConflict>,
    errors: Vec<StorageError>,
    listeners: Vec<EventListener>,
}

/// One emitted event, for a listener.
#[derive(Clone, Debug)]
pub enum ReplicationEvent {
    /// A document written to the fork by the pull.
    ProcessedDown(DocumentData),
    /// A row pushed to the master.
    ProcessedUp(WriteToMasterRow),
    /// A conflict the master reported, with the handler's output.
    ResolvedConflict(ResolvedConflict),
    /// An error that would otherwise be unhandled.
    Error(StorageError),
    /// The pull finished its first sync.
    FirstSyncDown,
    /// The push finished its first sync.
    FirstSyncUp,
    /// The replication was cancelled.
    Cancelled,
}

/// A conflict the master reported and the handler's output.
///
/// Ports `RxReplicationConflict` (`conflict-handling.d.ts:39-42`).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedConflict {
    /// The three states the handler decided between.
    pub input: crate::conflict::ConflictHandlerInput,
    /// What the handler returned.
    pub output: DocumentData,
}

// The `record_*` / `set_*` mutators are the write side of the observables. They have no
// caller yet because the downstream and upstream loops are the next layer; the read side
// and the tests exercise every one of them. `#[allow(dead_code)]` here is honest about
// that rather than deleting the API the protocol is written against.
#[allow(dead_code)]
impl ReplicationEvents {
    /// Fresh state: not cancelled, not paused, both directions active.
    ///
    /// Upstream starts both `active` flags `true` (`index.ts:88-89`) because the
    /// initial resync is already enqueued by the time anyone can observe them.
    pub fn new() -> Self {
        let events = ReplicationEvents::default();
        {
            let mut state = events.inner.borrow_mut();
            state.active_up = true;
            state.active_down = true;
        }
        events
    }

    /// Subscribes. The listener is retained until the replication is dropped.
    pub fn subscribe(&self, listener: EventListener) {
        self.inner.borrow_mut().listeners.push(listener);
    }

    fn emit(&self, event: ReplicationEvent) {
        // Clone the list so a listener that subscribes from inside a callback cannot
        // mutate the vector being iterated, and drop the borrow before calling out.
        let listeners = self.inner.borrow().listeners.clone();
        for listener in listeners {
            listener(&event);
        }
    }

    /// True when the replication was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.borrow().cancelled
    }

    /// True when the replication is paused.
    pub fn is_paused(&self) -> bool {
        self.inner.borrow().paused
    }

    /// Sets the pause flag.
    pub(crate) fn set_paused(&self, paused: bool) {
        self.inner.borrow_mut().paused = paused;
    }

    /// Cancels: terminal, and emits exactly once.
    pub(crate) fn cancel(&self) {
        let first = {
            let mut state = self.inner.borrow_mut();
            if state.cancelled {
                false
            } else {
                state.cancelled = true;
                true
            }
        };
        if first {
            self.emit(ReplicationEvent::Cancelled);
        }
    }

    /// `active.{up,down}`.
    pub fn is_active(&self, direction: Direction) -> bool {
        let state = self.inner.borrow();
        match direction {
            Direction::Up => state.active_up,
            Direction::Down => state.active_down,
        }
    }

    /// Sets a direction's active flag.
    pub(crate) fn set_active(&self, direction: Direction, active: bool) {
        let mut state = self.inner.borrow_mut();
        match direction {
            Direction::Up => state.active_up = active,
            Direction::Down => state.active_down = active,
        }
    }

    /// True once that direction has completed its first sync.
    pub fn first_sync_done(&self, direction: Direction) -> bool {
        let state = self.inner.borrow();
        match direction {
            Direction::Up => state.first_sync_up,
            Direction::Down => state.first_sync_down,
        }
    }

    /// Records that a direction finished its first sync. Emits on the first call only.
    pub(crate) fn set_first_sync_done(&self, direction: Direction) {
        let already = {
            let mut state = self.inner.borrow_mut();
            match direction {
                Direction::Up => std::mem::replace(&mut state.first_sync_up, true),
                Direction::Down => std::mem::replace(&mut state.first_sync_down, true),
            }
        };
        if !already {
            self.emit(match direction {
                Direction::Up => ReplicationEvent::FirstSyncUp,
                Direction::Down => ReplicationEvent::FirstSyncDown,
            });
        }
    }

    /// A document written to the fork.
    pub(crate) fn record_processed_down(&self, document: DocumentData) {
        self.inner
            .borrow_mut()
            .processed_down
            .push(document.clone());
        self.emit(ReplicationEvent::ProcessedDown(document));
    }

    /// A row pushed to the master.
    pub(crate) fn record_processed_up(&self, row: WriteToMasterRow) {
        self.inner.borrow_mut().processed_up.push(row.clone());
        self.emit(ReplicationEvent::ProcessedUp(row));
    }

    /// A resolved conflict.
    pub(crate) fn record_resolved_conflict(&self, conflict: ResolvedConflict) {
        self.inner
            .borrow_mut()
            .resolved_conflicts
            .push(conflict.clone());
        self.emit(ReplicationEvent::ResolvedConflict(conflict));
    }

    /// An unhandled error.
    pub(crate) fn record_error(&self, error: StorageError) {
        self.inner.borrow_mut().errors.push(error.clone());
        self.emit(ReplicationEvent::Error(error));
    }

    /// The documents pulled so far.
    pub fn processed_down(&self) -> Vec<DocumentData> {
        self.inner.borrow().processed_down.clone()
    }

    /// The rows pushed so far.
    pub fn processed_up(&self) -> Vec<WriteToMasterRow> {
        self.inner.borrow().processed_up.clone()
    }

    /// The conflicts resolved so far.
    pub fn resolved_conflicts(&self) -> Vec<ResolvedConflict> {
        self.inner.borrow().resolved_conflicts.clone()
    }

    /// The errors recorded so far.
    pub fn errors(&self) -> Vec<StorageError> {
        self.inner.borrow().errors.clone()
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// The counters upstream exposes for tests and debugging.
///
/// Ports `RxStorageInstanceReplicationState.stats`
/// (`replication-protocol.d.ts:251-274`). Names are kept because the conformance suite
/// reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplicationStats {
    /// Pull counters.
    pub down: DownStats,
    /// Push counters.
    pub up: UpStats,
}

/// Pull counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownStats {
    /// Tasks added to the queue, from the stream or from a resync.
    pub add_new_task: u64,
    /// Full resync passes.
    pub downstream_resync_once: u64,
    /// Batched change-stream drains.
    pub downstream_process_changes: u64,
    /// Items received from the master's change stream.
    pub master_change_stream_emit: u64,
    /// Calls to persist a pulled batch.
    pub persist_from_master: u64,
}

/// Push counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpStats {
    /// Initial sync passes, including re-runs after a conflict.
    pub upstream_initial_sync: u64,
    /// Items received from the fork's change stream.
    pub fork_change_stream_emit: u64,
    /// Task drains.
    pub process_tasks: u64,
    /// Calls to persist a pushed batch.
    pub persist_to_master: u64,
    /// Pushes that reported at least one conflict.
    pub persist_to_master_had_conflicts: u64,
    /// Conflict-resolution writes to the fork.
    pub persist_to_master_conflict_writes: u64,
}

// ---------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------

/// Shared replication state: one handle, many owners.
///
/// Every field is either a cell (an observable flag, a counter, a queue) or an owned
/// capability (the instances, the handler, the clock, the spawner), so a clone is a
/// handle to the same replication rather than a second one. That mirrors upstream,
/// where the state object is passed around by reference and its subjects are shared.
pub struct ReplicationState<S: RxStorageInstance> {
    inner: Rc<ReplicationStateInner<S>>,
}

struct ReplicationStateInner<S: RxStorageInstance> {
    input: ReplicationInput<S>,
    /// Computed once at construction. Upstream stores a promise because
    /// `crypto.subtle` is async; the value never changes, so a `String` is the state.
    checkpoint_key: String,
    /// The `context` a downstream write uses, so the upstream recognises and ignores
    /// its own writes.
    downstream_bulk_write_flag: String,
    /// The fork schema's primary path, cached because every loop reads it.
    primary_path: String,
    /// True when the fork schema declares attachments.
    has_attachments: bool,
    events: ReplicationEvents,
    stats: RefCell<ReplicationStats>,
    queues: Queues,
    last_checkpoint_docs: RefCell<LastCheckpointDocs>,
    /// Swappable so a test can pin time (D4) or opt into concurrency (D3) without
    /// rebuilding state that already has queued work.
    clock: RefCell<Box<dyn Clock>>,
    spawner: RefCell<Rc<dyn Spawner>>,
}

#[derive(Default)]
struct Queues {
    down: TaskQueue,
    up: TaskQueue,
    checkpoint: TaskQueue,
}

#[derive(Default)]
struct LastCheckpointDocs {
    up: Option<DocumentData>,
    down: Option<DocumentData>,
}

impl<S: RxStorageInstance> ReplicationState<S> {
    /// Builds the state. Upstream calls this `replicateRxStorageInstance`.
    ///
    /// Ports `index.ts:70-131`. Every derived value is computed here, once:
    ///
    /// * `primary_path` from the fork schema,
    /// * `has_attachments` from the fork schema's `attachments`,
    /// * the checkpoint key from `identifier || databaseName || collectionName`,
    /// * the downstream bulk-write flag as `"replication-downstream-" + checkpoint key`.
    pub fn new(input: ReplicationInput<S>) -> Self {
        let checkpoint_key = crate::checkpoint::checkpoint_key(
            input.hash_function.as_ref(),
            &input.identifier,
            input.fork.database_name(),
            input.fork.collection_name(),
        );
        let downstream_bulk_write_flag = format!("replication-downstream-{checkpoint_key}");
        let primary_path = input.fork.schema().primary_path.clone();
        let has_attachments = input.fork.schema().has_attachments;
        ReplicationState {
            inner: Rc::new(ReplicationStateInner {
                input,
                checkpoint_key,
                downstream_bulk_write_flag,
                primary_path,
                has_attachments,
                events: ReplicationEvents::new(),
                stats: RefCell::new(ReplicationStats::default()),
                queues: Queues::default(),
                last_checkpoint_docs: RefCell::new(LastCheckpointDocs::default()),
                clock: RefCell::new(Box::new(MonotonicClock::new())),
                spawner: RefCell::new(Rc::new(dxdb_storage::SequentialSpawner::new())),
            }),
        }
    }

    /// Replaces the clock (D4). Existing state and queued work are kept.
    pub fn set_clock(&self, clock: Box<dyn Clock>) {
        *self.inner.clock.borrow_mut() = clock;
    }

    /// Replaces the spawner (D3). Existing state and queued work are kept.
    pub fn set_spawner(&self, spawner: Rc<dyn Spawner>) {
        *self.inner.spawner.borrow_mut() = spawner;
    }

    /// The fork instance.
    pub fn fork(&self) -> &S {
        &self.inner.input.fork
    }

    /// The persistent instance behind the fork, when the fork is a cache tier.
    ///
    /// Upstream's `getUnderlyingPersistentStorage` (`helper.ts:168-178`) walks this
    /// chain so a replication never targets a memory cache. This is the one place the
    /// dyn mirror is unavoidable: the accessor returns `&DynRxStorageInstance<'_>`.
    pub fn underlying_fork(&self) -> Option<&DynRxStorageInstance<'_>> {
        self.inner.input.fork.underlying_persistent_storage()
    }

    /// The meta instance.
    pub fn meta_instance(&self) -> &S {
        &self.inner.input.meta
    }

    /// The transport.
    pub fn handler(&self) -> &dyn ReplicationHandler {
        self.inner.input.handler.as_ref()
    }

    /// The conflict handler.
    pub fn conflict_handler(&self) -> &dyn ConflictHandler {
        self.inner.input.conflict_handler.as_ref()
    }

    /// The checkpoint key: the revision token of every meta row.
    ///
    /// `hash(identifier || databaseName || collectionName)`, prefixed
    /// `rx_storage_replication_` (`checkpoint.ts:145`). It names persisted revisions.
    pub fn checkpoint_key(&self) -> &str {
        &self.inner.checkpoint_key
    }

    /// The raw replication identifier, which is also a `_meta` key on replicated documents.
    ///
    /// Upstream writes `_meta[state.input.identifier] = <next revision height>`
    /// (`downstream.ts:445`), so the key is the identifier string itself, not a hash.
    pub fn identifier_value(&self) -> &str {
        &self.inner.input.identifier
    }

    /// `hash(identifier)` — **not** the checkpoint key.
    ///
    /// The pull tags its own writes with this in `_meta.o.hash` (`downstream.ts:67`), and the
    /// push compares against it. It differs from [`Self::checkpoint_key`], which folds in the
    /// database and collection names; using one where the other belongs silently changes
    /// which writes look self-authored.
    pub fn identifier_hash(&self) -> String {
        self.inner
            .input
            .hash_function
            .hash(&self.inner.input.identifier)
    }

    /// The `context` string a downstream write uses.
    pub fn downstream_bulk_write_flag(&self) -> &str {
        &self.inner.downstream_bulk_write_flag
    }

    /// The fork schema's primary path.
    pub fn primary_path(&self) -> &str {
        &self.inner.primary_path
    }

    /// True when the fork schema declares attachments.
    pub fn has_attachments(&self) -> bool {
        self.inner.has_attachments
    }

    /// Pull batch size.
    pub fn pull_batch_size(&self) -> usize {
        self.inner.input.pull_batch_size.0
    }

    /// Push batch size.
    pub fn push_batch_size(&self) -> usize {
        self.inner.input.push_batch_size.0
    }

    /// True when per-document assumed-master states are not stored.
    pub fn skip_storing_pull_meta(&self) -> bool {
        self.inner.input.skip_storing_pull_meta
    }

    /// True when the master's `_meta` and `_rev` are kept.
    pub fn keep_meta(&self) -> bool {
        self.inner.input.keep_meta
    }

    /// The initial checkpoint for a direction.
    pub fn initial_checkpoint_for(&self, direction: Direction) -> Option<&Checkpoint> {
        self.inner.input.initial_checkpoint_for(direction)
    }

    /// The awaitable an input may supply before persisting a fork write.
    pub fn wait_before_persist(&self) -> Option<Rc<dyn Fn() -> BoxFuture<'static, ()>>> {
        self.inner.input.wait_before_persist.clone()
    }

    /// The events.
    pub fn events(&self) -> &ReplicationEvents {
        &self.inner.events
    }

    /// The counters.
    pub fn stats(&self) -> ReplicationStats {
        self.inner.stats.borrow().clone()
    }

    /// Mutates a pull counter.
    pub fn update_down_stats(&self, f: impl FnOnce(&mut DownStats)) {
        f(&mut self.inner.stats.borrow_mut().down);
    }

    /// Mutates a push counter.
    pub fn update_up_stats(&self, f: impl FnOnce(&mut UpStats)) {
        f(&mut self.inner.stats.borrow_mut().up);
    }

    /// The pull queue.
    pub fn down_queue(&self) -> &TaskQueue {
        &self.inner.queues.down
    }

    /// The push queue.
    pub fn up_queue(&self) -> &TaskQueue {
        &self.inner.queues.up
    }

    /// The checkpoint queue.
    pub fn checkpoint_queue(&self) -> &TaskQueue {
        &self.inner.queues.checkpoint
    }

    /// The current `_meta.lwt` from the injected clock.
    pub fn lwt(&self) -> f64 {
        // The borrow is scoped to the call: `MonotonicClock::now` takes `&self` and
        // returns a value, so no borrow escapes.
        let clock = self.inner.clock.borrow();
        clock.now()
    }

    /// Spawns work the caller does not await (D3).
    pub fn spawn(&self, work: BoxFuture<'static, ()>) {
        self.inner.spawner.borrow().spawn(work);
    }

    /// True when the spawner actually runs work concurrently.
    ///
    /// This is D3's assertion hook: a test that gets `false` has proven it exercised the
    /// sequential default rather than silently getting a parallel one.
    pub fn spawner_is_concurrent(&self) -> bool {
        self.inner.spawner.borrow().spawns_concurrently()
    }

    /// Drives the sequential spawner's queue.
    ///
    /// A parallel spawner manages its own work, so this is a no-op for it — which
    /// `spawns_concurrently()` distinguishes, rather than assuming.
    pub fn drive_spawner(&self) {
        let spawner = self.inner.spawner.borrow().clone();
        if spawner.spawns_concurrently() {
            return;
        }
        // `yield_now` on the sequential spawner drives everything queued and then
        // resolves; polling it once is enough because it is `ready(())` afterwards.
        let future = spawner.yield_now();
        let mut future = future;
        let _ = poll_once(future.as_mut());
    }

    /// True when cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.events.is_cancelled()
    }

    /// True when paused.
    pub fn is_paused(&self) -> bool {
        self.inner.events.is_paused()
    }

    /// True when cancelled or paused: the condition every loop checks.
    pub fn is_stopped_or_paused(&self) -> bool {
        self.is_cancelled() || self.is_paused()
    }

    /// The cached checkpoint row for a direction.
    ///
    /// Upstream reads `state.lastCheckpointDoc[direction]` by reference. A `RefCell`
    /// cannot hand a borrow out past the returning call, and every caller here is a
    /// write path that immediately needs an owned `previous` state (`bulk_write` takes
    /// `previous: DocumentData`), so this clones.
    pub fn last_checkpoint_doc(&self, direction: Direction) -> Option<DocumentData> {
        match direction {
            Direction::Up => self.inner.last_checkpoint_docs.borrow().up.clone(),
            Direction::Down => self.inner.last_checkpoint_docs.borrow().down.clone(),
        }
    }

    /// Replaces the cached checkpoint row.
    pub fn set_last_checkpoint_doc(&self, direction: Direction, doc: Option<DocumentData>) {
        let mut docs = self.inner.last_checkpoint_docs.borrow_mut();
        match direction {
            Direction::Up => docs.up = doc,
            Direction::Down => docs.down = doc,
        }
    }

    /// Cancels: terminal, idempotent, and waits for the checkpoint queue.
    ///
    /// Ports `cancelRxStorageReplication` (`index.ts:141-154`): set the flag, complete
    /// the streams, and await the checkpoint queue. Draining that queue is the second
    /// half of the guard in `set_checkpoint`: without it, a checkpoint could be written
    /// for data that was pulled but never written to the fork.
    pub fn cancel(&self) {
        self.inner.events.cancel();
        self.inner.queues.checkpoint.drain();
    }

    /// Pauses both directions. Not terminal; queued work stays queued.
    ///
    /// Upstream's paused replication still drops *new* change-stream tasks and re-checks
    /// the flag before writing meta after a push (`upstream.ts:435-446`), which is what
    /// stops a pause from marking unsent documents as sent.
    pub fn pause(&self) {
        self.inner.events.set_paused(true);
    }

    /// Resumes a paused replication, returning the `RESYNC` the caller should feed into
    /// both change streams.
    ///
    /// Upstream expresses resume as `start()` on an already-started state, which sets
    /// `paused = false` and calls `reSync()` (`plugins/replication/index.ts:210-225`).
    /// Returning the item rather than emitting it keeps this crate free of a second
    /// channel: the caller owns both the fork stream and the master stream.
    pub fn resume(&self) -> PullStreamItem {
        self.inner.events.set_paused(false);
        PullStreamItem::Resync
    }

    /// True when both directions have completed a first sync.
    pub fn is_first_sync_done(&self) -> bool {
        self.inner.events.first_sync_done(Direction::Up)
            && self.inner.events.first_sync_done(Direction::Down)
    }

    /// Awaits the first sync in both directions, driving the queues while waiting.
    ///
    /// Ports `awaitRxStorageReplicationFirstInSync` (`index.ts:133-146`). The loop drives
    /// the queues rather than sleeping, because this port has no runtime to make progress
    /// on its own — the caller *is* the executor (D3).
    pub fn await_first_in_sync(&self) {
        while !self.is_first_sync_done() {
            if self.is_cancelled() {
                return;
            }
            self.drive_all();
        }
    }

    /// Awaits "queues empty and did not move", the port of
    /// `awaitRxStorageReplicationInSync` + `awaitRxStorageReplicationIdle`
    /// (`index.ts:148-181`).
    ///
    /// The twice-around loop is upstream's: the second pass catches work that arrived
    /// during the first, and a single pass would report in-sync at a moment when a new
    /// event was already queued.
    pub fn await_in_sync(&self) {
        loop {
            let down_before = self.inner.queues.down.generation();
            let up_before = self.inner.queues.up.generation();
            self.drive_all();
            if self.inner.queues.down.is_settled(down_before)
                && self.inner.queues.up.is_settled(up_before)
                && self.inner.queues.checkpoint.is_idle()
            {
                return;
            }
            if self.is_cancelled() {
                return;
            }
            // A second pass: anything queued by the drain above is processed here, and
            // the identity check then really means "nothing moved".
            self.drive_all();
            if self.inner.queues.down.is_settled(down_before)
                && self.inner.queues.up.is_settled(up_before)
                && self.inner.queues.checkpoint.is_idle()
            {
                return;
            }
        }
    }

    /// Drains every queue plus the spawner.
    ///
    /// Public so a caller driving the replication from its own executor can pump it
    /// without awaiting a specific condition.
    pub fn drive_all(&self) {
        // Checkpoints first: a task that just persisted one has already enqueued it, and
        // upstream's ordering is that a checkpoint write is *last* in a drain.
        loop {
            let worked = self.inner.queues.down.drain()
                + self.inner.queues.up.drain()
                + self.inner.queues.checkpoint.drain();
            self.drive_spawner();
            if worked == 0
                && self.inner.queues.down.is_idle()
                && self.inner.queues.up.is_idle()
                && self.inner.queues.checkpoint.is_idle()
            {
                return;
            }
            if self.is_cancelled() {
                return;
            }
        }
    }
}

impl<S: RxStorageInstance> Clone for ReplicationState<S> {
    fn clone(&self) -> Self {
        ReplicationState {
            inner: self.inner.clone(),
        }
    }
}

impl<S: RxStorageInstance> fmt::Debug for ReplicationState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplicationState")
            .field("identifier", &self.inner.input.identifier)
            .field("checkpoint_key", &self.inner.checkpoint_key)
            .field("primary_path", &self.inner.primary_path)
            .field("has_attachments", &self.inner.has_attachments)
            .field("cancelled", &self.is_cancelled())
            .field("paused", &self.is_paused())
            .field("stats", &self.stats())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_size_default_is_upstreams_collection_default() {
        assert_eq!(BatchSize::default().0, 100);
    }

    #[test]
    fn the_pull_channel_drains_in_order_then_empties() {
        let channel = PullChannel::new();
        assert!(channel.is_empty());
        channel.push(PullStreamItem::Changes(DocumentsWithCheckpoint {
            documents: Vec::new(),
            checkpoint: None,
        }));
        channel.resync();
        let items = channel.drain();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], PullStreamItem::Changes(_)));
        assert_eq!(items[1], PullStreamItem::Resync);
        assert!(channel.is_empty(), "drain empties the channel");
    }

    #[test]
    fn a_task_queue_runs_in_call_order() {
        let queue = TaskQueue::new();
        let order = Rc::new(RefCell::new(Vec::new()));
        for index in 0..3u8 {
            let order = order.clone();
            queue.enqueue(async move {
                order.borrow_mut().push(index);
            });
        }
        assert_eq!(queue.generation(), 3);
        assert_eq!(queue.pending(), 3);
        assert_eq!(queue.drain(), 3, "all three completed");
        assert_eq!(*order.borrow(), vec![0, 1, 2], "FIFO, not reversed");
        assert!(queue.is_idle());
    }

    #[test]
    fn a_pending_task_is_parked_not_lost() {
        let queue = TaskQueue::new();
        let progressed = Rc::new(RefCell::new(0u8));
        let counter = progressed.clone();
        // A future that is never ready models work waiting on an external event.
        queue.enqueue(async move {
            std::future::pending::<()>().await;
            *counter.borrow_mut() += 1;
        });
        let ran = Rc::new(RefCell::new(false));
        let flag = ran.clone();
        queue.enqueue(async move {
            *flag.borrow_mut() = true;
        });
        queue.drain();
        assert_eq!(queue.pending(), 1, "the pending task stays queued");
        assert!(*ran.borrow(), "and its successor still ran");
        assert_eq!(*progressed.borrow(), 0);
    }

    #[test]
    fn settled_means_empty_and_not_moved() {
        let queue = TaskQueue::new();
        let before = queue.generation();
        assert!(
            queue.is_settled(before),
            "an empty queue that did not move is settled"
        );
        queue.enqueue(async {});
        assert!(
            !queue.is_settled(before),
            "a queue that grew is not settled even before it drains"
        );
        queue.drain();
        assert!(
            !queue.is_settled(before),
            "drained but the generation still differs from before"
        );
    }

    #[test]
    fn a_reentrant_drain_is_a_no_op() {
        let queue = TaskQueue::new();
        let queue_inner = queue.clone();
        queue.enqueue(async move {
            // A task that enqueues and drains from inside itself must not recurse.
            queue_inner.enqueue(async {});
            assert_eq!(queue_inner.drain(), 0, "the outer drain is already polling");
        });
        queue.drain();
        assert!(
            queue.is_idle(),
            "the inner enqueue still ran in the outer drain"
        );
    }

    #[test]
    fn events_start_active_in_both_directions_like_upstream() {
        let events = ReplicationEvents::new();
        assert!(events.is_active(Direction::Up));
        assert!(events.is_active(Direction::Down));
        assert!(!events.is_cancelled());
        assert!(!events.is_paused());
    }

    #[test]
    fn cancel_is_terminal_and_emits_once() {
        let events = ReplicationEvents::new();
        let seen = Rc::new(RefCell::new(0usize));
        let counter = seen.clone();
        events.subscribe(Rc::new(move |event| {
            if matches!(event, ReplicationEvent::Cancelled) {
                *counter.borrow_mut() += 1;
            }
        }));
        events.cancel();
        events.cancel();
        assert!(events.is_cancelled());
        assert_eq!(*seen.borrow(), 1, "cancelling twice must not emit twice");
    }

    #[test]
    fn pause_is_not_terminal() {
        let events = ReplicationEvents::new();
        events.set_paused(true);
        assert!(events.is_paused());
        assert!(!events.is_cancelled());
        events.set_paused(false);
        assert!(!events.is_paused());
    }

    #[test]
    fn first_sync_done_emits_once_per_direction() {
        let events = ReplicationEvents::new();
        let count = Rc::new(RefCell::new(0usize));
        let counter = count.clone();
        events.subscribe(Rc::new(move |event| {
            if matches!(
                event,
                ReplicationEvent::FirstSyncDown | ReplicationEvent::FirstSyncUp
            ) {
                *counter.borrow_mut() += 1;
            }
        }));
        events.set_first_sync_done(Direction::Down);
        events.set_first_sync_done(Direction::Down);
        events.set_first_sync_done(Direction::Up);
        assert_eq!(
            *count.borrow(),
            2,
            "one emission per direction, not per set"
        );
        assert!(events.first_sync_done(Direction::Down));
        assert!(events.first_sync_done(Direction::Up));
    }

    #[test]
    fn stats_start_at_zero() {
        let stats = ReplicationStats::default();
        assert_eq!(stats.down.add_new_task, 0);
        assert_eq!(stats.up.upstream_initial_sync, 0);
    }
}

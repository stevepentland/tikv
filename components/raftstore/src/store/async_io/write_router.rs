// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
//! The implementation of write router for raftstore.

use std::{
    mem,
    ops::Index,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crossbeam::channel::TrySendError;
use engine_traits::{KvEngine, RaftEngine};
use resource_control::channel::Sender;
use tikv_util::{
    config::{Tracker, VersionTrack},
    error, info, safe_panic,
    time::Instant,
};

use crate::store::{
    async_io::write::WriteMsg, config::Config, fsm::store::PollContext, local_metrics::RaftMetrics,
    metrics::*,
};

const RETRY_SCHEDULE_MILLISECONDS: u64 = 10;

pub trait WriteRouterContext<EK, ER>
where
    EK: KvEngine,
    ER: RaftEngine,
{
    fn write_senders(&self) -> &WriteSenders<EK, ER>;
    fn config(&self) -> &Config;
    fn raft_metrics(&self) -> &RaftMetrics;
}

impl<EK, ER, T> WriteRouterContext<EK, ER> for PollContext<EK, ER, T>
where
    EK: KvEngine,
    ER: RaftEngine,
{
    fn write_senders(&self) -> &WriteSenders<EK, ER> {
        &self.write_senders
    }

    fn config(&self) -> &Config {
        &self.cfg
    }

    fn raft_metrics(&self) -> &RaftMetrics {
        &self.raft_metrics
    }
}

/// A router for each peer that routes write messages to the write worker.
pub struct WriteRouter<EK, ER>
where
    EK: KvEngine,
    ER: RaftEngine,
{
    tag: String,
    writer_id: usize,
    /// Next retry time of rescheduling.
    next_retry_time: Instant,
    /// Next writer id after recheduling done.
    next_writer_id: Option<usize>,
    /// Number of last unpersisted ready since rescheduling if not None.
    last_unpersisted: Option<u64>,
    /// Pending write msgs since rescheduling.
    pending_write_msgs: Vec<WriteMsg<EK, ER>>,
    /// The scheduling priority of the last msg, only valid when priority
    /// scheduling is enabled
    last_msg_priority: Option<u64>,
}

impl<EK, ER> WriteRouter<EK, ER>
where
    EK: KvEngine,
    ER: RaftEngine,
{
    pub fn new(tag: String) -> Self {
        Self {
            tag,
            writer_id: 0,
            next_retry_time: Instant::now_coarse(),
            next_writer_id: None,
            last_unpersisted: None,
            pending_write_msgs: vec![],
            last_msg_priority: None,
        }
    }

    /// Send write msg to write worker or push into inner buffer and wait for
    /// rescheduling.
    pub fn send_write_msg<C: WriteRouterContext<EK, ER>>(
        &mut self,
        ctx: &mut C,
        last_unpersisted: Option<u64>,
        msg: WriteMsg<EK, ER>,
    ) {
        if last_unpersisted.is_none() {
            // reset when there is no pending write
            self.last_msg_priority = None;
        }
        if self.should_send(ctx, last_unpersisted) {
            self.send(ctx, msg);
        } else {
            STORE_IO_RESCHEDULE_PENDING_TASKS_TOTAL_GAUGE.inc();
            self.pending_write_msgs.push(msg);
        }
    }

    /// If there is some msgs need to be rescheduled, check the new persisted
    /// number and sending these msgs to a new write worker if persisted
    /// number is greater than `self.last_unpersisted`.
    pub fn check_new_persisted<C: WriteRouterContext<EK, ER>>(
        &mut self,
        ctx: &mut C,
        persisted_number: u64,
    ) {
        if self.last_unpersisted.is_none_or(|n| n > persisted_number) {
            return;
        }
        // The peer must be destroyed after all previous write tasks have been finished.
        // So do not worry about a destroyed peer being counted in
        // `io_reschedule_concurrent_count`.
        ctx.write_senders()
            .io_reschedule_concurrent_count
            .fetch_sub(1, Ordering::SeqCst);

        STORE_IO_RESCHEDULE_PEER_TOTAL_GAUGE.dec();

        let pre_writer_id = self.writer_id;
        self.writer_id = self.next_writer_id.take().unwrap();
        self.next_retry_time = Instant::now_coarse() + ctx.config().io_reschedule_hotpot_duration.0;
        self.last_unpersisted = None;

        let msgs = mem::take(&mut self.pending_write_msgs);

        info!(
            "finishs io reschedule";
            "tag" => &self.tag,
            "pre_writer_id" => pre_writer_id,
            "writer_id" => self.writer_id,
            "msg_len" => msgs.len()
        );
        STORE_IO_RESCHEDULE_PENDING_TASKS_TOTAL_GAUGE.sub(msgs.len() as i64);

        for m in msgs {
            self.send(ctx, m);
        }
    }

    /// Check if write task can be sent to write worker or pushed into
    /// `self.pending_write_msgs`.
    ///
    /// Returns false if the task should be pushed into
    /// `self.pending_write_msgs`. true means the task should be sent to the
    /// write worker.
    fn should_send<C: WriteRouterContext<EK, ER>>(
        &mut self,
        ctx: &mut C,
        last_unpersisted: Option<u64>,
    ) -> bool {
        // If `last_unpersisted` is not None, the later msg should be pushed into it.
        if self.last_unpersisted.is_some() {
            return false;
        }
        // Local senders may not be updated when `store_io_pool_size()` has been
        // increased by the `ctx.config().update()`, keep the real size until it's
        // updated by `poller.begin()`.
        let async_io_pool_size =
            std::cmp::min(ctx.write_senders().size(), ctx.config().store_io_pool_size);
        if last_unpersisted.is_none() {
            // If no previous pending ready, we can randomly select a new writer worker.
            self.writer_id = rand::random::<usize>() % async_io_pool_size;
            self.next_retry_time =
                Instant::now_coarse() + ctx.config().io_reschedule_hotpot_duration.0;
            self.next_writer_id = None;
            return true;
        }
        if ctx.config().io_reschedule_concurrent_max_count == 0 {
            // No rescheduling
            return true;
        }
        let now = Instant::now_coarse();
        // Whether the time is later than `next_retry_time`.
        if now <= self.next_retry_time {
            return true;
        }
        if self.next_writer_id.is_none() {
            // The hot write peers should not be rescheduled entirely.
            // So it will not be rescheduled if the random id is the same as the original
            // one.
            let new_id = rand::random::<usize>() % async_io_pool_size;
            if new_id == self.writer_id {
                // Reset the time
                self.next_retry_time = now + ctx.config().io_reschedule_hotpot_duration.0;
                return true;
            }
            self.next_writer_id = Some(new_id);
        }
        // This peer should be rescheduled.
        // Try to add 1 to `io_reschedule_concurrent_count`.
        // The `cfg.io_reschedule_concurrent_max_count` is used for controlling the
        // concurrent count of rescheduling peer fsm because rescheduling will
        // introduce performance penalty.
        let success = ctx
            .write_senders()
            .io_reschedule_concurrent_count
            .fetch_update(Ordering::SeqCst, Ordering::Relaxed, |c| {
                if c < ctx.config().io_reschedule_concurrent_max_count {
                    Some(c + 1)
                } else {
                    None
                }
            })
            .is_ok();
        if success {
            STORE_IO_RESCHEDULE_PEER_TOTAL_GAUGE.inc();
            // Rescheduling succeeds. The task should be pushed into
            // `self.pending_write_msgs`.
            self.last_unpersisted = last_unpersisted;
            info!("starts io reschedule"; "tag" => &self.tag);
            false
        } else {
            // Rescheduling fails at this time. Retry 10ms later.
            // The task should be sent to the original write worker.
            self.next_retry_time = now + Duration::from_millis(RETRY_SCHEDULE_MILLISECONDS);
            true
        }
    }

    fn send<C: WriteRouterContext<EK, ER>>(&mut self, ctx: &mut C, msg: WriteMsg<EK, ER>) {
        let sender = &ctx.write_senders()[self.writer_id];
        sender.consume_msg_resource(&msg);
        // pass the priority of last msg as low bound to make sure all messages of one
        // peer are handled sequentially.
        match sender.try_send(msg, self.last_msg_priority) {
            // TODO: handle last msg priority properly
            Ok(priority) => self.last_msg_priority = priority,
            Err(TrySendError::Full(msg)) => {
                let now = Instant::now();
                if sender.send(msg, self.last_msg_priority).is_err() {
                    // Write threads are destroyed after store threads during shutdown.
                    safe_panic!("{} failed to send write msg, err: disconnected", self.tag);
                }
                ctx.raft_metrics()
                    .write_block_wait
                    .observe(now.saturating_elapsed_secs());
            }
            Err(TrySendError::Disconnected(_)) => {
                // Write threads are destroyed after store threads during shutdown.
                safe_panic!("{} failed to send write msg, err: disconnected", self.tag);
            }
        }
    }
}

/// Safefly shared senders among the controller and raftstore threads.
/// Senders in it can only be accessed by cloning method `senders()`.
///
/// `Clone` is safe to race with concurrent `Sender.send()` because the
/// `RefCell` field `last_msg_group` in `Sender` is skipped.
#[derive(Clone)]
pub struct SharedSenders<EK: KvEngine, ER: RaftEngine>(Vec<Sender<WriteMsg<EK, ER>>>);

impl<EK: KvEngine, ER: RaftEngine> Default for SharedSenders<EK, ER> {
    fn default() -> Self {
        Self(vec![])
    }
}

impl<EK: KvEngine, ER: RaftEngine> SharedSenders<EK, ER> {
    #[inline]
    pub fn get(&self) -> Vec<Sender<WriteMsg<EK, ER>>> {
        self.0.clone()
    }

    #[inline]
    pub fn set(&mut self, senders: Vec<Sender<WriteMsg<EK, ER>>>) {
        self.0 = senders;
    }
}

/// All `Sender`s in `SharedSenders` are shared by the global controller
/// thread and raftstore threads. There won't exist concurrent `Sender.send()`
/// calling scenarios among threads on a same `Sender`.
/// On the one hand, th controller thread will not call `Sender.send()` to
/// consume resources to send messages, just updating the size of `Sender`s if
/// `store-io-pool-size` is resized. On the other hand, each raftstore thread
/// just use its local cloned `Sender`s for sending messages and update it at
/// `begin()`, the first stage for processing messages.
/// Therefore, it's safe to manually remain `Send` trait for
/// `SharedSenders`.
///
/// TODO: use an elegant implementation, such as `Mutex<Sender>`, to avoid this
/// hack for sharing `Sender`s among multi-threads.
unsafe impl<EK: KvEngine, ER: RaftEngine> Sync for SharedSenders<EK, ER> {}

/// Senders for asynchronous writes. There can be multiple senders, generally
/// you should use `WriteRouter` to decide which sender to be used.
#[derive(Clone)]
pub struct WriteSenders<EK: KvEngine, ER: RaftEngine> {
    senders: Tracker<SharedSenders<EK, ER>>,
    cached_senders: Vec<Sender<WriteMsg<EK, ER>>>,
    io_reschedule_concurrent_count: Arc<AtomicUsize>,
}

impl<EK: KvEngine, ER: RaftEngine> WriteSenders<EK, ER> {
    pub fn new(senders: Arc<VersionTrack<SharedSenders<EK, ER>>>) -> Self {
        let cached_senders = senders.value().get();
        WriteSenders {
            senders: senders.tracker("async writers' tracker".to_owned()),
            cached_senders,
            io_reschedule_concurrent_count: Arc::default(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cached_senders.is_empty()
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.cached_senders.len()
    }

    #[inline]
    pub fn refresh(&mut self) {
        if let Some(senders) = self.senders.any_new() {
            self.cached_senders = senders.get();
        }
    }
}

impl<EK: KvEngine, ER: RaftEngine> Index<usize> for WriteSenders<EK, ER> {
    type Output = Sender<WriteMsg<EK, ER>>;

    #[inline]
    fn index(&self, index: usize) -> &Sender<WriteMsg<EK, ER>> {
        &self.cached_senders[index]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::thread;

    use engine_test::{kv::KvTestEngine, raft::RaftTestEngine};
    use resource_control::channel::{Receiver, bounded};
    use tikv_util::config::ReadableDuration;

    use super::*;

    pub struct TestContext {
        pub senders: WriteSenders<KvTestEngine, RaftTestEngine>,
        pub config: Config,
        pub raft_metrics: RaftMetrics,
    }

    impl WriteRouterContext<KvTestEngine, RaftTestEngine> for TestContext {
        fn write_senders(&self) -> &WriteSenders<KvTestEngine, RaftTestEngine> {
            &self.senders
        }

        fn config(&self) -> &Config {
            &self.config
        }

        fn raft_metrics(&self) -> &RaftMetrics {
            &self.raft_metrics
        }
    }

    struct TestWriteRouter {
        receivers: Vec<Receiver<WriteMsg<KvTestEngine, RaftTestEngine>>>,
        ctx: TestContext,
    }

    impl TestWriteRouter {
        fn new(config: Config) -> Self {
            let (mut receivers, mut senders) = (vec![], vec![]);
            for _ in 0..config.store_io_pool_size {
                let (tx, rx) = bounded(None, config.store_io_notify_capacity);
                receivers.push(rx);
                senders.push(tx);
            }
            Self {
                receivers,
                ctx: TestContext {
                    senders: WriteSenders::new(Arc::new(VersionTrack::new(SharedSenders(senders)))),
                    config,
                    raft_metrics: RaftMetrics::new(true),
                },
            }
        }

        fn must_same_msg_count(&self, id: usize, mut count: usize) {
            while self.receivers[id].try_recv().is_ok() {
                if count == 0 {
                    panic!("msg count is smaller");
                }
                count -= 1;
            }
            if count != 0 {
                panic!("msg count is larger, {} left", count);
            }
        }

        fn must_same_reschedule_count(&self, count: usize) {
            let cnt = self
                .ctx
                .senders
                .io_reschedule_concurrent_count
                .load(Ordering::Relaxed);
            if cnt != count {
                panic!("reschedule count not same, {} != {}", cnt, count);
            }
        }
    }

    #[test]
    fn test_write_router_no_schedule() {
        let mut config = Config::new();
        config.io_reschedule_concurrent_max_count = 0;
        config.io_reschedule_hotpot_duration = ReadableDuration::millis(1);
        config.store_io_pool_size = 4;
        let mut t = TestWriteRouter::new(config);
        let mut r = WriteRouter::new("1".to_string());
        r.send_write_msg(&mut t.ctx, None, WriteMsg::Shutdown);
        let writer_id = r.writer_id;
        for _ in 1..10 {
            r.send_write_msg(&mut t.ctx, Some(10), WriteMsg::Shutdown);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(writer_id, r.writer_id);
        t.must_same_msg_count(writer_id, 10);
        t.must_same_reschedule_count(0);
    }

    #[test]
    fn test_write_router_schedule() {
        let mut config = Config::new();
        config.io_reschedule_concurrent_max_count = 4;
        config.io_reschedule_hotpot_duration = ReadableDuration::millis(5);
        config.store_io_pool_size = 4;
        let mut t = TestWriteRouter::new(config);
        let mut r = WriteRouter::new("1".to_string());

        let last_time = r.next_retry_time;
        thread::sleep(Duration::from_millis(10));
        // `writer_id` will be chosen randomly due to `last_unpersisted` is None
        r.send_write_msg(&mut t.ctx, None, WriteMsg::Shutdown);
        assert!(r.next_retry_time > last_time);
        assert_eq!(r.next_writer_id, None);
        assert_eq!(r.last_unpersisted, None);
        assert!(r.pending_write_msgs.is_empty());
        t.must_same_msg_count(r.writer_id, 1);
        t.must_same_reschedule_count(0);

        thread::sleep(Duration::from_millis(10));
        // Should reschedule due to `last_unpersisted` is not None.
        // However it's possible that it will not scheduled due to random
        // so using loop here.
        let writer_id = r.writer_id;
        let timer = Instant::now();
        loop {
            r.send_write_msg(&mut t.ctx, Some(10), WriteMsg::Shutdown);
            if let Some(id) = r.next_writer_id {
                assert!(writer_id != id);
                assert_eq!(r.last_unpersisted, Some(10));
                assert_eq!(r.pending_write_msgs.len(), 1);
                t.must_same_msg_count(r.writer_id, 0);
                t.must_same_reschedule_count(1);
                break;
            }

            t.must_same_msg_count(r.writer_id, 1);

            if timer.saturating_elapsed() > Duration::from_secs(5) {
                panic!("not schedule after 5 seconds")
            }
            thread::sleep(Duration::from_millis(10));
        }

        r.send_write_msg(&mut t.ctx, Some(20), WriteMsg::Shutdown);
        assert!(r.next_writer_id.is_some());
        // `last_unpersisted` should not change
        assert_eq!(r.last_unpersisted, Some(10));
        assert_eq!(r.pending_write_msgs.len(), 2);
        t.must_same_msg_count(r.writer_id, 0);
        t.must_same_reschedule_count(1);

        // No effect due to 9 < `last_unpersisted`(10)
        r.check_new_persisted(&mut t.ctx, 9);
        assert!(r.next_writer_id.is_some());
        assert_eq!(r.last_unpersisted, Some(10));
        assert_eq!(r.pending_write_msgs.len(), 2);
        t.must_same_msg_count(r.writer_id, 0);
        t.must_same_reschedule_count(1);

        // Should reschedule and send msg
        r.check_new_persisted(&mut t.ctx, 10);
        assert_eq!(r.next_writer_id, None);
        assert_eq!(r.last_unpersisted, None);
        assert!(r.pending_write_msgs.is_empty());
        t.must_same_msg_count(r.writer_id, 2);
        t.must_same_reschedule_count(0);

        thread::sleep(Duration::from_millis(10));
        t.ctx
            .senders
            .io_reschedule_concurrent_count
            .store(4, Ordering::Relaxed);
        // Should retry reschedule next time because the limitation of concurrent count.
        // However it's possible that it will not scheduled due to random
        // so using loop here.
        let timer = Instant::now();
        loop {
            r.send_write_msg(&mut t.ctx, Some(30), WriteMsg::Shutdown);
            t.must_same_msg_count(r.writer_id, 1);
            if r.next_writer_id.is_some() {
                assert_eq!(r.last_unpersisted, None);
                assert!(r.pending_write_msgs.is_empty());
                t.must_same_reschedule_count(4);
                break;
            }

            if timer.saturating_elapsed() > Duration::from_secs(5) {
                panic!("not retry schedule after 5 seconds")
            }
            thread::sleep(Duration::from_millis(10));
        }

        t.ctx
            .senders
            .io_reschedule_concurrent_count
            .store(3, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(RETRY_SCHEDULE_MILLISECONDS + 2));
        // Should reschedule now
        r.send_write_msg(&mut t.ctx, Some(40), WriteMsg::Shutdown);
        assert!(r.next_writer_id.is_some());
        assert_eq!(r.last_unpersisted, Some(40));
        t.must_same_msg_count(r.writer_id, 0);
        t.must_same_reschedule_count(4);
    }
}

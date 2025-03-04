//! Defines storage layer for finalized blocks.
use anyhow::Context as _;
use std::{collections::VecDeque, fmt, sync::Arc};
use zksync_concurrency::{ctx, error::Wrap as _, sync};
use zksync_consensus_roles::validator;

mod metrics;

/// State of the `BlockStore`: continuous range of blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStoreState {
    /// Stored block with the lowest number.
    /// Currently always same as `genesis.first_block`.
    pub first: validator::BlockNumber,
    /// Stored block with the highest number.
    /// None iff store is empty.
    pub last: Option<validator::CommitQC>,
}

impl BlockStoreState {
    /// Checks whether block with the given number is stored in the `BlockStore`.
    pub fn contains(&self, number: validator::BlockNumber) -> bool {
        let Some(last) = &self.last else { return false };
        self.first <= number && number <= last.header().number
    }

    /// Number of the next block that can be stored in the `BlockStore`.
    /// (i.e. `last` + 1).
    pub fn next(&self) -> validator::BlockNumber {
        match &self.last {
            Some(qc) => qc.header().number.next(),
            None => self.first,
        }
    }
}

/// Storage of a continuous range of L2 blocks.
///
/// Implementations **must** propagate context cancellation using [`StorageError::Canceled`].
#[async_trait::async_trait]
pub trait PersistentBlockStore: fmt::Debug + Send + Sync {
    /// Genesis matching the block store content.
    /// Consensus code calls this method only once.
    async fn genesis(&self, ctx: &ctx::Ctx) -> ctx::Result<validator::Genesis>;

    /// Last block available in storage.
    /// Consensus code calls this method only once and then tracks the
    /// range of available blocks internally.
    async fn last(&self, ctx: &ctx::Ctx) -> ctx::Result<Option<validator::CommitQC>>;

    /// Gets a block by its number.
    /// Returns error if block is missing.
    /// Caller is expected to know the state (by calling `state()`)
    /// and only request the blocks contained in the state.
    async fn block(
        &self,
        ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::Result<validator::FinalBlock>;

    /// Persistently store a block.
    /// Implementations are only required to accept a block directly after the current last block,
    /// so that the stored blocks always constitute a continuous range.
    /// Implementation should return only after the block is stored PERSISTENTLY -
    /// consensus liveness property depends on this behavior.
    async fn store_next_block(
        &self,
        ctx: &ctx::Ctx,
        block: &validator::FinalBlock,
    ) -> ctx::Result<()>;
}

#[derive(Debug)]
struct Inner {
    queued_state: sync::watch::Sender<BlockStoreState>,
    persisted_state: BlockStoreState,
    queue: VecDeque<validator::FinalBlock>,
}

/// A wrapper around a PersistentBlockStore which adds caching blocks in-memory
/// and other useful utilities.
#[derive(Debug)]
pub struct BlockStore {
    inner: sync::watch::Sender<Inner>,
    persistent: Box<dyn PersistentBlockStore>,
    genesis: validator::Genesis,
}

/// Runner of the BlockStore background tasks.
#[must_use]
pub struct BlockStoreRunner(Arc<BlockStore>);

impl BlockStoreRunner {
    /// Runs the background tasks of the BlockStore.
    pub async fn run(self, ctx: &ctx::Ctx) -> anyhow::Result<()> {
        #[vise::register]
        static COLLECTOR: vise::Collector<Option<metrics::BlockStore>> = vise::Collector::new();
        let store_ref = Arc::downgrade(&self.0);
        let _ = COLLECTOR.before_scrape(move || Some(store_ref.upgrade()?.scrape_metrics()));

        let res = async {
            let inner = &mut self.0.inner.subscribe();
            loop {
                let block = sync::wait_for(ctx, inner, |inner| !inner.queue.is_empty())
                    .await?
                    .queue[0]
                    .clone();

                // TODO: monitor errors as well.
                let t = metrics::PERSISTENT_BLOCK_STORE
                    .store_next_block_latency
                    .start();
                self.0.persistent.store_next_block(ctx, &block).await?;
                t.observe();
                tracing::info!(
                    "stored block #{}: {:#?}",
                    block.header().number,
                    block.header().hash()
                );

                self.0.inner.send_modify(|inner| {
                    debug_assert_eq!(inner.persisted_state.next(), block.header().number);
                    inner.persisted_state.last = Some(block.justification.clone());
                    inner.queue.pop_front();
                });
            }
        }
        .await;
        match res {
            Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }
}

impl BlockStore {
    /// Constructs a BlockStore.
    /// BlockStore takes ownership of the passed PersistentBlockStore,
    /// i.e. caller should modify the underlying persistent storage
    /// ONLY through the constructed BlockStore.
    pub async fn new(
        ctx: &ctx::Ctx,
        persistent: Box<dyn PersistentBlockStore>,
    ) -> ctx::Result<(Arc<Self>, BlockStoreRunner)> {
        let t = metrics::PERSISTENT_BLOCK_STORE.genesis_latency.start();
        let genesis = persistent.genesis(ctx).await.wrap("persistent.genesis()")?;
        t.observe();
        let t = metrics::PERSISTENT_BLOCK_STORE.last_latency.start();
        let last = persistent.last(ctx).await.wrap("persistent.last()")?;
        t.observe();
        if let Some(last) = &last {
            last.verify(&genesis).context("last.verify()")?;
        }
        let state = BlockStoreState {
            first: genesis.fork.first_block,
            last,
        };
        let this = Arc::new(Self {
            inner: sync::watch::channel(Inner {
                queued_state: sync::watch::channel(state.clone()).0,
                persisted_state: state,
                queue: VecDeque::new(),
            })
            .0,
            genesis,
            persistent,
        });
        // Verify the first block.
        if let Some(block) = this.block(ctx, this.genesis.fork.first_block).await? {
            block
                .verify(&this.genesis)
                .with_context(|| format!("verify({:?})", this.genesis.fork.first_block))?;
        }
        Ok((this.clone(), BlockStoreRunner(this)))
    }

    /// Genesis specification for this block store.
    pub fn genesis(&self) -> &validator::Genesis {
        &self.genesis
    }

    /// Fetches a block (from queue or persistent storage).
    pub async fn block(
        &self,
        ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::Result<Option<validator::FinalBlock>> {
        {
            let inner = self.inner.borrow();
            if !inner.queued_state.borrow().contains(number) {
                return Ok(None);
            }
            if !inner.persisted_state.contains(number) {
                // Subtraction is safe, because we know that the block
                // is in inner.queue at this point.
                let idx = number.0 - inner.persisted_state.next().0;
                return Ok(inner.queue.get(idx as usize).cloned());
            }
        }
        let t = metrics::PERSISTENT_BLOCK_STORE.block_latency.start();
        let block = self
            .persistent
            .block(ctx, number)
            .await
            .wrap("persistent.block()")?;
        t.observe();
        Ok(Some(block))
    }

    /// Insert block to a queue to be persisted eventually.
    /// Since persisting a block may take a significant amount of time,
    /// BlockStore contains a queue of blocks waiting to be persisted.
    /// `queue_block()` adds a block to the queue as soon as all intermediate
    /// blocks are queued_state as well. Queue is unbounded, so it is caller's
    /// responsibility to manage the queue size.
    pub async fn queue_block(
        &self,
        ctx: &ctx::Ctx,
        block: validator::FinalBlock,
    ) -> ctx::Result<()> {
        let number = block.number();
        {
            let sub = &mut self.subscribe();
            let queued_state =
                sync::wait_for(ctx, sub, |queued_state| queued_state.next() >= number).await?;
            if queued_state.next() > number {
                return Ok(());
            }
            block.verify(&self.genesis).context("block.verify()")?;
            // Verify parent hash, if previous block is available.
            if let Some(last) = queued_state.last.as_ref() {
                if Some(last.header().hash()) != block.header().parent {
                    return Err(anyhow::format_err!(
                        "block.parent = {:?}, want {:?}",
                        block.header().parent,
                        last.header().hash()
                    )
                    .into());
                }
            }
        }
        self.inner.send_if_modified(|inner| {
            let modified = inner.queued_state.send_if_modified(|queued_state| {
                // It may happen that the same block is queued_state by 2 calls.
                if queued_state.next() != number {
                    return false;
                }
                queued_state.last = Some(block.justification.clone());
                true
            });
            if !modified {
                return false;
            }
            inner.queue.push_back(block);
            true
        });
        Ok(())
    }

    /// Waits until the given block is queued to be stored.
    pub async fn wait_until_queued(
        &self,
        ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::OrCanceled<()> {
        sync::wait_for(ctx, &mut self.subscribe(), |queued_state| {
            number < queued_state.next()
        })
        .await?;
        Ok(())
    }

    /// Waits until the given block is stored persistently.
    pub async fn wait_until_persisted(
        &self,
        ctx: &ctx::Ctx,
        number: validator::BlockNumber,
    ) -> ctx::OrCanceled<()> {
        sync::wait_for(ctx, &mut self.inner.subscribe(), |inner| {
            number < inner.persisted_state.next()
        })
        .await?;
        Ok(())
    }

    /// Subscribes to the `BlockStoreState` changes.
    /// Note that this state includes both queue AND stored blocks.
    pub fn subscribe(&self) -> sync::watch::Receiver<BlockStoreState> {
        self.inner.borrow().queued_state.subscribe()
    }

    fn scrape_metrics(&self) -> metrics::BlockStore {
        let m = metrics::BlockStore::default();
        let inner = self.inner.borrow();
        m.next_queued_block
            .set(inner.queued_state.borrow().next().0);
        m.next_persisted_block.set(inner.persisted_state.next().0);
        m
    }
}

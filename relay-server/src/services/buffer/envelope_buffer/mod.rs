use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::time::Instant;

use relay_base_schema::project::ProjectKey;
use relay_config::Config;
use stack_key::StackKey;

use crate::envelope::Envelope;
use crate::services::buffer::envelope_stack::sqlite::SqliteEnvelopeStackError;
use crate::services::buffer::envelope_stack::{EnvelopeStack, StackProvider};
use crate::services::buffer::sqlite_envelope_store::SqliteEnvelopeStoreError;
use crate::services::buffer::stack_provider::memory::MemoryStackProvider;
use crate::services::buffer::stack_provider::sqlite::SqliteStackProvider;
use crate::statsd::{RelayCounters, RelayGauges};

/// Polymorphic envelope buffering interface.
///
/// The underlying buffer can either be disk-based or memory-based,
/// depending on the given configuration.
///
/// NOTE: This is implemented as an enum because a trait object with async methods would not be
/// object safe.
#[derive(Debug)]
#[allow(private_interfaces)]
pub enum PolymorphicEnvelopeBuffer {
    /// An enveloper buffer that uses in-memory envelopes stacks.
    InMemory(EnvelopeBuffer<MemoryStackProvider>),
    /// An enveloper buffer that uses sqlite envelopes stacks.
    #[allow(dead_code)]
    Sqlite(EnvelopeBuffer<SqliteStackProvider>),
}

impl PolymorphicEnvelopeBuffer {
    /// Creates either a memory-based or a disk-based envelope buffer,
    /// depending on the given configuration.
    pub fn from_config(config: &Config) -> Self {
        if config.spool_envelopes_path().is_some() {
            panic!("Disk backend not yet supported for spool V2");
        }
        Self::InMemory(EnvelopeBuffer::<MemoryStackProvider>::new())
    }

    /// Adds an envelope to the buffer.
    pub async fn push(&mut self, envelope: Box<Envelope>) -> Result<(), EnvelopeBufferError> {
        match self {
            Self::Sqlite(buffer) => buffer.push(envelope).await,
            Self::InMemory(buffer) => buffer.push(envelope).await,
        }?;
        relay_statsd::metric!(counter(RelayCounters::BufferEnvelopesWritten) += 1);
        Ok(())
    }

    /// Returns a reference to the next-in-line envelope.
    pub async fn peek(&mut self) -> Result<Option<&Envelope>, EnvelopeBufferError> {
        match self {
            Self::Sqlite(buffer) => buffer.peek().await,
            Self::InMemory(buffer) => buffer.peek().await,
        }
    }

    /// Pops the next-in-line envelope.
    pub async fn pop(&mut self) -> Result<Option<Box<Envelope>>, EnvelopeBufferError> {
        let envelope = match self {
            Self::Sqlite(buffer) => buffer.pop().await,
            Self::InMemory(buffer) => buffer.pop().await,
        }?;
        relay_statsd::metric!(counter(RelayCounters::BufferEnvelopesRead) += 1);
        Ok(envelope)
    }

    /// Marks a project as ready or not ready.
    ///
    /// The buffer reprioritizes its envelopes based on this information.
    pub fn mark_ready(&mut self, project: &ProjectKey, is_ready: bool) -> bool {
        match self {
            Self::Sqlite(buffer) => buffer.mark_ready(project, is_ready),
            Self::InMemory(buffer) => buffer.mark_ready(project, is_ready),
        }
    }
}

/// Error that occurs while interacting with the envelope buffer.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeBufferError {
    #[error("sqlite")]
    Sqlite(#[from] SqliteEnvelopeStackError),

    #[error("impossible")]
    Impossible(#[from] Infallible),
}

/// An envelope buffer that holds an individual stack for each project/sampling project combination.
///
/// Envelope stacks are organized in a priority queue, and are reprioritized every time an envelope
/// is pushed, popped, or when a project becomes ready.
#[derive(Debug)]
struct EnvelopeBuffer<P: StackProvider> {
    /// The central priority queue.
    priority_queue: priority_queue::PriorityQueue<QueueItem<StackKey, P::Stack>, Priority>,
    /// A lookup table to find all stacks involving a project.
    stacks_by_project: hashbrown::HashMap<ProjectKey, BTreeSet<StackKey>>,
    /// A helper to create new stacks.
    ///
    /// This indirection is needed because different stack implementations might need different
    /// initialization (e.g. a database connection).
    stack_provider: P,
}

impl EnvelopeBuffer<MemoryStackProvider> {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Self {
            stacks_by_project: Default::default(),
            priority_queue: Default::default(),
            stack_provider: MemoryStackProvider,
        }
    }
}

#[allow(dead_code)]
impl EnvelopeBuffer<SqliteStackProvider> {
    /// Creates an empty buffer.
    pub async fn new(config: &Config) -> Result<Self, SqliteEnvelopeStoreError> {
        Ok(Self {
            stacks_by_project: Default::default(),
            priority_queue: Default::default(),
            stack_provider: SqliteStackProvider::new(config).await?,
        })
    }
}

impl<P: StackProvider> EnvelopeBuffer<P>
where
    EnvelopeBufferError: std::convert::From<<P::Stack as EnvelopeStack>::Error>,
{
    /// Pushes an envelope to the appropriate envelope stack and reprioritizes the stack.
    ///
    /// If the envelope stack does not exist, a new stack is pushed to the priority queue.
    /// The priority of the stack is updated with the envelope's received_at time.
    pub async fn push(&mut self, envelope: Box<Envelope>) -> Result<(), EnvelopeBufferError> {
        let received_at = envelope.meta().start_time();
        let stack_key = StackKey::from_envelope(&envelope);
        if let Some((
            QueueItem {
                key: _,
                value: stack,
            },
            _,
        )) = self.priority_queue.get_mut(&stack_key)
        {
            stack.push(envelope).await?;
        } else {
            self.push_stack(envelope);
        }
        self.priority_queue.change_priority_by(&stack_key, |prio| {
            prio.received_at = received_at;
        });

        Ok(())
    }

    /// Returns a reference to the next-in-line envelope, if one exists.
    pub async fn peek(&mut self) -> Result<Option<&Envelope>, EnvelopeBufferError> {
        let Some((
            QueueItem {
                key: _,
                value: stack,
            },
            _,
        )) = self.priority_queue.peek_mut()
        else {
            return Ok(None);
        };

        Ok(stack.peek().await?)
    }

    /// Returns the next-in-line envelope, if one exists.
    ///
    /// The priority of the envelope's stack is updated with the next envelope's received_at
    /// time. If the stack is empty after popping, it is removed from the priority queue.
    pub async fn pop(&mut self) -> Result<Option<Box<Envelope>>, EnvelopeBufferError> {
        let Some((QueueItem { key, value: stack }, _)) = self.priority_queue.peek_mut() else {
            return Ok(None);
        };
        let stack_key = *key;
        let envelope = stack.pop().await.unwrap().expect("found an empty stack");

        let next_received_at = stack
            .peek()
            .await?
            .map(|next_envelope| next_envelope.meta().start_time());
        match next_received_at {
            None => {
                self.pop_stack(stack_key);
            }
            Some(next_received_at) => {
                self.priority_queue.change_priority_by(&stack_key, |prio| {
                    prio.received_at = next_received_at;
                });
            }
        }
        Ok(Some(envelope))
    }

    /// Reprioritizes all stacks that involve the given project key by setting it to "ready".
    pub fn mark_ready(&mut self, project: &ProjectKey, is_ready: bool) -> bool {
        let mut changed = false;
        if let Some(stack_keys) = self.stacks_by_project.get(project) {
            for stack_key in stack_keys {
                self.priority_queue.change_priority_by(stack_key, |stack| {
                    let mut found = false;
                    for (subkey, readiness) in [
                        (stack_key.lesser(), &mut stack.readiness.0),
                        (stack_key.greater(), &mut stack.readiness.1),
                    ] {
                        if subkey == project {
                            found = true;
                            if *readiness != is_ready {
                                changed = true;
                                *readiness = is_ready;
                            }
                        }
                    }
                    debug_assert!(found);
                });
            }
        }
        changed
    }

    fn push_stack(&mut self, envelope: Box<Envelope>) {
        let received_at = envelope.meta().start_time();
        let stack_key = StackKey::from_envelope(&envelope);
        let previous_entry = self.priority_queue.push(
            QueueItem {
                key: stack_key,
                value: self.stack_provider.create_stack(envelope),
            },
            Priority::new(received_at),
        );
        debug_assert!(previous_entry.is_none());
        for project_key in stack_key.iter() {
            self.stacks_by_project
                .entry(project_key)
                .or_default()
                .insert(stack_key);
        }
        relay_statsd::metric!(
            gauge(RelayGauges::BufferStackCount) = self.priority_queue.len() as u64
        );
    }

    fn pop_stack(&mut self, stack_key: StackKey) {
        for project_key in stack_key.iter() {
            self.stacks_by_project
                .get_mut(&project_key)
                .expect("project_key is missing from lookup")
                .remove(&stack_key);
        }
        self.priority_queue.remove(&stack_key);
        relay_statsd::metric!(
            gauge(RelayGauges::BufferStackCount) = self.priority_queue.len() as u64
        );
    }
}

mod stack_key {
    use super::*;
    /// Sorted stack key.
    ///
    /// Contains a pair of project keys. The lower key is always the first
    /// element in the pair, such that `(k1, k2)` and `(k2, k1)` map to the same
    /// stack key.
    #[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
    pub struct StackKey(ProjectKey, ProjectKey);

    impl StackKey {
        pub fn from_envelope(envelope: &Envelope) -> Self {
            let own_key = envelope.meta().public_key();
            let sampling_key = envelope.sampling_key().unwrap_or(own_key);
            Self::new(own_key, sampling_key)
        }

        pub fn lesser(&self) -> &ProjectKey {
            &self.0
        }

        pub fn greater(&self) -> &ProjectKey {
            &self.1
        }

        pub fn iter(&self) -> impl Iterator<Item = ProjectKey> {
            std::iter::once(self.0).chain((self.0 != self.1).then_some(self.1))
        }

        fn new(mut key1: ProjectKey, mut key2: ProjectKey) -> Self {
            if key2 < key1 {
                std::mem::swap(&mut key1, &mut key2);
            }
            Self(key1, key2)
        }
    }
}

#[derive(Debug)]
struct QueueItem<K, V> {
    key: K,
    value: V,
}

impl<K, V> std::borrow::Borrow<K> for QueueItem<K, V> {
    fn borrow(&self) -> &K {
        &self.key
    }
}

impl<K: std::hash::Hash, V> std::hash::Hash for QueueItem<K, V> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl<K: PartialEq, V> PartialEq for QueueItem<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: PartialEq, V> Eq for QueueItem<K, V> {}

#[derive(Debug)]
struct Priority {
    readiness: Readiness,
    received_at: Instant,
}

impl Priority {
    fn new(received_at: Instant) -> Self {
        Self {
            readiness: Readiness::new(),
            received_at,
        }
    }
}

impl PartialEq for Priority {
    fn eq(&self, other: &Self) -> bool {
        self.readiness.ready() == other.readiness.ready() && self.received_at == other.received_at
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Priority {}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.readiness.ready(), other.readiness.ready()) {
            (true, true) => self.received_at.cmp(&other.received_at),
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // For non-ready stacks, we invert the priority, such that projects that are not
            // ready and did not receive envelopes recently can be evicted.
            (false, false) => self.received_at.cmp(&other.received_at).reverse(),
        }
    }
}

#[derive(Debug)]
struct Readiness(bool, bool);

impl Readiness {
    fn new() -> Self {
        Self(false, false)
    }

    fn ready(&self) -> bool {
        self.0 && self.1
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use uuid::Uuid;

    use relay_common::Dsn;
    use relay_sampling::DynamicSamplingContext;

    use crate::envelope::{Item, ItemType};
    use crate::extractors::RequestMeta;

    use super::*;

    fn new_envelope(project_key: ProjectKey, sampling_key: Option<ProjectKey>) -> Box<Envelope> {
        let mut envelope = Envelope::from_request(
            None,
            RequestMeta::new(Dsn::from_str(&format!("http://{project_key}@localhost/1")).unwrap()),
        );
        if let Some(sampling_key) = sampling_key {
            envelope.set_dsc(DynamicSamplingContext {
                public_key: sampling_key,
                trace_id: Uuid::new_v4(),
                release: None,
                user: Default::default(),
                replay_id: None,
                environment: None,
                transaction: None,
                sample_rate: None,
                sampled: None,
                other: Default::default(),
            });
            envelope.add_item(Item::new(ItemType::Transaction));
        }
        envelope
    }

    #[tokio::test]
    async fn insert_pop() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new();

        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let project_key3 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();

        assert!(buffer.pop().await.unwrap().is_none());
        assert!(buffer.peek().await.unwrap().is_none());

        buffer.push(new_envelope(project_key1, None)).await.unwrap();
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );

        buffer.push(new_envelope(project_key2, None)).await.unwrap();
        // Both projects are not ready, so project 1 is on top (has the oldest envelopes):
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );

        buffer.push(new_envelope(project_key3, None)).await.unwrap();
        // All projects are not ready, so project 1 is on top (has the oldest envelopes):
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );

        // After marking a project ready, it goes to the top:
        buffer.mark_ready(&project_key3, true);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key3
        );
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key3
        );

        // After popping, project 1 is on top again:
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );

        // Mark project 1 as ready (still on top):
        buffer.mark_ready(&project_key1, true);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );

        // Mark project 2 as ready as well (now on top because most recent):
        buffer.mark_ready(&project_key2, true);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().public_key(),
            project_key2
        );
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key2
        );

        // Pop last element:
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().public_key(),
            project_key1
        );
        assert!(buffer.pop().await.unwrap().is_none());
        assert!(buffer.peek().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn project_internal_order() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new();

        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();

        let envelope1 = new_envelope(project_key, None);
        let instant1 = envelope1.meta().start_time();
        let envelope2 = new_envelope(project_key, None);
        let instant2 = envelope2.meta().start_time();

        assert!(instant2 > instant1);

        buffer.push(envelope1).await.unwrap();
        buffer.push(envelope2).await.unwrap();

        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().start_time(),
            instant2
        );
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().start_time(),
            instant1
        );
        assert!(buffer.pop().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sampling_projects() {
        let mut buffer = EnvelopeBuffer::<MemoryStackProvider>::new();

        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();

        let envelope1 = new_envelope(project_key1, None);
        let instant1 = envelope1.meta().start_time();
        buffer.push(envelope1).await.unwrap();

        let envelope2 = new_envelope(project_key2, None);
        let instant2 = envelope2.meta().start_time();
        buffer.push(envelope2).await.unwrap();

        let envelope3 = new_envelope(project_key1, Some(project_key2));
        let instant3 = envelope3.meta().start_time();
        buffer.push(envelope3).await.unwrap();

        // Nothing is ready, instant1 is on top:
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().start_time(),
            instant1
        );

        // Mark project 2 ready, gets on top:
        buffer.mark_ready(&project_key2, true);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().start_time(),
            instant2
        );

        // Revert
        buffer.mark_ready(&project_key2, false);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().start_time(),
            instant1
        );

        // Project 1 ready:
        buffer.mark_ready(&project_key1, true);
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().start_time(),
            instant1
        );

        // when both projects are ready, event no 3 ends up on top:
        buffer.mark_ready(&project_key2, true);
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().start_time(),
            instant3
        );
        assert_eq!(
            buffer.peek().await.unwrap().unwrap().meta().start_time(),
            instant2
        );

        buffer.mark_ready(&project_key2, false);
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().start_time(),
            instant1
        );
        assert_eq!(
            buffer.pop().await.unwrap().unwrap().meta().start_time(),
            instant2
        );

        assert!(buffer.pop().await.unwrap().is_none());
    }
}

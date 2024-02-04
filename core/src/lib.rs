//! # Measured. A metrics crate.
//!
//! This crate was born out of a desire for better ergonomics dealing with prometheus,
//! with the added extra goal of minimizing small allocations to reduce memory fragmentation.
//!
//! ## Prometheus vs Memory Fragmentation
//!
//! The [`prometheus`](https://docs.rs/prometheus/0.13.3/prometheus/index.html) crate allows you to very quickly
//! start recording metrics for your application and expose a text-based scrape endpoint. However, the implementation
//! can quickly lead to memory fragmentation issues.
//!
//! For example, let's look at `IntCounterVec`. It's an alias for `MetricVec<CounterVecBuilder<AtomicU64>>`. `MetricVec` has the following definition:
//!
//! ```no_compile
//! pub struct MetricVec<T: MetricVecBuilder> {
//!     pub(crate) v: Arc<MetricVecCore<T>>,
//! }
//! pub(crate) struct MetricVecCore<T: MetricVecBuilder> {
//!     pub children: RwLock<HashMap<u64, T::M>>,
//!     // ...
//! }
//! ```
//!
//! And for our int counter, `T::M` here is
//!
//! ```no_compile
//! pub struct GenericCounter<P: Atomic> {
//!     v: Arc<Value<P>>,
//! }
//!
//! pub struct Value<P: Atomic> {
//!     pub val: P,
//!     pub label_pairs: Vec<LabelPair>,
//!     // ...
//! }
//!
//! pub struct LabelPair {
//!     name: ::protobuf::SingularField<::std::string::String>,
//!     value: ::protobuf::SingularField<::std::string::String>,
//!     // ...
//! }
//! ```
//!
//! So, if we have a counter vec with 3 different labels, and a totel of 24 unique label groups, then we will have
//!
//! * 1 allocation for the `MetricVec` `Arc`
//! * 1 allocation for the `MetricVecCore` `HashMap`
//! * 24 allocations for the counter value `Arc`
//! * 24 allocations for the label pairs `Vec`
//! * 144 allocations for the `String`s in the `LabelPair`
//!
//! Totalling **194 small allocations**.
//!
//! There's nothing wrong with small allocations necessarily, but since these are long-lived allocations that are not allocated inside of
//! an arena, it can lead to fragmentation issues where each small alloc can occupy many different allocator pages and prevent them from being freed.
//!
//! Compared to this crate, `measured` **only needs 1 allocation** for the `HashMap`.
//! If you have semi-dynamic string labels (such as REST API path slugs) then that would add 4 allocations for
//! a [`RodeoReader`](lasso::RodeoReader) or 2 allocations for an [`IndexSet`](indexmap::IndexSet) to track them.
//!
//! And while it's bad form to have extremely high-cardinality metrics, this crate can easily handle
//! 100,000 unique label groups with just a few large allocations.
//!
//! ## Comparisons to the `metrics` family of crates
//!
//! The [`metrics`](https://docs.rs/metrics/latest/metrics/) facade crate and
//! [`metrics_exporter_prometheus`](https://docs.rs/metrics-exporter-prometheus/latest/metrics_exporter_prometheus/index.html)
//! implementation add a lot of complexity to exposing metrics. They also still alloc an `Arc<AtomicU64>` per individual counter
//! which does not solve the problem of memory fragmentation.

use std::{
    hash::Hash,
    sync::{atomic::AtomicU64, RwLock},
};

use label::{LabelGroup, LabelGroupSet, LabelVisitor, NoLabels};
use rustc_hash::FxHasher;
use text::{Bucket, Count, MetricName, Sum, TextEncoder};

type FxHashMap<K, V> = hashbrown::HashMap<K, V, BuildFxHasher>;

pub mod label;
pub mod text;

#[derive(Default)]
pub struct CounterState {
    count: AtomicU64,
}

pub type CounterRef<'a> = MetricRef<'a, CounterState>;

impl CounterRef<'_> {
    pub fn inc(self) {
        self.0
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn inc_by(self, x: u64) {
        self.0
            .count
            .fetch_add(x, std::sync::atomic::Ordering::Relaxed);
    }
}

impl MetricType for CounterState {
    type Metadata = ();
}

pub trait MetricEncoder<T>: MetricType {
    fn write_type(name: impl MetricName, enc: &mut T);
    fn collect_into(
        &self,
        metadata: &Self::Metadata,
        labels: impl LabelGroup,
        name: impl MetricName,
        enc: &mut T,
    );
}

pub struct HistogramState<const N: usize> {
    buckets: [AtomicU64; N],
    count: AtomicU64,
    sum: AtomicU64,
}

pub type HistogramRef<'a, const N: usize> = MetricRef<'a, HistogramState<N>>;

impl<const N: usize> Default for HistogramState<N> {
    fn default() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            buckets: [ZERO; N],
            count: ZERO,
            sum: AtomicU64::new(f64::to_bits(0.0)),
        }
    }
}

impl<const N: usize> MetricType for HistogramState<N> {
    type Metadata = Thresholds<N>;
}

pub struct Thresholds<const N: usize> {
    le: [f64; N],
}
impl<const N: usize> Thresholds<N> {
    pub fn exponential_buckets(start: f64, factor: f64) -> Self {
        if start <= 0.0 {
            panic!(
                "exponential_buckets needs a positive start value, \
                 start: {start}",
            );
        }
        if factor <= 1.0 {
            panic!(
                "exponential_buckets needs a factor greater than 1, \
                 factor: {factor}",
            );
        }

        let mut next = start;
        let mut buckets = std::array::from_fn(|_| {
            let x = next;
            next *= factor;
            x
        });
        buckets[N - 1] = f64::INFINITY;

        Thresholds { le: buckets }
    }
}

impl<const N: usize> HistogramRef<'_, N> {
    pub fn observe(self, x: f64) {
        for i in 0..N {
            if x < self.1.le[i] {
                self.0.buckets[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.0
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0
            .sum
            .fetch_update(
                std::sync::atomic::Ordering::Release,
                std::sync::atomic::Ordering::Acquire,
                |y| Some(f64::to_bits(f64::from_bits(y) + x)),
            )
            .expect("we always return Some in fetch_update");
    }
}

pub trait MetricType: Default {
    type Metadata: Sized;
}

pub struct Metric<M: MetricType> {
    metric: M,
    metadata: M::Metadata,
}

impl<M: MetricType> Metric<M> {
    pub fn new_metric(metadata: M::Metadata) -> Self {
        Self {
            metric: M::default(),
            metadata,
        }
    }

    pub fn get_metric(&self) -> MetricRef<'_, M> {
        MetricRef(&self.metric, &self.metadata)
    }
}

pub struct MetricVec<M: MetricType, L: label::LabelGroupSet> {
    metrics: VecInner<L::Unique, M>,
    metadata: M::Metadata,
    label_set: L,
}

enum VecInner<U: Hash + Eq, M: MetricType> {
    Dense(Box<[M]>),
    Sparse(RwLock<FxHashMap<U, M>>),
}

impl<M: MetricType, L: label::LabelGroupSet> MetricVec<M, L> {
    pub fn new_metric_vec(label_set: L, metadata: M::Metadata) -> Self {
        let metrics = match label_set.cardinality() {
            Some(c) => {
                let mut vec = Vec::with_capacity(c);
                vec.resize_with(c, M::default);
                VecInner::Dense(vec.into_boxed_slice())
            }
            None => VecInner::Sparse(RwLock::new(FxHashMap::with_hasher(BuildFxHasher))),
        };

        Self {
            metrics,
            metadata,
            label_set,
        }
    }

    /// Create a new sparse metric vec. Useful if you have a fixed cardinality vec but the cardinality is quite high
    pub const fn new_sparse_metric_vec(label_set: L, metadata: M::Metadata) -> Self {
        Self {
            metrics: VecInner::Sparse(RwLock::new(FxHashMap::with_hasher(BuildFxHasher))),
            metadata,
            label_set,
        }
    }

    pub fn metadata(&self) -> &M::Metadata {
        &self.metadata
    }

    pub fn with_labels(&self, label: L::Group<'_>) -> Option<LabelId<L>> {
        Some(LabelId(self.label_set.encode(label)?))
    }

    pub fn get_metric<R>(
        &self,
        id: LabelId<L>,
        f: impl for<'a> FnOnce(MetricRef<'a, M>) -> R,
    ) -> R {
        let index = id.0;
        match &self.metrics {
            VecInner::Dense(metrics) => {
                let m = &metrics[self.label_set.encode_dense(index).unwrap()];
                f(MetricRef(m, &self.metadata))
            }
            VecInner::Sparse(metrics) => {
                if let Some(m) = metrics.read().unwrap().get(&index) {
                    return f(MetricRef(m, &self.metadata));
                }

                let _ = metrics.write().unwrap().entry(index).or_default();

                let read = metrics.read().unwrap();
                let m = read.get(&index).unwrap();
                f(MetricRef(m, &self.metadata))
            }
        }
    }
}

pub type Histogram<const N: usize> = Metric<HistogramState<N>>;
pub type HistogramVec<L, const N: usize> = MetricVec<HistogramState<N>, L>;
pub type Counter = Metric<CounterState>;
pub type CounterVec<L> = MetricVec<CounterState, L>;
impl<L: label::LabelGroupSet> MetricVec<CounterState, L> {
    pub fn new_counter_vec(label_set: L) -> Self {
        Self::new_metric_vec(label_set, ())
    }
    pub fn new_sparse_counter_vec(label_set: L) -> Self {
        Self::new_sparse_metric_vec(label_set, ())
    }

    pub fn inc(&self, label: L::Group<'_>) {
        self.get_metric(
            self.with_labels(label)
                .expect("label group should be in the set"),
            |x| x.inc(),
        )
    }

    pub fn inc_by(&self, label: L::Group<'_>, y: u64) {
        self.get_metric(
            self.with_labels(label)
                .expect("label group should be in the set"),
            |x| x.inc_by(y),
        )
    }
}

pub struct MetricRef<'a, M: MetricType>(&'a M, &'a M::Metadata);

pub struct LabelId<L: LabelGroupSet>(L::Unique);

// pub trait Collect<Encoder> {

// }

struct HistogramLabelLe {
    le: f64,
}

impl LabelGroup for HistogramLabelLe {
    fn label_names() -> impl IntoIterator<Item = &'static str> {
        std::iter::once("le")
    }

    fn label_values(&self, v: &mut impl LabelVisitor) {
        v.write_float(self.le)
    }
}

impl<const N: usize> MetricEncoder<TextEncoder> for HistogramState<N> {
    fn write_type(name: impl MetricName, enc: &mut TextEncoder) {
        enc.write_type(&name, text::MetricType::Histogram);
    }
    fn collect_into(
        &self,
        metadata: &Thresholds<N>,
        labels: impl LabelGroup,
        name: impl MetricName,
        enc: &mut TextEncoder,
    ) {
        for i in 0..N {
            let le = metadata.le[i];
            let val = &self.buckets[i];
            enc.write_metric(
                &name.by_ref().with_suffix(Bucket),
                labels.by_ref().compose_with(HistogramLabelLe { le }),
                text::MetricValue::Int(val.load(std::sync::atomic::Ordering::Relaxed) as i64),
            );
        }
        enc.write_metric(
            &name.by_ref().with_suffix(Sum),
            labels.by_ref(),
            text::MetricValue::Float(f64::from_bits(
                self.sum.load(std::sync::atomic::Ordering::Relaxed),
            )),
        );
        enc.write_metric(
            &name.by_ref().with_suffix(Count),
            labels,
            text::MetricValue::Int(self.count.load(std::sync::atomic::Ordering::Relaxed) as i64),
        );
    }
}

impl MetricEncoder<TextEncoder> for CounterState {
    fn write_type(name: impl MetricName, enc: &mut TextEncoder) {
        enc.write_type(&name, text::MetricType::Counter);
    }
    fn collect_into(
        &self,
        _m: &(),
        labels: impl LabelGroup,
        name: impl MetricName,
        enc: &mut TextEncoder,
    ) {
        enc.write_metric(
            &name,
            labels,
            text::MetricValue::Int(self.count.load(std::sync::atomic::Ordering::Relaxed) as i64),
        );
    }
}

impl<M: MetricType> Metric<M> {
    pub fn collect_into<T>(&self, name: impl MetricName, enc: &mut T)
    where
        M: MetricEncoder<T>,
    {
        M::write_type(&name, enc);
        self.metric
            .collect_into(&self.metadata, NoLabels, name, enc)
    }
}
impl<M: MetricType, L: LabelGroupSet> MetricVec<M, L> {
    pub fn collect_into<T>(&self, name: impl MetricName, enc: &mut T)
    where
        M: MetricEncoder<T>,
    {
        M::write_type(&name, enc);
        match &self.metrics {
            VecInner::Dense(m) => {
                for (index, value) in m.iter().enumerate() {
                    value.collect_into(
                        &self.metadata,
                        self.label_set.decode_dense(index),
                        &name,
                        enc,
                    )
                }
            }
            VecInner::Sparse(m) => {
                for (key, value) in &*m.read().unwrap() {
                    value.collect_into(&self.metadata, self.label_set.decode(key), &name, enc)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BuildFxHasher;

impl std::hash::BuildHasher for BuildFxHasher {
    type Hasher = FxHasher;

    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::ActorObservabilityConfig;

const HISTOGRAM_BOUNDS_MS: [u64; 12] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000];

#[derive(Default)]
struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum_ms: u64,
    max_ms: u64,
}

impl Histogram {
    fn with_bucket_count() -> Self {
        Self {
            buckets: vec![0; HISTOGRAM_BOUNDS_MS.len() + 1],
            ..Self::default()
        }
    }

    fn observe(&mut self, value_ms: u64) {
        self.count = self.count.saturating_add(1);
        self.sum_ms = self.sum_ms.saturating_add(value_ms);
        self.max_ms = self.max_ms.max(value_ms);

        let mut bucket_idx = HISTOGRAM_BOUNDS_MS.len();
        for (idx, bound) in HISTOGRAM_BOUNDS_MS.iter().enumerate() {
            if value_ms <= *bound {
                bucket_idx = idx;
                break;
            }
        }
        self.buckets[bucket_idx] = self.buckets[bucket_idx].saturating_add(1);
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut named_buckets = HashMap::new();
        for (idx, count) in self.buckets.iter().enumerate() {
            let name = if idx < HISTOGRAM_BOUNDS_MS.len() {
                format!("le_{}ms", HISTOGRAM_BOUNDS_MS[idx])
            } else {
                "gt_5000ms".to_string()
            };
            named_buckets.insert(name, *count);
        }
        HistogramSnapshot {
            count: self.count,
            sum_ms: self.sum_ms,
            max_ms: self.max_ms,
            buckets: named_buckets,
        }
    }
}

#[derive(Default)]
struct MetricsInner {
    counters: HashMap<String, u64>,
    gauges: HashMap<String, i64>,
    histograms: HashMap<String, Histogram>,
}

struct MetricsState {
    enabled: AtomicBool,
    queue_depth_export_interval_secs: AtomicU64,
    slow_actor_warn_ms: AtomicU64,
    inner: Mutex<MetricsInner>,
}

impl Default for MetricsState {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            queue_depth_export_interval_secs: AtomicU64::new(5),
            slow_actor_warn_ms: AtomicU64::new(200),
            inner: Mutex::new(MetricsInner::default()),
        }
    }
}

#[derive(Serialize)]
struct HistogramSnapshot {
    count: u64,
    sum_ms: u64,
    max_ms: u64,
    buckets: HashMap<String, u64>,
}

#[derive(Serialize)]
struct MetricsSnapshot {
    exported_at_ms: i64,
    counters: HashMap<String, u64>,
    gauges: HashMap<String, i64>,
    histograms: HashMap<String, HistogramSnapshot>,
}

pub struct MetricsExporter {
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl MetricsExporter {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

fn state() -> &'static MetricsState {
    static STATE: OnceLock<MetricsState> = OnceLock::new();
    STATE.get_or_init(MetricsState::default)
}

fn enabled() -> bool {
    state().enabled.load(Ordering::Relaxed)
}

pub fn configure(config: &ActorObservabilityConfig) {
    let st = state();
    st.enabled.store(config.metrics_enabled, Ordering::Relaxed);
    st.queue_depth_export_interval_secs.store(
        config.queue_depth_export_interval_secs.max(1),
        Ordering::Relaxed,
    );
    st.slow_actor_warn_ms
        .store(config.slow_actor_warn_ms.max(1), Ordering::Relaxed);

    if config.metrics_enabled {
        let mut inner = st.inner.lock().expect("metrics state poisoned");
        inner.counters.clear();
        inner.gauges.clear();
        inner.histograms.clear();
    }
}

pub fn spawn_exporter() -> Option<MetricsExporter> {
    if !enabled() {
        return None;
    }

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let interval_secs = state()
        .queue_depth_export_interval_secs
        .load(Ordering::Relaxed)
        .max(1);
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => emit_snapshot(),
                _ = &mut shutdown_rx => break,
            }
        }
    });

    Some(MetricsExporter {
        shutdown_tx: Some(shutdown_tx),
        handle,
    })
}

fn emit_snapshot() {
    if !enabled() {
        return;
    }

    let mut counters = HashMap::new();
    let mut gauges = HashMap::new();
    let mut histograms = HashMap::new();
    {
        let inner = state().inner.lock().expect("metrics state poisoned");
        counters.extend(inner.counters.iter().map(|(k, v)| (k.clone(), *v)));
        gauges.extend(inner.gauges.iter().map(|(k, v)| (k.clone(), *v)));
        histograms.extend(
            inner
                .histograms
                .iter()
                .map(|(k, v)| (k.clone(), v.snapshot())),
        );
    }

    let snapshot = MetricsSnapshot {
        exported_at_ms: Utc::now().timestamp_millis(),
        counters,
        gauges,
        histograms,
    };
    match serde_json::to_string(&snapshot) {
        Ok(json) => info!(actor = "observability", metrics = %json, "Actor metrics snapshot"),
        Err(err) => warn!("Failed to serialize actor metrics snapshot: {err}"),
    }
}

pub fn inc_counter(name: &str) {
    add_counter(name, 1);
}

pub fn add_counter(name: &str, delta: u64) {
    if !enabled() {
        return;
    }
    let mut inner = state().inner.lock().expect("metrics state poisoned");
    let entry = inner.counters.entry(name.to_string()).or_insert(0);
    *entry = entry.saturating_add(delta);
}

pub fn set_gauge(name: &str, value: i64) {
    if !enabled() {
        return;
    }
    let mut inner = state().inner.lock().expect("metrics state poisoned");
    inner.gauges.insert(name.to_string(), value.max(0));
}

pub fn add_gauge(name: &str, delta: i64) {
    if !enabled() {
        return;
    }
    let mut inner = state().inner.lock().expect("metrics state poisoned");
    let entry = inner.gauges.entry(name.to_string()).or_insert(0);
    let next = (*entry).saturating_add(delta);
    *entry = next.max(0);
}

pub fn observe_duration(name: &str, duration: Duration) {
    observe_ms(name, duration.as_millis() as u64);
}

pub fn observe_ms(name: &str, value_ms: u64) {
    if !enabled() {
        return;
    }
    let mut inner = state().inner.lock().expect("metrics state poisoned");
    let histogram = inner
        .histograms
        .entry(name.to_string())
        .or_insert_with(Histogram::with_bucket_count);
    histogram.observe(value_ms);
}

pub fn observe_actor_latency(actor: &str, duration: Duration) {
    let latency_ms = duration.as_millis() as u64;
    observe_ms(&format!("latency.actor.{actor}_ms"), latency_ms);
    let slow_threshold = state().slow_actor_warn_ms.load(Ordering::Relaxed).max(1);
    if enabled() && latency_ms >= slow_threshold {
        warn!(
            actor,
            latency_ms,
            slow_threshold_ms = slow_threshold,
            "Slow actor handler latency",
        );
    }
}

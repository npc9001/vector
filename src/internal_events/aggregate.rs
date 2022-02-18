use metrics::counter;
use vector_core::internal_event::InternalEvent;

#[derive(Debug)]
pub(crate) struct AggregateEventRecorded;

impl InternalEvent for AggregateEventRecorded {
    fn emit_metrics(&self) {
        counter!("aggregate_events_recorded_total", 1);
    }
}

#[derive(Debug)]
pub(crate) struct AggregateFlushed;

impl InternalEvent for AggregateFlushed {
    fn emit_metrics(&self) {
        counter!("aggregate_flushes_total", 1);
    }
}

#[derive(Debug)]
pub(crate) struct AggregateUpdateFailed;

impl InternalEvent for AggregateUpdateFailed {
    fn emit_metrics(&self) {
        counter!("aggregate_failed_updates", 1);
    }
}

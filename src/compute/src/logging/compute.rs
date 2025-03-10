// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logging dataflows for events generated by clusterd.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::operators::arrange::Arranged;
use differential_dataflow::trace::TraceReader;
use mz_expr::{permutation_for_arrangement, MirScalarExpr};
use mz_ore::cast::CastFrom;
use mz_repr::{Datum, DatumVec, Diff, GlobalId, Row, Timestamp};
use mz_timely_util::replay::MzReplay;
use timely::communication::Allocate;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::channels::pushers::buffer::Session;
use timely::dataflow::channels::pushers::{Counter, Tee};
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::{Filter, InspectCore};
use timely::dataflow::{Scope, StreamCore};
use timely::logging::WorkerIdentifier;
use timely::scheduling::Scheduler;
use timely::worker::Worker;
use timely::Container;
use tracing::error;
use uuid::Uuid;

use crate::extensions::arrange::MzArrange;
use crate::logging::{ComputeLog, EventQueue, LogVariant, SharedLoggingState};
use crate::typedefs::{KeysValsHandle, RowSpine};

/// Type alias for a logger of compute events.
pub type Logger = timely::logging_core::Logger<ComputeEvent, WorkerIdentifier>;

/// A logged compute event.
#[derive(Debug, Clone, PartialOrd, PartialEq)]
pub enum ComputeEvent {
    /// A dataflow export was created.
    Export {
        /// Identifier of the export.
        id: GlobalId,
        /// Timely worker index of the exporting dataflow.
        dataflow_index: usize,
    },
    /// A dataflow export was dropped.
    ExportDropped {
        /// Identifier of the export.
        id: GlobalId,
    },
    /// Dataflow export depends on a named dataflow import.
    ExportDependency {
        /// Identifier of the export.
        export_id: GlobalId,
        /// Identifier of the import on which the export depends.
        import_id: GlobalId,
    },
    /// Peek command, true for install and false for retire.
    Peek(Peek, bool),
    /// Available frontier information for dataflow exports.
    Frontier {
        id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
    /// Available frontier information for dataflow imports.
    ImportFrontier {
        import_id: GlobalId,
        export_id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
    /// Arrangement heap size update
    ArrangementHeapSize {
        /// Operator index
        operator: usize,
        /// Delta of the heap size in bytes of the arrangement.
        delta_size: isize,
    },
    /// Arrangement heap size update
    ArrangementHeapCapacity {
        /// Operator index
        operator: usize,
        /// Delta of the heap capacity in bytes of the arrangement.
        delta_capacity: isize,
    },
    /// Arrangement heap size update
    ArrangementHeapAllocations {
        /// Operator index
        operator: usize,
        /// Delta of distinct heap allocations backing the arrangement.
        delta_allocations: isize,
    },
    /// Arrangement size operator address
    ArrangementHeapSizeOperator {
        /// Operator index
        operator: usize,
        /// The address of the operator.
        address: Vec<usize>,
    },
    /// Arrangement size operator dropped
    ArrangementHeapSizeOperatorDrop {
        /// Operator index
        operator: usize,
    },
    /// All operators of a dataflow have shut down.
    DataflowShutdown {
        /// Timely worker index of the dataflow.
        dataflow_index: usize,
    },
}

/// A logged peek event.
#[derive(Debug, Clone, PartialOrd, PartialEq)]
pub struct Peek {
    /// The identifier of the view the peek targets.
    id: GlobalId,
    /// The logical timestamp requested.
    time: Timestamp,
    /// The ID of the peek.
    uuid: Uuid,
}

impl Peek {
    /// Create a new peek from its arguments.
    pub fn new(id: GlobalId, time: Timestamp, uuid: Uuid) -> Self {
        Self { id, time, uuid }
    }
}

/// Constructs the logging dataflow for compute logs.
///
/// Params
/// * `worker`: The Timely worker hosting the log analysis dataflow.
/// * `config`: Logging configuration.
/// * `event_queue`: The source to read compute log events from.
///
/// Returns a map from log variant to a tuple of a trace handle and a dataflow drop token.
pub(super) fn construct<A: Allocate + 'static>(
    worker: &mut timely::worker::Worker<A>,
    config: &mz_compute_client::logging::LoggingConfig,
    event_queue: EventQueue<ComputeEvent>,
    shared_state: Rc<RefCell<SharedLoggingState>>,
) -> BTreeMap<LogVariant, (KeysValsHandle, Rc<dyn Any>)> {
    let logging_interval_ms = std::cmp::max(1, config.interval.as_millis());
    let worker_id = worker.index();
    let worker2 = worker.clone();

    worker.dataflow_named("Dataflow: compute logging", move |scope| {
        let (mut logs, token) = Some(event_queue.link).mz_replay(
            scope,
            "compute logs",
            config.interval,
            event_queue.activator,
        );

        // If logging is disabled, we still need to install the indexes, but we can leave them
        // empty. We do so by immediately filtering all logs events.
        if !config.enable_logging {
            logs = logs.filter(|_| false);
        }

        // Build a demux operator that splits the replayed event stream up into the separate
        // logging streams.
        let mut demux = OperatorBuilder::new("Compute Logging Demux".to_string(), scope.clone());
        let mut input = demux.new_input(&logs, Pipeline);
        let (mut export_out, export) = demux.new_output();
        let (mut dependency_out, dependency) = demux.new_output();
        let (mut frontier_out, frontier) = demux.new_output();
        let (mut import_frontier_out, import_frontier) = demux.new_output();
        let (mut frontier_delay_out, frontier_delay) = demux.new_output();
        let (mut peek_out, peek) = demux.new_output();
        let (mut peek_duration_out, peek_duration) = demux.new_output();
        let (mut shutdown_duration_out, shutdown_duration) = demux.new_output();
        let (mut arrangement_heap_size_out, arrangement_heap_size) = demux.new_output();
        let (mut arrangement_heap_capacity_out, arrangement_heap_capacity) = demux.new_output();
        let (mut arrangement_heap_allocations_out, arrangement_heap_allocations) =
            demux.new_output();

        let mut demux_state = DemuxState::new(worker2);
        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            move |_frontiers| {
                let mut export = export_out.activate();
                let mut dependency = dependency_out.activate();
                let mut frontier = frontier_out.activate();
                let mut import_frontier = import_frontier_out.activate();
                let mut frontier_delay = frontier_delay_out.activate();
                let mut peek = peek_out.activate();
                let mut peek_duration = peek_duration_out.activate();
                let mut shutdown_duration = shutdown_duration_out.activate();
                let mut arrangement_heap_size = arrangement_heap_size_out.activate();
                let mut arrangement_heap_capacity = arrangement_heap_capacity_out.activate();
                let mut arrangement_heap_allocations = arrangement_heap_allocations_out.activate();

                input.for_each(|cap, data| {
                    data.swap(&mut demux_buffer);

                    let mut output_sessions = DemuxOutput {
                        export: export.session(&cap),
                        dependency: dependency.session(&cap),
                        frontier: frontier.session(&cap),
                        import_frontier: import_frontier.session(&cap),
                        frontier_delay: frontier_delay.session(&cap),
                        peek: peek.session(&cap),
                        peek_duration: peek_duration.session(&cap),
                        shutdown_duration: shutdown_duration.session(&cap),
                        arrangement_heap_size: arrangement_heap_size.session(&cap),
                        arrangement_heap_capacity: arrangement_heap_capacity.session(&cap),
                        arrangement_heap_allocations: arrangement_heap_allocations.session(&cap),
                    };

                    for (time, logger_id, event) in demux_buffer.drain(..) {
                        // We expect the logging infrastructure to not shuffle events between
                        // workers and this code relies on the assumption that each worker handles
                        // its own events.
                        assert_eq!(logger_id, worker_id);

                        DemuxHandler {
                            state: &mut demux_state,
                            shared_state: &mut shared_state.borrow_mut(),
                            output: &mut output_sessions,
                            logging_interval_ms,
                            time,
                        }
                        .handle(event);
                    }
                });
            }
        });

        // Encode the contents of each logging stream into its expected `Row` format.
        let dataflow_current = export.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::String(&datum.id.to_string()),
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::UInt64(u64::cast_from(datum.dataflow_id)),
            ])
        });
        let dataflow_dependency = dependency.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::String(&datum.export_id.to_string()),
                Datum::String(&datum.import_id.to_string()),
                Datum::UInt64(u64::cast_from(worker_id)),
            ])
        });
        let frontier_current = frontier.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::String(&datum.export_id.to_string()),
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::MzTimestamp(datum.frontier),
            ])
        });
        let import_frontier_current = import_frontier.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::String(&datum.export_id.to_string()),
                Datum::String(&datum.import_id.to_string()),
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::MzTimestamp(datum.frontier),
            ])
        });
        let frontier_delay = frontier_delay.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::String(&datum.export_id.to_string()),
                Datum::String(&datum.import_id.to_string()),
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::UInt64(datum.delay_pow.try_into().expect("pow too big")),
            ])
        });
        let peek_current = peek.as_collection().map(move |datum| {
            Row::pack_slice(&[
                Datum::Uuid(datum.uuid),
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::String(&datum.id.to_string()),
                Datum::MzTimestamp(datum.time),
            ])
        });
        let peek_duration = peek_duration.as_collection().map(move |bucket| {
            Row::pack_slice(&[
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::UInt64(bucket.try_into().expect("bucket too big")),
            ])
        });
        let shutdown_duration = shutdown_duration.as_collection().map(move |bucket| {
            Row::pack_slice(&[
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::UInt64(bucket.try_into().expect("bucket too big")),
            ])
        });

        let arrangement_heap_datum_to_row = move |ArrangementHeapDatum { operator_id }| {
            Row::pack_slice(&[
                Datum::UInt64(operator_id.try_into().expect("operator_id too big")),
                Datum::UInt64(u64::cast_from(worker_id)),
            ])
        };

        let arrangement_heap_size = arrangement_heap_size
            .as_collection()
            .map(arrangement_heap_datum_to_row.clone());

        let arrangement_heap_capacity = arrangement_heap_capacity
            .as_collection()
            .map(arrangement_heap_datum_to_row.clone());

        let arrangement_heap_allocations = arrangement_heap_allocations
            .as_collection()
            .map(arrangement_heap_datum_to_row);

        use ComputeLog::*;
        let logs = [
            (DataflowCurrent, dataflow_current),
            (DataflowDependency, dataflow_dependency),
            (FrontierCurrent, frontier_current),
            (ImportFrontierCurrent, import_frontier_current),
            (FrontierDelay, frontier_delay),
            (PeekCurrent, peek_current),
            (PeekDuration, peek_duration),
            (ShutdownDuration, shutdown_duration),
            (ArrangementHeapSize, arrangement_heap_size),
            (ArrangementHeapCapacity, arrangement_heap_capacity),
            (ArrangementHeapAllocations, arrangement_heap_allocations),
        ];

        // Build the output arrangements.
        let mut traces = BTreeMap::new();
        for (variant, collection) in logs {
            let variant = LogVariant::Compute(variant);
            if config.index_logs.contains_key(&variant) {
                let key = variant.index_by();
                let (_, value) = permutation_for_arrangement(
                    &key.iter()
                        .cloned()
                        .map(MirScalarExpr::Column)
                        .collect::<Vec<_>>(),
                    variant.desc().arity(),
                );
                let trace = collection
                    .map({
                        let mut row_buf = Row::default();
                        let mut datums = DatumVec::new();
                        move |row| {
                            let datums = datums.borrow_with(&row);
                            row_buf.packer().extend(key.iter().map(|k| datums[*k]));
                            let row_key = row_buf.clone();
                            row_buf.packer().extend(value.iter().map(|k| datums[*k]));
                            let row_val = row_buf.clone();
                            (row_key, row_val)
                        }
                    })
                    .mz_arrange::<RowSpine<_, _, _, _>>(&format!("ArrangeByKey {:?}", variant))
                    .trace;
                traces.insert(variant.clone(), (trace, Rc::clone(&token)));
            }
        }

        traces
    })
}

/// State maintained by the demux operator.
struct DemuxState<A: Allocate> {
    /// The worker hosting this operator.
    worker: Worker<A>,
    /// Maps dataflow exports to dataflow IDs.
    export_dataflows: BTreeMap<GlobalId, usize>,
    /// Maps dataflow exports to their imports and frontier delay tracking state.
    export_imports: BTreeMap<GlobalId, BTreeMap<GlobalId, FrontierDelayState>>,
    /// Maps live dataflows to counts of their exports.
    dataflow_export_counts: BTreeMap<usize, u32>,
    /// Maps dropped dataflows to their drop time.
    dataflow_drop_times: BTreeMap<usize, Duration>,
    /// Contains dataflows that have shut down but not yet been dropped.
    shutdown_dataflows: BTreeSet<usize>,
    /// Maps pending peeks to their installation time.
    peek_stash: BTreeMap<Uuid, Duration>,
    /// Arrangement size stash
    arrangement_size: BTreeMap<usize, ArrangementSizeState>,
}

impl<A: Allocate> DemuxState<A> {
    fn new(worker: Worker<A>) -> Self {
        Self {
            worker,
            export_dataflows: Default::default(),
            export_imports: Default::default(),
            dataflow_export_counts: Default::default(),
            dataflow_drop_times: Default::default(),
            shutdown_dataflows: Default::default(),
            peek_stash: Default::default(),
            arrangement_size: Default::default(),
        }
    }
}

/// State for tracking import-export frontier lag.
#[derive(Default)]
struct FrontierDelayState {
    /// A list of input timestamps that have appeared on the input
    /// frontier, but that the output frontier has not yet advanced beyond,
    /// and the time at which we were informed of their availability.
    time_deque: VecDeque<(Timestamp, Duration)>,
    /// A histogram of emitted delays (bucket size to bucket_count).
    delay_map: BTreeMap<u128, i64>,
}

type Update<D> = (D, Timestamp, Diff);
type Pusher<D> = Counter<Timestamp, Update<D>, Tee<Timestamp, Update<D>>>;
type OutputSession<'a, D> = Session<'a, Timestamp, Vec<Update<D>>, Pusher<D>>;

/// Bundled output sessions used by the demux operator.
struct DemuxOutput<'a> {
    export: OutputSession<'a, ExportDatum>,
    dependency: OutputSession<'a, DependencyDatum>,
    frontier: OutputSession<'a, FrontierDatum>,
    import_frontier: OutputSession<'a, ImportFrontierDatum>,
    frontier_delay: OutputSession<'a, FrontierDelayDatum>,
    peek: OutputSession<'a, Peek>,
    peek_duration: OutputSession<'a, u128>,
    shutdown_duration: OutputSession<'a, u128>,
    arrangement_heap_size: OutputSession<'a, ArrangementHeapDatum>,
    arrangement_heap_capacity: OutputSession<'a, ArrangementHeapDatum>,
    arrangement_heap_allocations: OutputSession<'a, ArrangementHeapDatum>,
}

#[derive(Clone)]
struct ExportDatum {
    id: GlobalId,
    dataflow_id: usize,
}

#[derive(Clone)]
struct DependencyDatum {
    export_id: GlobalId,
    import_id: GlobalId,
}

#[derive(Clone)]
struct FrontierDatum {
    export_id: GlobalId,
    frontier: Timestamp,
}

#[derive(Clone)]
struct ImportFrontierDatum {
    export_id: GlobalId,
    import_id: GlobalId,
    frontier: Timestamp,
}

#[derive(Clone)]
struct FrontierDelayDatum {
    export_id: GlobalId,
    import_id: GlobalId,
    delay_pow: u128,
}

#[derive(Clone)]
struct ArrangementHeapDatum {
    operator_id: usize,
}

#[derive(Default)]
struct ArrangementSizeState {
    size: isize,
    capacity: isize,
    count: isize,
}

/// Event handler of the demux operator.
struct DemuxHandler<'a, 'b, A: Allocate + 'static> {
    /// State kept by the demux operator.
    state: &'a mut DemuxState<A>,
    /// State shared across log receivers.
    shared_state: &'a mut SharedLoggingState,
    /// Demux output sessions.
    output: &'a mut DemuxOutput<'b>,
    /// The logging interval specifying the time granularity for the updates.
    logging_interval_ms: u128,
    /// The current event time.
    time: Duration,
}

impl<A: Allocate> DemuxHandler<'_, '_, A> {
    /// Return the timestamp associated with the current event, based on the event time and the
    /// logging interval.
    fn ts(&self) -> Timestamp {
        let time_ms = self.time.as_millis();
        let interval = self.logging_interval_ms;
        let rounded = (time_ms / interval + 1) * interval;
        rounded.try_into().expect("must fit")
    }

    /// Handle the given compute event.
    fn handle(&mut self, event: ComputeEvent) {
        use ComputeEvent::*;

        match event {
            Export { id, dataflow_index } => self.handle_export(id, dataflow_index),
            ExportDropped { id } => self.handle_export_dropped(id),
            ExportDependency {
                export_id,
                import_id,
            } => self.handle_export_dependency(export_id, import_id),
            Peek(peek, true) => self.handle_peek_install(peek),
            Peek(peek, false) => self.handle_peek_retire(peek),
            Frontier { id, time, diff } => self.handle_frontier(id, time, diff),
            ImportFrontier {
                import_id,
                export_id,
                time,
                diff,
            } => self.handle_import_frontier(import_id, export_id, time, diff),
            ArrangementHeapSize {
                operator,
                delta_size: size,
            } => self.handle_arrangement_heap_size(operator, size),
            ArrangementHeapCapacity {
                operator,
                delta_capacity: capacity,
            } => self.handle_arrangement_heap_capacity(operator, capacity),
            ArrangementHeapAllocations {
                operator,
                delta_allocations: allocations,
            } => self.handle_arrangement_heap_allocations(operator, allocations),
            ArrangementHeapSizeOperator { operator, address } => {
                self.handle_arrangement_heap_size_operator(operator, address)
            }
            ArrangementHeapSizeOperatorDrop { operator } => {
                self.handle_arrangement_heap_size_operator_dropped(operator)
            }
            DataflowShutdown { dataflow_index } => self.handle_dataflow_shutdown(dataflow_index),
        }
    }

    fn handle_export(&mut self, id: GlobalId, dataflow_id: usize) {
        let ts = self.ts();
        let datum = ExportDatum { id, dataflow_id };
        self.output.export.give((datum, ts, 1));

        self.state.export_dataflows.insert(id, dataflow_id);
        self.state.export_imports.insert(id, BTreeMap::new());
        *self
            .state
            .dataflow_export_counts
            .entry(dataflow_id)
            .or_default() += 1;
    }

    fn handle_export_dropped(&mut self, id: GlobalId) {
        let ts = self.ts();
        if let Some(dataflow_id) = self.state.export_dataflows.remove(&id) {
            let datum = ExportDatum { id, dataflow_id };
            self.output.export.give((datum, ts, -1));

            match self.state.dataflow_export_counts.get_mut(&dataflow_id) {
                entry @ Some(0) | entry @ None => {
                    error!(
                        export = ?id,
                        dataflow = ?dataflow_id,
                        "invalid dataflow_export_counts entry at time of export drop: {entry:?}",
                    );
                }
                Some(1) => self.handle_dataflow_dropped(dataflow_id),
                Some(count) => *count -= 1,
            }
        } else {
            error!(
                export = ?id,
                "missing export_dataflows entry at time of export drop"
            );
        }

        // Remove dependency and frontier delay logging for this export.
        if let Some(imports) = self.state.export_imports.remove(&id) {
            for (import_id, delay_state) in imports {
                let datum = DependencyDatum {
                    export_id: id,
                    import_id,
                };
                self.output.dependency.give((datum, ts, -1));

                for (delay_pow, count) in delay_state.delay_map {
                    let datum = FrontierDelayDatum {
                        export_id: id,
                        import_id,
                        delay_pow,
                    };
                    self.output.frontier_delay.give((datum, ts, -count));
                }
            }
        } else {
            error!(
                export = ?id,
                "missing export_imports entry at time of export drop"
            );
        }
    }

    fn handle_dataflow_dropped(&mut self, id: usize) {
        self.state.dataflow_export_counts.remove(&id);

        if self.state.shutdown_dataflows.remove(&id) {
            // Dataflow has already shut down before it was dropped.
            self.output.shutdown_duration.give((0, self.ts(), 1));
        } else {
            // Dataflow has not yet shut down.
            let existing = self.state.dataflow_drop_times.insert(id, self.time);
            if existing.is_some() {
                error!(dataflow = ?id, "dataflow already dropped");
            }
        }
    }

    fn handle_dataflow_shutdown(&mut self, id: usize) {
        if let Some(start) = self.state.dataflow_drop_times.remove(&id) {
            // Dataflow has alredy been dropped.
            let elapsed_ns = self.time.saturating_sub(start).as_nanos();
            let elapsed_pow = elapsed_ns.next_power_of_two();
            self.output
                .shutdown_duration
                .give((elapsed_pow, self.ts(), 1));
        } else {
            // Dataflow has not yet been dropped.
            let was_new = self.state.shutdown_dataflows.insert(id);
            if !was_new {
                error!(dataflow = ?id, "dataflow already shutdown");
            }
        }
    }

    fn handle_export_dependency(&mut self, export_id: GlobalId, import_id: GlobalId) {
        let ts = self.ts();
        let datum = DependencyDatum {
            export_id,
            import_id,
        };
        self.output.dependency.give((datum, ts, 1));

        if let Some(imports) = self.state.export_imports.get_mut(&export_id) {
            imports.insert(import_id, Default::default());
        } else {
            error!(
                export = ?export_id, import = ?import_id,
                "tried to create import for export that doesn't exist"
            );
        }
    }

    fn handle_peek_install(&mut self, peek: Peek) {
        let uuid = peek.uuid;
        let ts = self.ts();
        self.output.peek.give((peek, ts, 1));

        let existing = self.state.peek_stash.insert(uuid, self.time);
        if existing.is_some() {
            error!(
                uuid = ?uuid,
                "peek already registered",
            );
        }
    }

    fn handle_peek_retire(&mut self, peek: Peek) {
        let uuid = peek.uuid;
        let ts = self.ts();
        self.output.peek.give((peek, ts, -1));

        if let Some(start) = self.state.peek_stash.remove(&uuid) {
            let elapsed_ns = self.time.saturating_sub(start).as_nanos();
            let elapsed_pow = elapsed_ns.next_power_of_two();
            self.output.peek_duration.give((elapsed_pow, ts, 1));
        } else {
            error!(
                uuid = ?uuid,
                "peek not yet registered",
            );
        }
    }

    fn handle_frontier(&mut self, export_id: GlobalId, frontier: Timestamp, diff: i8) {
        let ts = self.ts();
        let datum = FrontierDatum {
            export_id,
            frontier,
        };
        self.output.frontier.give((datum, ts, diff.into()));

        // Everything below only applies to frontier insertions.
        if diff <= 0 {
            return;
        }

        // Check if we have imports associated to this export and report frontier advancement
        // delays.
        if let Some(import_map) = self.state.export_imports.get_mut(&export_id) {
            for (&import_id, delay_state) in import_map {
                let FrontierDelayState {
                    time_deque,
                    delay_map,
                } = delay_state;
                while let Some(current_front) = time_deque.pop_front() {
                    let (import_frontier, update_time) = current_front;
                    if frontier >= import_frontier {
                        let elapsed_ns = self.time.saturating_sub(update_time).as_nanos();
                        let elapsed_pow = elapsed_ns.next_power_of_two();
                        let datum = FrontierDelayDatum {
                            export_id,
                            import_id,
                            delay_pow: elapsed_pow,
                        };
                        self.output.frontier_delay.give((datum, ts, 1));

                        let delay_count = delay_map.entry(elapsed_pow).or_default();
                        *delay_count += 1;
                    } else {
                        time_deque.push_front(current_front);
                        break;
                    }
                }
            }
        }
    }

    fn handle_import_frontier(
        &mut self,
        import_id: GlobalId,
        export_id: GlobalId,
        frontier: Timestamp,
        diff: i8,
    ) {
        let ts = self.ts();
        let datum = ImportFrontierDatum {
            export_id,
            import_id,
            frontier,
        };
        self.output.import_frontier.give((datum, ts, diff.into()));

        // Everything below only applies to frontier insertions.
        if diff <= 0 {
            return;
        }

        // Note that it is possible that we receive frontier updates for exports no longer present
        // in `export_imports`. This behavior arises because `ImportFrontier` events are generated
        // by a dataflow `inspect_container` operator, which may outlive the corresponding trace or
        // sink recording in the current `ComputeState` until Timely eventually drops it.
        if let Some(import_map) = self.state.export_imports.get_mut(&export_id) {
            if let Some(delay_state) = import_map.get_mut(&import_id) {
                delay_state.time_deque.push_back((frontier, self.time));
            } else {
                error!(
                    export = ?export_id, import = ?import_id,
                    "tried to create update frontier for import that doesn't exist"
                );
            }
        }
    }

    /// Update the allocation size for an arrangement.
    fn handle_arrangement_heap_size(&mut self, operator_id: usize, size: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {return};

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_size
            .give((datum, ts, Diff::cast_from(size)));

        state.size += size;
    }

    /// Update the allocation capacity for an arrangement.
    fn handle_arrangement_heap_capacity(&mut self, operator_id: usize, capacity: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {return};

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_capacity
            .give((datum, ts, Diff::cast_from(capacity)));

        state.capacity += capacity;
    }

    /// Update the allocation count for an arrangement.
    fn handle_arrangement_heap_allocations(&mut self, operator_id: usize, count: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {return};

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_allocations
            .give((datum, ts, Diff::cast_from(count)));

        state.count += count;
    }

    /// Indicate that a new arrangement exists, start maintaining the heap size state.
    fn handle_arrangement_heap_size_operator(&mut self, operator_id: usize, address: Vec<usize>) {
        let activator = self.state.worker.activator_for(&address);
        self.state
            .arrangement_size
            .insert(operator_id, Default::default());
        self.shared_state
            .arrangement_size_activators
            .insert(operator_id, activator);
    }

    /// Indicate that an arrangement has been dropped and we can cleanup the heap size state.
    fn handle_arrangement_heap_size_operator_dropped(&mut self, operator_id: usize) {
        if let Some(state) = self.state.arrangement_size.remove(&operator_id) {
            let ts = self.ts();
            let datum = ArrangementHeapDatum { operator_id };
            self.output.arrangement_heap_size.give((
                datum.clone(),
                ts,
                -Diff::cast_from(state.size),
            ));
            self.output.arrangement_heap_capacity.give((
                datum.clone(),
                ts,
                -Diff::cast_from(state.capacity),
            ));
            self.output.arrangement_heap_allocations.give((
                datum,
                ts,
                -Diff::cast_from(state.count),
            ));
        }
        self.shared_state
            .arrangement_size_activators
            .remove(&operator_id);
    }
}

pub(crate) trait LogImportFrontiers {
    fn log_import_frontiers(
        self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self;
}

impl<G, C> LogImportFrontiers for StreamCore<G, C>
where
    G: Scope<Timestamp = Timestamp>,
    C: Container,
{
    fn log_import_frontiers(
        self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self {
        // Using `RetractImportFrontiers` ensures that retraction events are logged even when the
        // dataflow is dropped before the input advances to the empty frontier.
        let mut retractions = RetractImportFrontiers {
            logger: logger.clone(),
            import_id,
            export_ids: export_ids.clone(),
            time: None,
        };

        self.inspect_container(move |event| {
            let Err(frontier) = event else { return };

            retractions.log();

            let Some(&time) = frontier.get(0) else { return };
            for &export_id in export_ids.iter() {
                logger.log(ComputeEvent::ImportFrontier {
                    import_id,
                    export_id,
                    time,
                    diff: 1,
                });
                retractions.time = Some(time);
            }
        })
    }
}

impl<G, Tr> LogImportFrontiers for Arranged<G, Tr>
where
    G: Scope<Timestamp = Timestamp>,
    Tr: TraceReader + Clone,
{
    fn log_import_frontiers(
        mut self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self {
        self.stream = self
            .stream
            .log_import_frontiers(logger, import_id, export_ids);
        self
    }
}

struct RetractImportFrontiers {
    logger: Logger,
    import_id: GlobalId,
    export_ids: Vec<GlobalId>,
    time: Option<Timestamp>,
}

impl RetractImportFrontiers {
    fn log(&mut self) {
        if let Some(time) = self.time.take() {
            for &export_id in self.export_ids.iter() {
                self.logger.log(ComputeEvent::ImportFrontier {
                    import_id: self.import_id,
                    export_id,
                    time,
                    diff: -1,
                });
            }
        }
    }
}

impl Drop for RetractImportFrontiers {
    fn drop(&mut self) {
        self.log();
    }
}

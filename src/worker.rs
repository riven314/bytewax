//! Definition of a Bytewax worker.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::PyTypeError;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use timely::communication::Allocate;
use timely::dataflow::operators::Broadcast;
use timely::dataflow::operators::Concatenate;
use timely::dataflow::operators::Probe;
use timely::dataflow::ProbeHandle;
use timely::dataflow::Scope;
use timely::dataflow::Stream;
use timely::progress::Timestamp;
use timely::worker::Worker as TimelyWorker;
use tracing::instrument;

use crate::dataflow::Dataflow;
use crate::dataflow::Operator;
use crate::dataflow::StreamId;
use crate::errors::tracked_err;
use crate::errors::PythonException;
use crate::inputs::*;
use crate::operators::*;
use crate::outputs::*;
use crate::pyo3_extensions::TdPyAny;
use crate::recovery::*;

/// Bytewax worker.
///
/// Wraps a [`TimelyWorker`].
struct Worker<'a, A, F>
where
    A: Allocate,
    F: Fn() -> bool,
{
    worker: &'a mut TimelyWorker<A>,
    /// This is a function that should return `true` only when the
    /// dataflow should perform an abrupt shutdown.
    interrupt_callback: F,
    abort: Arc<AtomicBool>,
}

impl<'a, A, F> Worker<'a, A, F>
where
    A: Allocate,
    F: Fn() -> bool,
{
    fn new(worker: &'a mut TimelyWorker<A>, interrupt_callback: F) -> Self {
        Self {
            worker,
            interrupt_callback,
            abort: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run a specific dataflow until it is complete.
    ///
    /// [`ProbeHandle`]s are how we ID a dataflow.
    fn run<T>(&mut self, probe: ProbeHandle<T>)
    where
        T: Timestamp,
    {
        tracing::info!("Timely dataflow start");
        let cooldown = Duration::from_millis(1);
        while !(self.abort.load(atomic::Ordering::Relaxed)
            || (self.interrupt_callback)()
            || probe.done())
        {
            tracing::debug_span!("step").in_scope(|| {
                self.worker.step_or_park(Some(cooldown));
            });
        }
        tracing::info!("Timely dataflow stop");
    }

    /// Terminate all dataflows in this worker.
    ///
    /// We need this because otherwise all of Timely's entry points
    /// (e.g. [`timely::execute::execute_from`]) wait until all work
    /// is complete and we will hang if we are shutting down due to
    /// error.
    fn shutdown(&mut self) {
        for dataflow_id in self.worker.installed_dataflows() {
            self.worker.drop_dataflow(dataflow_id);
        }
    }
}

/// Public, main entry point for a worker thread.
#[instrument(name = "worker_main", skip_all, fields(worker = worker.index()))]
pub(crate) fn worker_main<A>(
    worker: &mut TimelyWorker<A>,
    interrupt_callback: impl Fn() -> bool,
    flow: Dataflow,
    epoch_interval: EpochInterval,
    recovery_config: Option<Py<RecoveryConfig>>,
) -> PyResult<()>
where
    A: Allocate,
{
    let worker_index = worker.index();
    let worker_count = worker.peers();
    let mut worker = Worker::new(worker, interrupt_callback);
    tracing::info!("Worker start");

    let recovery_config = recovery_config
        .map(|rc| Python::with_gil(|py| rc.extract(py)))
        .transpose()?;

    let flow_id = Python::with_gil(|py| flow.flow_id(py).unwrap());

    // TODO: Now, initialize the StateStore object.
    //       We need to decide if we can do a fast resume first.
    //       We CAN'T do a fast resume if:
    //       - The number of workers changed (state_store.worker_count vs current count)
    //       - Any of the workers can't access its own db (corrupted? volume gone?)
    //       If fast resume can be done, read the resume_from epoch from the state_store
    //       and start from there.
    let state = Rc::new(RefCell::new(StateStore::new(
        recovery_config,
        flow_id,
        worker_index,
        worker_count,
    )?));

    // TODO: Only reading the latest epoch from the local db,
    //       but we should also broadcast to all other workers
    //       to get the cluster frontier.
    let ResumeFrom(_ex, resume_epoch) = state.borrow().resume_from();

    let probe = Python::with_gil(|py| {
        build_production_dataflow(
            py,
            worker.worker,
            flow,
            epoch_interval,
            resume_epoch,
            state,
            &worker.abort,
        )
        .reraise("error building production dataflow")
    })?;

    tracing::info_span!("production_dataflow").in_scope(|| {
        worker.run(probe);
    });

    worker.shutdown();
    tracing::info!("Worker stop");
    Ok(())
}

struct StreamCache<S>(HashMap<StreamId, Stream<S, TdPyAny>>)
where
    S: Scope;

impl<S> StreamCache<S>
where
    S: Scope,
{
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn get_upstream(
        &self,
        py: Python,
        step: &Operator,
        port_name: &str,
    ) -> PyResult<&Stream<S, TdPyAny>> {
        let stream_id = step.get_port_stream(py, port_name)?;
        self.0.get(&stream_id).ok_or_else(|| {
            let msg = format!("unknown {stream_id:?}");
            tracked_err::<PyValueError>(&msg)
        })
    }

    fn get_upmultistream(
        &self,
        py: Python,
        step: &Operator,
        port_name: &str,
    ) -> PyResult<Vec<Stream<S, TdPyAny>>> {
        let stream_ids = step.get_multiport_streams(py, port_name)?;
        stream_ids
            .into_iter()
            .map(|stream_id| {
                self.0.get(&stream_id).cloned().ok_or_else(|| {
                    let msg = format!("unknown {stream_id:?}");
                    tracked_err::<PyValueError>(&msg)
                })
            })
            .collect()
    }

    fn insert_downstream(
        &mut self,
        py: Python,
        step: &Operator,
        port_name: &str,
        stream: Stream<S, TdPyAny>,
    ) -> PyResult<()> {
        let stream_id = step.get_port_stream(py, port_name)?;
        if self.0.insert(stream_id.clone(), stream).is_some() {
            let msg = format!("duplicate {stream_id:?}");
            Err(tracked_err::<PyValueError>(&msg))
        } else {
            Ok(())
        }
    }
}

/// Turn a Bytewax dataflow into a Timely dataflow.
fn build_production_dataflow<A>(
    py: Python,
    worker: &mut TimelyWorker<A>,
    flow: Dataflow,
    epoch_interval: EpochInterval,
    resume_epoch: ResumeEpoch,
    state_store: Rc<RefCell<StateStore>>,
    abort: &Arc<AtomicBool>,
) -> PyResult<ProbeHandle<u64>>
where
    A: Allocate,
{
    // Remember! Never build different numbers of Timely operators on
    // different workers! Timely does not like that and you'll see a
    // mysterious `failed to correctly cast channel` panic. You must
    // build asymmetry within each operator.
    worker.dataflow(|scope| {
        let mut probe = ProbeHandle::new();

        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut snaps = Vec::new();

        let recovery_on = state_store.borrow().recovery_on();
        let immediate_snapshot = state_store.borrow().immediate_snapshot();

        // This contains steps we still need to compile. Starts with
        // the top-level steps in the dataflow.
        let mut build_stack = flow.substeps(py)?;
        // Reverse since we want to pop substeps in added order.
        build_stack.reverse();
        let mut streams = StreamCache::new();

        while let Some(step) = build_stack.pop() {
            let step_id = step.step_id(py)?;

            if step.is_core(py)? {
                let name = step.name(py)?;
                match name.as_str() {
                    "_noop" => {
                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `_noop` missing port")?;
                        // No-op op.
                        streams
                            .insert_downstream(py, &step, "down", up.clone())
                            .reraise("core operator `_noop` missing port")?;
                    }
                    "branch" => {
                        let predicate = step.get_arg(py, "predicate")?.extract(py)?;

                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `branch` missing port")?;

                        let (trues, falses) = up.branch(step_id, predicate)?;

                        streams
                            .insert_downstream(py, &step, "trues", trues)
                            .reraise("core operator `branch` missing port")?;
                        streams
                            .insert_downstream(py, &step, "falses", falses)
                            .reraise("core operator `branch` missing port")?;
                    }
                    "flat_map_batch" => {
                        let mapper = step.get_arg(py, "mapper")?.extract(py)?;

                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `flat_map_batch` missing port")?;

                        let down = up.flat_map_batch(py, step_id, mapper)?;

                        streams
                            .insert_downstream(py, &step, "down", down)
                            .reraise("core operator `flat_map_batch` missing port")?;
                    }
                    "input" => {
                        let source = step.get_arg(py, "source")?.extract::<Source>(py)?;

                        if let Ok(source) = source.extract::<FixedPartitionedSource>(py) {
                            let mut state = InputState::new(step_id.clone(), state_store.clone());
                            state.hydrate(&source)?;
                            let (down, snap) = source
                                .partitioned_input(
                                    py,
                                    scope,
                                    step_id,
                                    epoch_interval,
                                    &probe,
                                    abort,
                                    resume_epoch,
                                    state,
                                )
                                .reraise("error building FixedPartitionedSource")?;

                            inputs.push(down.clone());
                            snaps.push(snap);

                            streams
                                .insert_downstream(py, &step, "down", down)
                                .reraise("core operator `input` missing port")?;
                        } else if let Ok(source) = source.extract::<DynamicSource>(py) {
                            let down = source
                                .dynamic_input(
                                    py,
                                    scope,
                                    step_id,
                                    epoch_interval,
                                    &probe,
                                    abort,
                                    resume_epoch,
                                )
                                .reraise("error building DynamicSource")?;

                            inputs.push(down.clone());

                            streams
                                .insert_downstream(py, &step, "down", down)
                                .reraise("core operator `input` missing port")?;
                        } else {
                            let msg = "unknown source type";
                            return Err(tracked_err::<PyTypeError>(msg));
                        }
                    }
                    "inspect_debug" => {
                        let inspector = step.get_arg(py, "inspector")?.extract(py)?;

                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `inspect_debug` missing port")?;

                        let (down, clock) = up.inspect_debug(py, step_id, inspector)?;

                        outputs.push(clock);

                        streams
                            .insert_downstream(py, &step, "down", down)
                            .reraise("core operator `inspect_debug` missing port")?;
                    }
                    "merge" => {
                        let ups = streams
                            .get_upmultistream(py, &step, "ups")
                            .reraise("core operator `merge` missing port")?;

                        let down = scope.merge(py, step_id, ups)?;

                        streams
                            .insert_downstream(py, &step, "down", down)
                            .reraise("core operator `merge` missing port")?;
                    }
                    "output" => {
                        let sink = step.get_arg(py, "sink")?.extract::<Sink>(py)?;

                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `output` missing port")?;

                        if let Ok(sink) = sink.extract::<FixedPartitionedSink>(py) {
                            let mut state = OutputState::new(step_id.clone(), state_store.clone());
                            state.hydrate(&sink)?;
                            let (clock, snap) = up
                                .partitioned_output(py, step_id, sink, state)
                                .reraise("error building FixedPartitionedSink")?;

                            outputs.push(clock.clone());
                            snaps.push(snap);
                        } else if let Ok(sink) = sink.extract::<DynamicSink>(py) {
                            let clock = up
                                .dynamic_output(py, step_id, sink)
                                .reraise("error building DynamicSink")?;

                            outputs.push(clock.clone());
                        } else {
                            let msg = "unknown sink type";
                            return Err(tracked_err::<PyTypeError>(msg));
                        }
                    }
                    "redistribute" => {
                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `redistribute` missing port")?;

                        let down = up.redistribute(step_id);

                        streams
                            .insert_downstream(py, &step, "down", down)
                            .reraise("core operator `redistribute` missing port")?;
                    }
                    "stateful_batch" => {
                        let builder = step.get_arg(py, "builder")?.extract(py)?;

                        let mut state =
                            StatefulBatchState::new(step_id.clone(), state_store.clone());
                        state.hydrate(&builder)?;

                        let up = streams
                            .get_upstream(py, &step, "up")
                            .reraise("core operator `stateful_batch` missing port")?;

                        let (down, snap) =
                            up.stateful_batch(py, step_id, builder, resume_epoch, state)?;

                        snaps.push(snap);

                        streams
                            .insert_downstream(py, &step, "down", down)
                            .reraise("core operator `stateful_batch` missing port")?;
                    }
                    name => {
                        let msg = format!("Unknown core operator {name:?}");
                        return Err(tracked_err::<PyTypeError>(&msg));
                    }
                }
            } else {
                let mut substeps = step.substeps(py)?;
                substeps.reverse();
                build_stack.extend(substeps);
            }
        }

        if inputs.is_empty() {
            let msg = "Dataflow needs to contain at least one input step; \
                add with `bytewax.operators.input`";
            return Err(tracked_err::<PyValueError>(msg));
        }
        if outputs.is_empty() {
            let msg = "Dataflow needs to contain at least one output or inspect step; \
                add with `bytewax.operators.output` or `bytewax.operators.inspect`";
            return Err(tracked_err::<PyValueError>(msg));
        }

        // Attach the probe to the relevant final output.
        if recovery_on {
            let ssc = state_store.borrow();
            scope
                // Concatenate all snapshot streams
                .concatenate(snaps)
                // Compact all of the snapshots of each worker
                // into a temporary, local (to each worker) sqlite
                // file, and emit a stream of paths for the files.
                .compact_snapshots(state_store.clone())
                // Now save each segment from all workers into
                // a durable backup storate.
                .durable_backup(ssc.backup().unwrap(), immediate_snapshot)
                // Now that the snapshot data is safe, we can
                // update the cluster frontier.
                // Broadcast the stream since we want all workers
                // to write the cluster frontier info, even if they
                // have no new snapshot to save.
                .broadcast()
                // Write the frontier into a temp segment
                .frontier_segment(state_store.clone())
                // Upload the segment to the durable backup
                .durable_backup(ssc.backup().unwrap(), immediate_snapshot)
                // And finally save the cluster frontier locally.
                .compact_frontiers(state_store.clone())
                .probe_with(&mut probe);
        } else {
            scope.concatenate(outputs).probe_with(&mut probe);
        }

        Ok(probe)
    })
}

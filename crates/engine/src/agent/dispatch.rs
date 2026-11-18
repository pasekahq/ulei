// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The tool calls of one model step, run segment by segment as [`plan`] splits them; the calls
//! of a parallel segment run on scoped worker threads that exist only while it runs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use ulei_contracts::generated::c4g::ToolCall;

use crate::policy::Chain;
use crate::tools::{Context, Output, Registry, plan};

/// Most worker threads of one parallel segment.
const MAX_TOOL_WORKERS: usize = 8;
/// Default deadline of one parallel segment.
pub const DEFAULT_CONCURRENT_TOOL_TIMEOUT: Duration = Duration::from_secs(420);
/// Stack of a worker thread, reserved only while its segment runs.
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

/// The outputs of `calls` in call order. A parallel segment ends after `timeout` when one is
/// given; its calls without a result by then get the timeout text.
pub(super) fn run_calls(
    chain: &mut Chain,
    tools: &Registry,
    calls: &[ToolCall],
    ctx: Context<'_>,
    timeout: Option<Duration>,
) -> Vec<Output> {
    let mut outputs = Vec::with_capacity(calls.len());
    for segment in plan(calls, ctx.cwd) {
        let calls = &calls[segment.calls];
        if segment.parallel {
            outputs.extend(parallel(chain, tools, calls, ctx, timeout));
        } else {
            outputs.extend(calls.iter().map(|call| chain.run_tool(tools, call, ctx)));
        }
    }
    outputs
}

/// One parallel segment: the pre hooks in call order, the admitted calls on at most
/// [`MAX_TOOL_WORKERS`] threads, the post hooks in call order.
fn parallel(
    chain: &mut Chain,
    tools: &Registry,
    calls: &[ToolCall],
    ctx: Context<'_>,
    timeout: Option<Duration>,
) -> Vec<Output> {
    let denied: Vec<Option<Output>> = calls
        .iter()
        .map(|call| chain.admit(tools, call, ctx.cwd))
        .collect();
    let admitted: Vec<usize> = (0..calls.len()).filter(|&i| denied[i].is_none()).collect();
    let mut results: Vec<Option<Output>> = vec![None; calls.len()];
    let deadline = timeout.map(|limit| (Instant::now() + limit, limit));
    let next = AtomicUsize::new(0);
    let abandoned = AtomicBool::new(false);
    let (send, receive) = mpsc::channel();
    let mut failure = None;
    thread::scope(|scope| {
        for _ in 0..admitted.len().min(MAX_TOOL_WORKERS) {
            let send = send.clone();
            let (admitted, next, abandoned) = (&admitted, &next, &abandoned);
            let worker = move || {
                while !abandoned.load(Ordering::Relaxed) {
                    let Some(&i) = admitted.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let call: &ToolCall = &calls[i];
                    let output = tools.execute(&call.name, &call.input, ctx);
                    if send.send((i, output)).is_err() {
                        break;
                    }
                }
            };
            let spawned = thread::Builder::new()
                .stack_size(WORKER_STACK_BYTES)
                .spawn_scoped(scope, worker);
            if let Err(error) = spawned {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
        drop(send);
        for _ in 0..admitted.len() {
            // `Err(Some(limit))`: the deadline passed; `Err(None)`: every worker has ended.
            let received = match deadline {
                Some((at, limit)) => receive
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                    .map_err(|e| (e == RecvTimeoutError::Timeout).then_some(limit)),
                None => receive.recv().map_err(|_| None),
            };
            match received {
                Ok((i, output)) => results[i] = Some(output),
                Err(Some(limit)) => {
                    abandoned.store(true, Ordering::Relaxed);
                    failure = Some(format!("timed out after {:.1}s", limit.as_secs_f64()));
                    break;
                }
                Err(None) => break,
            }
        }
        // A worker past the deadline finishes its call before the scope ends; the model
        // already has the timeout text for it, so its late result is dropped.
    });
    // An admitted call without a result has a failure: the deadline passed or no worker
    // started. A worker that panics makes the scope panic before this point.
    calls
        .iter()
        .zip(denied.into_iter().zip(results))
        .filter_map(|(call, (denied, result))| {
            if denied.is_some() {
                return denied;
            }
            let mut output = result.or_else(|| {
                failure.as_ref().map(|reason| Output {
                    content: format!("Error executing tool '{}': {reason}", call.name),
                    is_error: true,
                })
            })?;
            chain.post_tool(call, &mut output);
            Some(output)
        })
        .collect()
}

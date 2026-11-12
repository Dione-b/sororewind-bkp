//! narrate: turn the host's diagnostic stream into a call tree.
//!
//! The host records `fn_call` before opening a frame and `fn_return` only when
//! that frame returns successfully, so a frame that never returned is a frame
//! on the failure path. `error` events carry the `ScError` itself, which is
//! where the failure is actually named.

use serde::Serialize;
use stellar_xdr::{DiagnosticEvent, ScError, ScVal};

use crate::display::{self, event_parts, scval};
use crate::strkey;

/// One frame of the invocation, in the order it was entered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    /// How many frames were open when this one was entered; the outermost
    /// call is 0.
    pub depth: usize,
    /// Full strkey of the contract that was called, when the host told us.
    pub contract: Option<String>,
    /// The function name from the `fn_call` event; empty if it had none.
    pub function: String,
    /// The arguments, each rendered for reading (see `display::scval`).
    pub args: Vec<String>,
    /// The arguments whole, each an `ScVal` in base64 XDR: `args` abbreviates
    /// long byte strings, large collections and deep nesting.
    pub args_xdr: Vec<String>,
    /// What the frame returned, when it returned at all.
    pub returned: Option<String>,
    /// The return value whole, an `ScVal` in base64 XDR.
    pub returned_xdr: Option<String>,
    /// Still open when the replay ended: this frame is on the failure path.
    pub open: bool,
    /// An inner frame failed and its caller carried on — the signature of a
    /// `try_call`. Set on the frame that failed.
    pub recovered: bool,
    /// The frame the fatal error was raised in.
    #[serde(rename = "failure")]
    pub is_failure: bool,
    /// `log` diagnostics emitted inside this frame.
    pub logs: Vec<String>,
}

/// Where and what the replay died on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    /// The fatal error itself, as the host raised it.
    #[serde(skip)]
    pub error: ScError,
    /// Error category: `contract`, `auth`, `storage`, `wasm_vm`, …
    pub error_type: &'static str,
    /// Host-defined code such as `InvalidAction`; `null` for contract errors.
    pub error_code: Option<&'static str>,
    /// The contract's own error number; `null` for host errors.
    pub contract_code: Option<u32>,
    /// The error in one phrase, as the narrative prints it.
    pub description: String,
    /// Contract-declared name for `contract_code`, when its spec has one.
    pub name: Option<String>,
    /// Where `name` came from.
    pub name_origin: Option<String>,
    /// The message the host attached to the error diagnostic.
    pub message: String,
    /// Full strkey of the contract whose frame raised the error, when known.
    pub contract: Option<String>,
    /// The function whose frame raised the error; `null` when no frame was
    /// open.
    pub function: Option<String>,
    /// Depth of the frame the error was raised in; `null` when no frame was
    /// open.
    pub depth: Option<usize>,
    /// Every `error` diagnostic, oldest first — the escalation trail.
    pub trail: Vec<String>,
    /// The contract error behind a host error that only wraps it: a custom
    /// account's `__check_auth` failing surfaces as an auth error, for one.
    pub cause: Option<Cause>,
    /// Index into [`Story::calls`] of the frame that failed, if any: the
    /// cause's frame when there is a cause.
    #[serde(skip)]
    pub call_index: Option<usize>,
}

/// A contract error the fatal host error was raised in response to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    /// The contract's own error number.
    pub contract_code: u32,
    /// The error in one phrase, as the narrative prints it.
    pub description: String,
    /// Contract-declared name for `contract_code`, when its spec has one.
    pub name: Option<String>,
    /// Where `name` came from.
    pub name_origin: Option<String>,
    /// The message the host attached to the contract error diagnostic.
    pub message: String,
    /// Full strkey of the contract that returned the error, when known.
    pub contract: Option<String>,
    /// The function that returned the error; `null` when no frame was open.
    pub function: Option<String>,
    /// Depth of the frame that returned the error; `null` when no frame was
    /// open.
    pub depth: Option<usize>,
}

/// An `error` diagnostic and the frame that was open when it fired.
struct ErrorEvent {
    error: Option<ScError>,
    message: String,
    open_frame: Option<usize>,
    /// How many frames had been entered or left when it fired. Errors of one
    /// escalation share a stretch: a failing frame neither calls nor returns.
    stretch: usize,
}

/// What the replay did: the call tree, and the failure it ended in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Story {
    /// The fatal error and where it was raised; `None` when the replay ended
    /// without one.
    pub failure: Option<Failure>,
    /// Every frame the host entered, in the order it entered them.
    pub calls: Vec<Call>,
}

impl Story {
    /// Walk the diagnostics once, keeping a stack of open frames.
    ///
    /// `fatal` is the `ScError` the replay itself ended with; it is what
    /// identifies *which* `error` diagnostic is the fatal one, as opposed to an
    /// error a `try_call` caught and shrugged off.
    pub fn build(events: &[DiagnosticEvent], fatal: Option<&ScError>) -> Story {
        let mut calls: Vec<Call> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut errors: Vec<ErrorEvent> = Vec::new();
        // Frames an `error` was emitted from; see `live_frame`.
        let mut errored: Vec<usize> = Vec::new();
        let mut stretch = 0usize;

        for event in events {
            let (topics, data) = event_parts(&event.event);
            let Some(kind) = topics.first().and_then(symbol) else {
                continue;
            };
            // The contract the event was emitted from: the caller for
            // `fn_call`, the returning contract for `fn_return`.
            let emitter = event
                .event
                .contract_id
                .as_ref()
                .map(|id| strkey::contract(&id.0.0));

            match kind.as_str() {
                "fn_call" => {
                    stretch += 1;
                    abandon_frames_above_caller(
                        &mut calls,
                        &mut stack,
                        &errored,
                        emitter.as_deref(),
                    );
                    let contract = topics
                        .get(1)
                        .and_then(bytes32)
                        .map(|id| strkey::contract(&id))
                        .or(emitter);
                    calls.push(Call {
                        depth: stack.len(),
                        contract,
                        function: topics.get(2).and_then(symbol).unwrap_or_default(),
                        args: call_args(data).iter().map(scval).collect(),
                        args_xdr: call_args(data).iter().filter_map(xdr).collect(),
                        returned: None,
                        returned_xdr: None,
                        open: true,
                        recovered: false,
                        is_failure: false,
                        logs: Vec::new(),
                    });
                    stack.push(calls.len() - 1);
                }
                "fn_return" => {
                    stretch += 1;
                    let function = topics.get(1).and_then(symbol);
                    close_frame(
                        &mut calls,
                        &mut stack,
                        function.as_deref(),
                        emitter.as_deref(),
                        data,
                    );
                }
                "error" => {
                    let message = message(data);
                    // The host's own word that a `try_call` caught the failure
                    // of the frames above the caller: they are dead, and the
                    // caller, which emits it, is alive.
                    let caught = message.starts_with(TRY_CALL_FAILED);
                    if caught {
                        abandon_frames_above_caller(
                            &mut calls,
                            &mut stack,
                            &errored,
                            emitter.as_deref(),
                        );
                    }
                    let open_frame = emitting_frame(&calls, &stack, emitter.as_deref());
                    if !caught {
                        errored.extend(open_frame);
                    }
                    errors.push(ErrorEvent {
                        error: topics.get(1).and_then(sc_error),
                        message,
                        open_frame,
                        stretch,
                    });
                }
                "log" => {
                    let frame = live_frame(&calls, &stack, &errored, emitter.as_deref())
                        .map(|position| stack[position])
                        .or_else(|| emitting_frame(&calls, &stack, emitter.as_deref()));
                    if let Some(frame) = frame {
                        calls[frame].logs.push(message(data));
                    }
                }
                _ => {}
            }
        }

        let failure = fatal.map(|fatal| build_failure(&errors, fatal, &calls, &stack, stretch));
        if let Some(call) = failure
            .as_ref()
            .and_then(|f| f.call_index)
            .and_then(|i| calls.get_mut(i))
        {
            call.is_failure = true;
        }

        Story { failure, calls }
    }
}

/// The message of the `error` diagnostic the host emits, from the caller,
/// when a `try_call` returns the callee's error instead of failing.
const TRY_CALL_FAILED: &str = "contract try_call failed";

/// The innermost open frame of the contract an event was emitted from,
/// falling back to the innermost frame of any contract.
///
/// The two differ after a `try_call` recovery: the failed frame is still on
/// the stack (nothing announces its death), but the event comes from its
/// caller.
fn emitting_frame(calls: &[Call], stack: &[usize], emitter: Option<&str>) -> Option<usize> {
    emitter
        .and_then(|contract| {
            stack
                .iter()
                .rev()
                .find(|&&i| calls[i].contract.as_deref() == Some(contract))
        })
        .or(stack.last())
        .copied()
}

/// The stack position of the frame of `contract` that is still running.
///
/// Nothing announces the death of a frame whose failure a `try_call`
/// absorbed, so with recursion (A calls A) the innermost frame of a contract
/// may be a dead one. A frame that emitted an `error` is taken to be dead
/// when an outer frame of the same contract did not: only when every frame
/// of the contract has errored is the innermost one used. A `try_call
/// failed` diagnostic does not count: the frame that emits it is alive.
fn live_frame(
    calls: &[Call],
    stack: &[usize],
    errored: &[usize],
    contract: Option<&str>,
) -> Option<usize> {
    let contract = contract?;
    let of_contract = |i: &usize| calls[*i].contract.as_deref() == Some(contract);
    stack
        .iter()
        .rposition(|i| of_contract(i) && !errored.contains(i))
        .or_else(|| stack.iter().rposition(of_contract))
}

/// A `fn_call` is emitted by the calling contract. If that contract's frame
/// is not the innermost one, every frame above it has failed without a
/// `fn_return` and its caller has moved on: a recovered `try_call`.
fn abandon_frames_above_caller(
    calls: &mut [Call],
    stack: &mut Vec<usize>,
    errored: &[usize],
    caller: Option<&str>,
) {
    let Some(position) = live_frame(calls, stack, errored, caller) else {
        return;
    };
    for &abandoned in &stack[position + 1..] {
        calls[abandoned].open = false;
        calls[abandoned].recovered = true;
    }
    stack.truncate(position + 1);
}

/// Close the frame a `fn_return` belongs to.
fn close_frame(
    calls: &mut [Call],
    stack: &mut Vec<usize>,
    function: Option<&str>,
    contract: Option<&str>,
    value: &ScVal,
) {
    let returning = |call: &Call| {
        function.is_some_and(|f| call.function == f)
            // A frame or an event without a contract id cannot be told apart
            // by contract; the function name has to do.
            && match (call.contract.as_deref(), contract) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    };
    // A return that matches no open frame still closes something: the host
    // emits them strictly nested, so it can only be the innermost frame.
    let position = stack
        .iter()
        .rposition(|&i| returning(&calls[i]))
        .or(stack.len().checked_sub(1));
    let Some(position) = position else {
        return;
    };

    for &abandoned in &stack[position + 1..] {
        calls[abandoned].open = false;
        calls[abandoned].recovered = true;
    }
    let frame = stack[position];
    calls[frame].returned = Some(scval(value));
    calls[frame].returned_xdr = xdr(value);
    calls[frame].open = false;
    stack.truncate(position);
}

fn build_failure(
    errors: &[ErrorEvent],
    fatal: &ScError,
    calls: &[Call],
    stack: &[usize],
    last_stretch: usize,
) -> Failure {
    // The fatal error is the end of the last escalation: the errors raised
    // since the last frame was entered or left. Its first diagnostic there is
    // where it originated; later ones are the same error climbing the frame
    // stack. An equal error from earlier on was caught by a `try_call`, and so
    // is any error from before a frame was last entered or left: when the
    // last stretch has none, the fatal error left no diagnostic at all.
    //
    // A frame a `try_call` recovered from did not fail the transaction, so
    // nothing raised in one is the origin.
    let recovered = |event: &ErrorEvent| event.open_frame.is_some_and(|i| calls[i].recovered);
    let escalation = || {
        errors
            .iter()
            .enumerate()
            .filter(move |(_, event)| event.stretch == last_stretch)
    };
    let origin = escalation()
        .filter(|(_, event)| !recovered(event))
        .find(|(_, event)| event.error.as_ref() == Some(fatal))
        .or_else(|| escalation().rfind(|(_, event)| !recovered(event)));

    // Without an origin, the failure belongs to the frame that was running.
    let (message, call_index) = match origin {
        Some((_, event)) => (event.message.clone(), event.open_frame),
        None => (String::new(), stack.last().copied()),
    };

    // When nothing was open (auth failures, budget blow-ups before the first
    // call) there is no frame to hang the failure on.
    let frame = call_index.and_then(|i| calls.get(i));

    // A host error can be the host's answer to a contract error: the last one
    // raised before it, in the same escalation. Callers re-emit the code of a
    // failed callee as it climbs (`contract try_call failed`, frame-exit
    // escalations), so the contract error is credited to its first emission
    // there: the innermost frame, whose contract declared the code.
    let cause_event = match (fatal, origin) {
        (ScError::Contract(_), _) | (_, None) => None,
        (_, Some((origin, _))) => {
            let contract_errors = || {
                escalation()
                    .take_while(move |(index, _)| *index < origin)
                    .filter_map(|(_, event)| match event.error {
                        Some(ScError::Contract(code)) => Some((code, event)),
                        _ => None,
                    })
            };
            contract_errors()
                .last()
                .and_then(|(last, _)| contract_errors().find(|(code, _)| *code == last))
        }
    };
    let cause = cause_event.map(|(code, event)| {
        let frame = event.open_frame.and_then(|i| calls.get(i));
        Cause {
            contract_code: code,
            description: display::describe_error(&ScError::Contract(code)),
            name: None,
            name_origin: None,
            message: event.message.clone(),
            contract: frame.and_then(|call| call.contract.clone()),
            function: frame.map(|call| call.function.clone()),
            depth: frame.map(|call| call.depth),
        }
    });
    // The cause's frame is the one that failed, unless a `try_call` caught
    // its error: then the frame that went on to fail is.
    let cause_frame = cause_event
        .and_then(|(_, event)| event.open_frame)
        .filter(|&i| !calls[i].recovered);

    Failure {
        error: fatal.clone(),
        error_type: display::error_type_code(fatal.discriminant()),
        error_code: display::error_code(fatal).map(|code| code.name()),
        contract_code: match fatal {
            ScError::Contract(code) => Some(*code),
            _ => None,
        },
        description: display::describe_error(fatal),
        name: None,
        name_origin: None,
        message,
        contract: frame.and_then(|call| call.contract.clone()),
        function: frame.map(|call| call.function.clone()),
        depth: frame.map(|call| call.depth),
        trail: errors.iter().map(|e| e.message.clone()).collect(),
        cause,
        call_index: cause_frame.or(call_index),
    }
}

fn call_args(data: &ScVal) -> &[ScVal] {
    match data {
        ScVal::Void => &[],
        ScVal::Vec(Some(items)) => items.0.as_slice(),
        other => std::slice::from_ref(other),
    }
}

/// A value whole, in base64 XDR, for the JSON document.
fn xdr(value: &ScVal) -> Option<String> {
    crate::codec::encode(value, "a value")
        .ok()
        .map(|bytes| crate::codec::b64_encode(&bytes))
}

/// The `error` and `log` diagnostics carry their message as the first data
/// argument, followed by the values it is about: `message [arg, arg]`.
fn message(data: &ScVal) -> String {
    let text = |value: &ScVal| match value {
        ScVal::String(s) => String::from_utf8_lossy(&s.0).into_owned(),
        other => scval(other),
    };
    match data {
        ScVal::Void => String::new(),
        ScVal::Vec(Some(items)) => match items.0.split_first() {
            None => String::new(),
            Some((first, [])) => text(first),
            Some((first, args)) => format!(
                "{} [{}]",
                text(first),
                args.iter().map(scval).collect::<Vec<_>>().join(", ")
            ),
        },
        other => text(other),
    }
}

fn symbol(value: &ScVal) -> Option<String> {
    match value {
        ScVal::Symbol(s) => Some(String::from_utf8_lossy(&s.0).into_owned()),
        _ => None,
    }
}

fn bytes32(value: &ScVal) -> Option<[u8; 32]> {
    match value {
        ScVal::Bytes(bytes) => bytes.0.as_slice().try_into().ok(),
        _ => None,
    }
}

fn sc_error(value: &ScVal) -> Option<ScError> {
    match value {
        ScVal::Error(error) => Some(error.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{error_event, fn_call, fn_return, log_event};
    use stellar_xdr::ScErrorCode;

    const A: [u8; 32] = [0xa; 32];
    const B: [u8; 32] = [0xb; 32];

    fn id(bytes: [u8; 32]) -> String {
        strkey::contract(&bytes)
    }

    #[test]
    fn nested_calls_that_return() {
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![ScVal::U32(1)]),
                fn_call(Some(A), B, "inner", vec![]),
                log_event(Some(B), "inside inner"),
                fn_return(B, "inner", ScVal::U32(2)),
                fn_return(A, "outer", ScVal::U32(3)),
            ],
            None,
        );
        assert!(story.failure.is_none());
        let [outer, inner] = story.calls.as_slice() else {
            panic!("expected two frames, got {:?}", story.calls);
        };
        assert_eq!(
            (
                outer.depth,
                outer.contract.as_deref(),
                outer.function.as_str()
            ),
            (0, Some(id(A).as_str()), "outer")
        );
        assert_eq!(outer.args, ["1"]);
        assert_eq!(outer.returned.as_deref(), Some("3"));
        assert_eq!((inner.depth, inner.returned.as_deref()), (1, Some("2")));
        assert_eq!(inner.logs, ["inside inner"]);
        assert!(
            story
                .calls
                .iter()
                .all(|c| !c.open && !c.recovered && !c.is_failure)
        );
    }

    #[test]
    fn failure_leaves_the_path_open() {
        let fatal = ScError::Contract(3);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "inner", vec![]),
                error_event(Some(B), fatal.clone(), "boom"),
                error_event(Some(A), fatal.clone(), "escalating"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.description, "contract error #3");
        assert_eq!(
            (
                failure.error_type,
                failure.error_code,
                failure.contract_code
            ),
            ("contract", None, Some(3))
        );
        // The first diagnostic carrying the fatal error names the origin.
        assert_eq!(failure.message, "boom");
        assert_eq!(failure.trail, ["boom", "escalating"]);
        assert_eq!(failure.contract.as_deref(), Some(id(B).as_str()));
        assert_eq!(
            (failure.function.as_deref(), failure.depth),
            (Some("inner"), Some(1))
        );

        assert!(story.calls[0].open && !story.calls[0].is_failure);
        assert!(story.calls[1].open && story.calls[1].is_failure);
    }

    /// Regression test for the `try_call` recovery bug: the return value used
    /// to land on the failed inner frame and the outer frame stayed open.
    #[test]
    fn recovered_inner_failure() {
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "inner", vec![]),
                error_event(Some(B), ScError::Contract(9), "inner failed"),
                error_event(Some(A), ScError::Contract(9), "contract try_call failed"),
                fn_return(A, "outer", ScVal::U32(7)),
            ],
            None,
        );
        let [outer, inner] = story.calls.as_slice() else {
            panic!("expected two frames");
        };
        // The outer frame is closed with its own return value...
        assert_eq!(outer.returned.as_deref(), Some("7"));
        assert!(!outer.open && !outer.recovered);
        // ...and the inner one is marked recovered, without a return value.
        assert_eq!(inner.returned, None);
        assert!(!inner.open && inner.recovered);
        assert!(story.failure.is_none());
    }

    #[test]
    fn calls_after_a_recovered_failure_are_siblings_not_children() {
        const C: [u8; 32] = [0xc; 32];
        let story = Story::build(
            &[
                fn_call(None, A, "batch", vec![]),
                fn_call(Some(A), B, "first", vec![]),
                error_event(Some(B), ScError::Contract(9), "first failed"),
                error_event(Some(A), ScError::Contract(9), "contract try_call failed"),
                // `batch` moves on: the next call comes from A, not from B.
                fn_call(Some(A), C, "second", vec![]),
                log_event(Some(C), "in second"),
                fn_return(C, "second", ScVal::Void),
                log_event(Some(A), "back in batch"),
                fn_return(A, "batch", ScVal::Void),
            ],
            None,
        );
        let depths: Vec<usize> = story.calls.iter().map(|c| c.depth).collect();
        assert_eq!(depths, [0, 1, 1]);
        assert!(story.calls[1].recovered && !story.calls[1].open);
        assert_eq!(story.calls[1].returned, None);
        assert_eq!(story.calls[2].logs, ["in second"]);
        assert_eq!(story.calls[0].logs, ["back in batch"]);
        assert!(story.calls.iter().all(|c| !c.open));
    }

    #[test]
    fn errors_are_attributed_to_the_contract_that_raised_them() {
        // B failed and A recovered; the fatal error is then raised by A while
        // B's dead frame is still the innermost one on the stack.
        let fatal = ScError::Contract(5);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "inner", vec![]),
                error_event(Some(B), ScError::Contract(9), "inner failed"),
                error_event(Some(A), fatal.clone(), "outer gives up"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.unwrap();
        assert_eq!(failure.function.as_deref(), Some("outer"));
        assert_eq!(failure.depth, Some(0));
        assert!(story.calls[0].is_failure && !story.calls[1].is_failure);
    }

    #[test]
    fn recovered_failure_then_fatal_error_picks_the_fatal_one() {
        let fatal = ScError::Auth(ScErrorCode::InvalidAction);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "inner", vec![]),
                error_event(Some(B), ScError::Contract(9), "recovered"),
                fn_call(Some(A), B, "second", vec![]),
                error_event(Some(B), fatal.clone(), "fatal"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.unwrap();
        assert_eq!(failure.message, "fatal");
        assert_eq!(failure.function.as_deref(), Some("second"));
        assert_eq!(failure.description, "auth error InvalidAction");
        assert_eq!(
            (failure.error_type, failure.error_code),
            ("auth", Some("InvalidAction"))
        );
        assert_eq!(failure.call_index, Some(2));
    }

    #[test]
    fn recursion_closes_the_innermost_frame() {
        let story = Story::build(
            &[
                fn_call(None, A, "walk", vec![ScVal::U32(2)]),
                fn_call(Some(A), A, "walk", vec![ScVal::U32(1)]),
                fn_return(A, "walk", ScVal::U32(10)),
                fn_return(A, "walk", ScVal::U32(20)),
            ],
            None,
        );
        assert_eq!(story.calls[0].returned.as_deref(), Some("20"));
        assert_eq!(story.calls[1].returned.as_deref(), Some("10"));
        assert!(story.calls.iter().all(|c| !c.recovered));
    }

    #[test]
    fn same_function_name_on_two_contracts() {
        // B::run fails; the `run` that returns is A's. Matching on the name
        // alone would close B's frame with A's value.
        let story = Story::build(
            &[
                fn_call(None, A, "run", vec![]),
                fn_call(Some(A), B, "run", vec![]),
                error_event(Some(B), ScError::Contract(1), "inner failed"),
                fn_return(A, "run", ScVal::Bool(true)),
            ],
            None,
        );
        assert_eq!(story.calls[0].returned.as_deref(), Some("true"));
        assert_eq!(story.calls[1].returned, None);
        assert!(story.calls[1].recovered);
    }

    #[test]
    fn error_with_no_open_frame() {
        let fatal = ScError::Auth(ScErrorCode::InvalidAction);
        let story = Story::build(
            &[error_event(None, fatal.clone(), "bad signature")],
            Some(&fatal),
        );
        assert!(story.calls.is_empty());
        let failure = story.failure.unwrap();
        assert_eq!(failure.message, "bad signature");
        assert_eq!(
            (failure.contract, failure.function, failure.depth),
            (None, None, None)
        );
        assert_eq!(failure.call_index, None);
    }

    #[test]
    fn fatal_error_without_any_diagnostic_blames_the_open_frame() {
        let fatal = ScError::Budget(ScErrorCode::ExceededLimit);
        let story = Story::build(&[fn_call(None, A, "spin", vec![])], Some(&fatal));
        let failure = story.failure.unwrap();
        assert_eq!(failure.message, "");
        assert_eq!(failure.function.as_deref(), Some("spin"));
        assert!(story.calls[0].is_failure);
    }

    #[test]
    fn unmatched_error_falls_back_to_the_last_diagnostic() {
        let story = Story::build(
            &[
                fn_call(None, A, "f", vec![]),
                error_event(Some(A), ScError::Contract(1), "first"),
                error_event(Some(A), ScError::Contract(2), "last"),
            ],
            Some(&ScError::Contract(99)),
        );
        assert_eq!(story.failure.unwrap().message, "last");
    }

    #[test]
    fn logs_outside_any_frame_are_dropped() {
        let story = Story::build(&[log_event(None, "orphan")], None);
        assert_eq!(story, Story::default());
    }

    #[test]
    fn a_stray_return_closes_the_innermost_frame() {
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_return(B, "unrelated", ScVal::Void),
                fn_return(A, "nothing-open", ScVal::Void),
            ],
            None,
        );
        assert_eq!(story.calls[0].returned.as_deref(), Some("void"));
        assert!(!story.calls[0].open);
    }

    /// A recovered error and the fatal one sharing a number, in the same
    /// contract or not, must not be confused.
    #[test]
    fn a_recovered_error_with_the_fatal_code_is_not_the_origin() {
        const C: [u8; 32] = [0xc; 32];
        let fatal = ScError::Contract(1);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "probe", vec![]),
                error_event(Some(B), fatal.clone(), "probe failed"),
                error_event(Some(A), fatal.clone(), "contract try_call failed"),
                fn_call(Some(A), C, "act", vec![]),
                error_event(Some(C), fatal.clone(), "act failed"),
                error_event(Some(A), fatal.clone(), "escalating"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.message, "act failed");
        assert_eq!(failure.contract.as_deref(), Some(id(C).as_str()));
        assert_eq!(failure.function.as_deref(), Some("act"));
        let [outer, probe, act] = story.calls.as_slice() else {
            panic!("expected three frames, got {:?}", story.calls);
        };
        assert!(probe.recovered && !probe.open && !probe.is_failure);
        assert!(act.open && act.is_failure);
        assert!(outer.open && !outer.is_failure);
    }

    #[test]
    fn a_recovered_self_call_does_not_adopt_later_frames() {
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                // A calls itself; the inner call fails and the outer recovers.
                fn_call(Some(A), A, "inner", vec![]),
                error_event(Some(A), ScError::Contract(9), "inner failed"),
                log_event(Some(A), "carrying on"),
                fn_call(Some(A), B, "next", vec![]),
                fn_return(B, "next", ScVal::Void),
                fn_return(A, "outer", ScVal::U32(1)),
            ],
            None,
        );
        let [outer, inner, next] = story.calls.as_slice() else {
            panic!("expected three frames, got {:?}", story.calls);
        };
        assert!(inner.recovered && !inner.open);
        // `next` is a sibling of the dead frame, not its child.
        assert_eq!((inner.depth, next.depth), (1, 1));
        assert_eq!(next.returned.as_deref(), Some("void"));
        // The log after the recovery belongs to the frame that is running.
        assert_eq!(outer.logs, ["carrying on"]);
        assert!(inner.logs.is_empty());
        assert_eq!(outer.returned.as_deref(), Some("1"));
    }

    #[test]
    fn a_host_error_names_the_contract_error_behind_it() {
        // A custom account rejects the signature: the host reports an auth
        // error, raised in answer to `__check_auth`'s contract error.
        let fatal = ScError::Auth(ScErrorCode::InvalidAction);
        let story = Story::build(
            &[
                fn_call(None, A, "transfer", vec![]),
                fn_call(Some(A), B, "__check_auth", vec![]),
                error_event(
                    Some(B),
                    ScError::Contract(3),
                    "escalating frame-exit to Err",
                ),
                error_event(Some(A), fatal.clone(), "failed account authentication"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.description, "auth error InvalidAction");
        assert_eq!(failure.function.as_deref(), Some("transfer"));
        let cause = failure.cause.as_ref().unwrap();
        assert_eq!(
            (cause.contract_code, cause.description.as_str()),
            (3, "contract error #3")
        );
        assert_eq!(cause.contract.as_deref(), Some(id(B).as_str()));
        assert_eq!(
            (cause.function.as_deref(), cause.depth),
            (Some("__check_auth"), Some(1))
        );
        // The frame that failed is the one the cause was raised in.
        assert!(story.calls[1].is_failure && !story.calls[0].is_failure);

        // A contract error caught by a try_call earlier on is not a cause.
        let story = Story::build(
            &[
                fn_call(None, A, "transfer", vec![]),
                fn_call(Some(A), B, "probe", vec![]),
                error_event(Some(B), ScError::Contract(3), "probe failed"),
                fn_call(Some(A), B, "other", vec![]),
                fn_return(B, "other", ScVal::Void),
                error_event(Some(A), fatal.clone(), "not authorized"),
            ],
            Some(&fatal),
        );
        assert_eq!(story.failure.as_ref().unwrap().cause, None);
        // Nor is a contract error ever the "cause" of a contract error.
        let fatal = ScError::Contract(4);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                error_event(Some(A), ScError::Contract(3), "first"),
                error_event(Some(A), fatal.clone(), "second"),
            ],
            Some(&fatal),
        );
        assert_eq!(story.failure.as_ref().unwrap().cause, None);
    }

    /// A host error that leaves no diagnostic, after a `try_call` was
    /// recovered from, must not be blamed on the recovered frame.
    #[test]
    fn a_silent_fatal_error_is_not_blamed_on_a_recovered_frame() {
        const C: [u8; 32] = [0xc; 32];
        let fatal = ScError::Budget(ScErrorCode::ExceededLimit);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "middle", vec![]),
                // B try-calls C, which fails; B handles it and returns.
                fn_call(Some(B), C, "probe", vec![]),
                error_event(Some(C), ScError::Contract(3), "probe failed"),
                error_event(Some(B), ScError::Contract(3), "contract try_call failed"),
                fn_return(B, "middle", ScVal::Void),
                // Then the budget runs out in A, with no diagnostic.
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.message, "");
        assert_eq!(failure.contract.as_deref(), Some(id(A).as_str()));
        assert_eq!(
            (failure.function.as_deref(), failure.depth),
            (Some("outer"), Some(0))
        );
        assert_eq!(failure.cause, None);
        let [outer, middle, probe] = story.calls.as_slice() else {
            panic!("expected three frames, got {:?}", story.calls);
        };
        assert!(outer.open && outer.is_failure);
        assert!(!middle.open && !middle.is_failure);
        assert!(probe.recovered && !probe.is_failure);
        // No frame is ever both.
        assert!(story.calls.iter().all(|c| !(c.recovered && c.is_failure)));

        // With nothing left open, there is no frame to blame.
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "probe", vec![]),
                error_event(Some(B), ScError::Contract(3), "probe failed"),
                fn_return(A, "outer", ScVal::Void),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.message, "");
        assert_eq!(
            (failure.function.as_deref(), failure.call_index),
            (None, None)
        );
        assert!(story.calls[1].recovered && !story.calls[1].is_failure);
    }

    /// The caller re-emits a caught contract error as `contract try_call
    /// failed`; the cause is still the callee that raised it, so its name is
    /// looked up in the callee's spec.
    #[test]
    fn a_cause_caught_by_try_call_is_credited_to_the_callee() {
        let fatal = ScError::WasmVm(ScErrorCode::InvalidAction);
        let story = Story::build(
            &[
                fn_call(None, A, "outer", vec![]),
                fn_call(Some(A), B, "probe", vec![]),
                error_event(Some(B), ScError::Contract(3), "probe failed"),
                error_event(Some(A), ScError::Contract(3), "contract try_call failed"),
                // A unwraps the error it was handed back and traps.
                error_event(Some(A), fatal.clone(), "escalating error to VM trap"),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.message, "escalating error to VM trap");
        assert_eq!(failure.function.as_deref(), Some("outer"));
        let cause = failure.cause.as_ref().unwrap();
        assert_eq!(cause.contract_code, 3);
        assert_eq!(cause.message, "probe failed");
        assert_eq!(cause.contract.as_deref(), Some(id(B).as_str()));
        assert_eq!(
            (cause.function.as_deref(), cause.depth),
            (Some("probe"), Some(1))
        );
        let [outer, probe] = story.calls.as_slice() else {
            panic!("expected two frames, got {:?}", story.calls);
        };
        // The try_call caught B's failure: B is recovered, and A, which went
        // on to trap, is the frame that failed.
        assert!(probe.recovered && !probe.open && !probe.is_failure);
        assert!(outer.open && outer.is_failure);
        assert_eq!(failure.call_index, Some(0));
    }

    #[test]
    fn messages_keep_their_arguments() {
        use crate::test_support::{diag, string, sym};
        let fatal = ScError::Contract(10);
        let data = ScVal::Vec(Some(
            vec![
                string("balance is not sufficient"),
                ScVal::I64(5),
                ScVal::I64(-7),
            ]
            .try_into()
            .unwrap(),
        ));
        let story = Story::build(
            &[
                fn_call(None, A, "pay", vec![]),
                diag(
                    Some(A),
                    vec![sym("error"), ScVal::Error(fatal.clone())],
                    data.clone(),
                    false,
                ),
                diag(Some(A), vec![sym("log")], data, false),
            ],
            Some(&fatal),
        );
        let failure = story.failure.as_ref().unwrap();
        assert_eq!(failure.message, "balance is not sufficient [5, -7]");
        assert_eq!(failure.trail, ["balance is not sufficient [5, -7]"]);
        assert_eq!(story.calls[0].logs, ["balance is not sufficient [5, -7]"]);
    }
}

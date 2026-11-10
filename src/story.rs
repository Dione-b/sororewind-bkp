//! narrate: turn the host's diagnostic stream into a call tree.
//!
//! The host records `fn_call` before opening a frame and `fn_return` only when
//! that frame returns successfully, so a frame that never returned is a frame
//! on the failure path. `error` events carry the `ScError` itself, which is
//! where the failure is actually named.

use stellar_xdr::{DiagnosticEvent, ScError, ScErrorType, ScVal};

use crate::tx::{scval_short, split_event};

/// One frame of the invocation, in the order it was entered.
#[derive(Debug, Clone)]
pub struct Call {
    pub depth: usize,
    /// Full strkey of the contract that was called, when the host told us.
    pub contract: Option<String>,
    pub function: String,
    pub args: Vec<String>,
    /// What the frame returned, when it returned at all.
    pub returned: Option<String>,
    /// Still open when the replay ended: this frame is on the failure path.
    pub open: bool,
    /// An inner frame failed and this one carried on — the signature of a
    /// `try_call`.
    pub recovered: bool,
    /// `log` diagnostics emitted inside this frame.
    pub logs: Vec<String>,
    /// The frame the fatal error was raised in.
    pub is_failure: bool,
}

/// Where and what the replay died on.
#[derive(Debug, Clone, Default)]
pub struct Failure {
    pub sc_error: Option<ScError>,
    /// The message the host attached to the error diagnostic.
    pub message: String,
    /// Every `error` diagnostic, oldest first — the escalation trail, printed
    /// under `-vv`.
    pub trail: Vec<String>,
    pub contract: Option<String>,
    pub function: Option<String>,
    pub depth: usize,
    /// Index into [`Story::calls`] of the frame that failed, if any.
    pub call_index: Option<usize>,
}

impl Failure {
    pub fn contract_error_code(&self) -> Option<u32> {
        match &self.sc_error {
            Some(ScError::Contract(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn error_type(&self) -> Option<ScErrorType> {
        self.sc_error.as_ref().map(|e| match e {
            ScError::Contract(_) => ScErrorType::Contract,
            ScError::WasmVm(_) => ScErrorType::WasmVm,
            ScError::Context(_) => ScErrorType::Context,
            ScError::Storage(_) => ScErrorType::Storage,
            ScError::Object(_) => ScErrorType::Object,
            ScError::Crypto(_) => ScErrorType::Crypto,
            ScError::Events(_) => ScErrorType::Events,
            ScError::Budget(_) => ScErrorType::Budget,
            ScError::Value(_) => ScErrorType::Value,
            ScError::Auth(_) => ScErrorType::Auth,
        })
    }
}

/// An `error` diagnostic and the frame that was open when it fired.
struct ErrorEvent {
    sc_error: Option<ScError>,
    message: String,
    open_frame: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Story {
    pub calls: Vec<Call>,
    pub failure: Option<Failure>,
}

impl Story {
    /// Walk the diagnostics once, keeping a stack of open frames.
    ///
    /// `host_error` is the `ScError` the replay itself ended with; it is what
    /// identifies *which* `error` diagnostic is the fatal one, as opposed to an
    /// error a `try_call` caught and shrugged off.
    pub fn build(events: &[DiagnosticEvent], host_error: Option<&ScError>) -> Story {
        let mut calls: Vec<Call> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut errors: Vec<ErrorEvent> = Vec::new();

        for event in events {
            let (topics, data) = split_event(&event.event);
            let Some(kind) = topics.first().and_then(symbol) else {
                continue;
            };

            match kind.as_str() {
                "fn_call" => {
                    let contract = topics
                        .get(1)
                        .and_then(bytes32)
                        .map(|b| crate::strkey::contract(&b))
                        .or_else(|| {
                            event
                                .event
                                .contract_id
                                .as_ref()
                                .map(|c| crate::strkey::contract(&c.0 .0))
                        });
                    let function = topics.get(2).and_then(symbol).unwrap_or_default();
                    let depth = stack.len();
                    calls.push(Call {
                        depth,
                        contract,
                        function,
                        args: data_args(&data),
                        returned: None,
                        open: true,
                        recovered: false,
                        logs: Vec::new(),
                        is_failure: false,
                    });
                    stack.push(calls.len() - 1);
                }
                "fn_return" => {
                    let value = scval_short(&data);
                    let function = topics.get(1).and_then(symbol);
                    // Frames above the matching one never returned: an inner
                    // call failed and this one recovered from it.
                    let position = function
                        .as_ref()
                        .and_then(|f| stack.iter().rposition(|&i| calls[i].function == *f));
                    match position {
                        Some(pos) => {
                            for &i in &stack[pos + 1..] {
                                calls[i].open = false;
                                calls[i].recovered = true;
                            }
                            if let Some(&top) = stack.last() {
                                calls[top].returned = Some(value);
                                calls[top].open = false;
                            }
                            stack.truncate(pos);
                        }
                        None => {
                            if let Some(top) = stack.pop() {
                                calls[top].returned = Some(value);
                                calls[top].open = false;
                            }
                        }
                    }
                }
                "error" => errors.push(ErrorEvent {
                    sc_error: topics.get(1).and_then(sc_error_of),
                    message: error_message(&data),
                    open_frame: stack.last().copied(),
                }),
                "log" => {
                    let message = error_message(&data);
                    if let Some(&i) = stack.last() {
                        calls[i].logs.push(message);
                    }
                }
                _ => {}
            }
        }

        let failure = build_failure(&errors, host_error, &calls, &stack);
        if let Some(index) = failure.as_ref().and_then(|f| f.call_index) {
            if let Some(call) = calls.get_mut(index) {
                call.is_failure = true;
            }
        }

        Story { calls, failure }
    }
}

fn build_failure(
    errors: &[ErrorEvent],
    host_error: Option<&ScError>,
    calls: &[Call],
    stack: &[usize],
) -> Option<Failure> {
    let host_error = host_error?;

    // The first diagnostic carrying the final error is where it originated;
    // later ones are the same error being escalated up the frame stack.
    let chosen = errors
        .iter()
        .position(|e| e.sc_error.as_ref() == Some(host_error))
        .or_else(|| errors.last().map(|_| errors.len() - 1));

    let trail: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();

    let (message, call_index) = match chosen.and_then(|i| errors.get(i)) {
        Some(e) => (e.message.clone(), e.open_frame),
        None => (String::new(), stack.last().copied()),
    };

    // When nothing was open (auth failures, budget blow-ups before the first
    // call) there is no frame to hang the failure on.
    let (contract, function, depth) = match call_index.and_then(|i| calls.get(i)) {
        Some(call) => (
            call.contract.clone(),
            Some(call.function.clone()),
            call.depth,
        ),
        None => (None, None, 0),
    };

    Some(Failure {
        sc_error: Some(host_error.clone()),
        message,
        trail,
        contract,
        function,
        depth,
        call_index,
    })
}

fn data_args(data: &ScVal) -> Vec<String> {
    match data {
        ScVal::Void => Vec::new(),
        ScVal::Vec(Some(v)) => v.0.iter().map(scval_short).collect(),
        other => vec![scval_short(other)],
    }
}

/// The `error` diagnostic carries the message as its first data argument.
fn error_message(data: &ScVal) -> String {
    match data {
        ScVal::String(s) => String::from_utf8_lossy(&s.0).into_owned(),
        ScVal::Vec(Some(v)) => {
            v.0.first()
                .map(|first| match first {
                    ScVal::String(s) => String::from_utf8_lossy(&s.0).into_owned(),
                    other => scval_short(other),
                })
                .unwrap_or_default()
        }
        ScVal::Void => String::new(),
        other => scval_short(other),
    }
}

fn symbol(v: &ScVal) -> Option<String> {
    match v {
        ScVal::Symbol(s) => Some(String::from_utf8_lossy(&s.0).into_owned()),
        _ => None,
    }
}

fn bytes32(v: &ScVal) -> Option<[u8; 32]> {
    match v {
        ScVal::Bytes(b) => b.0.as_slice().try_into().ok(),
        _ => None,
    }
}

fn sc_error_of(v: &ScVal) -> Option<ScError> {
    match v {
        ScVal::Error(e) => Some(e.clone()),
        _ => None,
    }
}

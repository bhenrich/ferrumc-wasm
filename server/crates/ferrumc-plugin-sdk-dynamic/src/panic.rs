//! Panic and cooperative-error status conversion.

use std::any::Any;
use std::fmt::{self, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use ferrumc_plugin_abi::{
    FcStatus, FC_DIAGNOSTIC_ERROR, FC_ERROR, FC_INVALID_ARGUMENT, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::PluginCall;
use ferrumc_plugin_sdk::{PluginError, MAX_DIAGNOSTIC_BYTES};

use crate::codec::WireError;

pub(crate) fn cooperative(call: &mut PluginCall<'_>, error: &PluginError) -> FcStatus {
    let mut message = BoundedMessage::new(MAX_DIAGNOSTIC_BYTES);
    let _result = write!(&mut message, "plugin callback failed: {error}");
    let _result = call.diagnostic(FC_DIAGNOSTIC_ERROR, message.as_str());
    FC_ERROR
}

pub(crate) fn invalid_event(call: &mut PluginCall<'_>, error: WireError) -> FcStatus {
    diagnostic(call, "invalid plugin event: ", error.reason());
    FC_INVALID_ARGUMENT
}

pub(crate) fn caught(
    call: &mut PluginCall<'_>,
    hook: &'static str,
    payload: Box<dyn Any + Send>,
) -> FcStatus {
    let detail = panic_detail(payload.as_ref());
    diagnostic(call, hook, detail);
    dispose_payload(payload);
    FC_PLUGIN_PANIC
}

pub(crate) fn drop_caught(
    call: &mut PluginCall<'_>,
    hook: &'static str,
    payload: Box<dyn Any + Send>,
) -> FcStatus {
    caught(call, hook, payload)
}

fn panic_detail(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}

fn diagnostic(call: &mut PluginCall<'_>, prefix: &str, detail: &str) {
    let prefix = bounded(prefix, MAX_DIAGNOSTIC_BYTES);
    let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(prefix.len());
    let detail = bounded(detail, remaining);
    let mut message = String::with_capacity(prefix.len().saturating_add(detail.len()));
    message.push_str(prefix);
    message.push_str(detail);
    let _result = call.diagnostic(FC_DIAGNOSTIC_ERROR, &message);
}

fn bounded(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

fn dispose_payload(payload: Box<dyn Any + Send>) {
    let result = catch_unwind(AssertUnwindSafe(|| drop(payload)));
    if let Err(second_payload) = result {
        // A destructor that panics while destroying a caught panic payload is
        // foreign plugin behavior. Retaining the second payload avoids another
        // destructor unwind on the extern callback path.
        std::mem::forget(second_payload);
    }
}

struct BoundedMessage {
    value: String,
    maximum: usize,
}

impl BoundedMessage {
    fn new(maximum: usize) -> Self {
        Self {
            value: String::with_capacity(maximum),
            maximum,
        }
    }

    fn as_str(&self) -> &str {
        &self.value
    }
}

impl Write for BoundedMessage {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let remaining = self.maximum.saturating_sub(self.value.len());
        self.value.push_str(bounded(value, remaining));
        Ok(())
    }
}

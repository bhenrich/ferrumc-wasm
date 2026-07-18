//! Shared-SDK to ABI-system callback bridge.

use std::panic::{catch_unwind, AssertUnwindSafe};

use ferrumc_plugin_abi::{
    FcSemanticVersion, FcStatus, FC_CAPABILITY_DENIED, FC_OK, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::{PluginCall, PluginEvent};
use ferrumc_plugin_sdk::{
    Capability, CapabilityManifest, Event, EventContext, FacadeError, LoadContext, Plugin,
    PluginError, Tick, UnloadContext, MAX_PLUGIN_ID_BYTES, MAX_PLUGIN_NAME_BYTES,
};

use crate::codec::{self, WireError};
use crate::panic;
use crate::services::AbiServices;
/// Returns an array length proving `P`'s declaration satisfies export bounds.
#[doc(hidden)]
pub const fn declaration_array_len<P: Plugin>() -> usize {
    let declaration = P::DECLARATION;
    if !declaration.id().is_empty()
        && declaration.id().len() <= MAX_PLUGIN_ID_BYTES
        && !declaration.name().is_empty()
        && declaration.name().len() <= MAX_PLUGIN_NAME_BYTES
    {
        1
    } else {
        0
    }
}

/// One initialized plugin instance owned by the ABI-system trampoline.
#[doc(hidden)]
pub struct DynamicInstance<P> {
    plugin: Option<P>,
    capabilities: CapabilityManifest,
}

/// Returns `P`'s numeric semantic version for the immutable descriptor.
#[doc(hidden)]
pub const fn plugin_version<P: Plugin>() -> FcSemanticVersion {
    FcSemanticVersion::new(
        P::DECLARATION.version().major(),
        P::DECLARATION.version().minor(),
        P::DECLARATION.version().patch(),
    )
}

/// Returns `P`'s requested ABI v1 capability mask.
#[doc(hidden)]
pub const fn requested_capabilities<P: Plugin>() -> u64 {
    // ABI v1 widens the SDK's complete u32 bitset without truncation.
    P::DECLARATION.requested_capabilities().bits() as u64
}

/// Initializes one plugin behind the ABI-system callback trampoline.
#[doc(hidden)]
pub fn initialize<P: Plugin>(
    call: &mut PluginCall<'_>,
    granted_capabilities: u64,
) -> Result<DynamicInstance<P>, FcStatus> {
    let create = catch_unwind(AssertUnwindSafe(|| {
        P::DECLARATION
            .validate()
            .map_err(|error| PluginError::failed(error.to_string()))?;
        Ok::<P, PluginError>(P::create())
    }));
    let mut plugin = match create {
        Ok(Ok(plugin)) => plugin,
        Ok(Err(error)) => return Err(panic::cooperative(call, &error)),
        Err(payload) => return Err(panic::caught(call, "plugin create panicked: ", payload)),
    };

    let capabilities = granted_manifest::<P>(granted_capabilities);
    let load = catch_unwind(AssertUnwindSafe(|| {
        let mut services = AbiServices::load(call, capabilities);
        let mut context = LoadContext::new(&mut services);
        plugin.on_load(&mut context)
    }));

    match load {
        Ok(Ok(())) => Ok(DynamicInstance {
            plugin: Some(plugin),
            capabilities,
        }),
        Ok(Err(error)) => {
            let status = panic::cooperative(call, &error);
            match catch_unwind(AssertUnwindSafe(|| drop(plugin))) {
                Ok(()) => Err(status),
                Err(payload) => Err(panic::drop_caught(
                    call,
                    "plugin drop after load failure panicked: ",
                    payload,
                )),
            }
        }
        Err(payload) => {
            std::mem::forget(plugin);
            Err(panic::caught(
                call,
                "plugin load callback panicked: ",
                payload,
            ))
        }
    }
}

/// Decodes and dispatches one event behind the ABI-system callback trampoline.
#[doc(hidden)]
pub fn on_event<P: Plugin>(
    instance: &mut DynamicInstance<P>,
    call: &mut PluginCall<'_>,
    raw_event: PluginEvent<'_>,
) -> FcStatus {
    if instance.plugin.is_none() {
        let _result = call.diagnostic(
            ferrumc_plugin_abi::FC_DIAGNOSTIC_ERROR,
            "plugin callback attempted after a prior panic",
        );
        return FC_PLUGIN_PANIC;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let event = codec::decode_event(raw_event.kind(), raw_event.payload())
            .map_err(DispatchError::Malformed)?;
        let _shard = raw_event.shard();
        require_event_capability(instance.capabilities, &event)?;
        let plugin = instance.plugin.as_mut().ok_or(DispatchError::Poisoned)?;
        dispatch(plugin, instance.capabilities, raw_event.tick(), event, call)
    }));

    match result {
        Ok(Ok(())) => FC_OK,
        Ok(Err(DispatchError::Malformed(error))) => panic::invalid_event(call, error),
        Ok(Err(DispatchError::Cooperative(error))) => panic::cooperative(call, &error),
        Ok(Err(DispatchError::CapabilityDenied)) => {
            let _result = call.diagnostic(
                ferrumc_plugin_abi::FC_DIAGNOSTIC_ERROR,
                "host delivered an event without its required granted capability",
            );
            FC_CAPABILITY_DENIED
        }
        Ok(Err(DispatchError::Poisoned)) => {
            let _result = call.diagnostic(
                ferrumc_plugin_abi::FC_DIAGNOSTIC_ERROR,
                "plugin callback attempted after a prior panic",
            );
            FC_PLUGIN_PANIC
        }
        Err(payload) => {
            if let Some(plugin) = instance.plugin.take() {
                std::mem::forget(plugin);
            }
            panic::caught(call, "plugin event callback panicked: ", payload)
        }
    }
}

fn dispatch<P: Plugin>(
    plugin: &mut P,
    capabilities: CapabilityManifest,
    tick: u64,
    event: Event,
    call: &mut PluginCall<'_>,
) -> Result<(), DispatchError> {
    let mut services = AbiServices::event(call, capabilities);
    match event {
        Event::BlockPlaceAttempt(attempt) => {
            let decision = {
                let mut context = EventContext::new(Tick::new(tick), &mut services);
                plugin.before_block_place(&attempt, &mut context)?
            };
            services.emit_block_decision(&decision)?;
        }
        Event::BlockBreakAttempt(attempt) => {
            let decision = {
                let mut context = EventContext::new(Tick::new(tick), &mut services);
                plugin.before_block_break(&attempt, &mut context)?
            };
            services.emit_event_decision(&decision, Capability::VetoBlockEdits)?;
        }
        Event::ChatAttempt(attempt) => {
            let decision = {
                let mut context = EventContext::new(Tick::new(tick), &mut services);
                plugin.before_chat(&attempt, &mut context)?
            };
            services.emit_event_decision(&decision, Capability::VetoEvents)?;
        }
        Event::InteractAttempt(attempt) => {
            let decision = {
                let mut context = EventContext::new(Tick::new(tick), &mut services);
                plugin.before_interact(&attempt, &mut context)?
            };
            services.emit_event_decision(&decision, Capability::VetoEvents)?;
        }
        Event::Command(invocation) => {
            let mut context = EventContext::new(Tick::new(tick), &mut services);
            plugin.on_command(&invocation, &mut context)?;
        }
        Event::Timer(timer) => {
            let mut context = EventContext::new(Tick::new(tick), &mut services);
            plugin.on_timer(timer, &mut context)?;
        }
        Event::PlayerJoin(_)
        | Event::PlayerLeave(_)
        | Event::BlockBreak(_)
        | Event::AfterBlockPlace(_)
        | Event::AfterBlockBreak(_)
        | Event::PlayerMove(_) => {
            let mut context = EventContext::new(Tick::new(tick), &mut services);
            plugin.on_event(&event, &mut context)?;
        }
        _ => {
            return Err(DispatchError::Malformed(WireError::new(
                "future SDK event has no ABI v1 callback mapping",
            )))
        }
    }
    Ok(())
}

fn require_event_capability(
    capabilities: CapabilityManifest,
    event: &Event,
) -> Result<(), DispatchError> {
    let required = match event {
        Event::PlayerJoin(_)
        | Event::PlayerLeave(_)
        | Event::BlockBreak(_)
        | Event::AfterBlockPlace(_)
        | Event::AfterBlockBreak(_)
        | Event::PlayerMove(_) => Some(Capability::ReceiveEvents),
        Event::BlockPlaceAttempt(_) | Event::BlockBreakAttempt(_) => {
            Some(Capability::VetoBlockEdits)
        }
        Event::ChatAttempt(_) | Event::InteractAttempt(_) => Some(Capability::VetoEvents),
        Event::Command(_) => Some(Capability::RegisterCommands),
        Event::Timer(_) => None,
        _ => return Err(DispatchError::CapabilityDenied),
    };
    if required.is_some_and(|capability| !capabilities.grants(capability)) {
        return Err(DispatchError::CapabilityDenied);
    }
    Ok(())
}

/// Shuts down and destroys one instance behind the ABI-system trampoline.
#[doc(hidden)]
pub fn shutdown<P: Plugin>(
    mut instance: DynamicInstance<P>,
    call: &mut PluginCall<'_>,
) -> FcStatus {
    let Some(mut plugin) = instance.plugin.take() else {
        let _result = call.diagnostic(
            ferrumc_plugin_abi::FC_DIAGNOSTIC_ERROR,
            "plugin shutdown skipped after a prior panic",
        );
        return FC_PLUGIN_PANIC;
    };

    let unload = catch_unwind(AssertUnwindSafe(|| {
        let mut services = AbiServices::unload(call, instance.capabilities);
        let mut context = UnloadContext::new(&mut services);
        plugin.on_unload(&mut context)
    }));
    let callback_status = match unload {
        Ok(Ok(())) => FC_OK,
        Ok(Err(error)) => panic::cooperative(call, &error),
        Err(payload) => {
            std::mem::forget(plugin);
            return panic::caught(call, "plugin unload callback panicked: ", payload);
        }
    };

    match catch_unwind(AssertUnwindSafe(|| drop(plugin))) {
        Ok(()) => callback_status,
        Err(payload) => panic::drop_caught(call, "plugin drop during shutdown panicked: ", payload),
    }
}

fn granted_manifest<P: Plugin>(granted_capabilities: u64) -> CapabilityManifest {
    let low = granted_capabilities & u64::from(u32::MAX);
    let Ok(raw) = u32::try_from(low) else {
        return CapabilityManifest::empty();
    };
    let granted = CapabilityManifest::from_bits_truncate(raw);
    let requested = P::DECLARATION.requested_capabilities();
    CapabilityManifest::from_bits_truncate(granted.bits() & requested.bits())
}

enum DispatchError {
    Malformed(WireError),
    Cooperative(PluginError),
    CapabilityDenied,
    Poisoned,
}

impl From<PluginError> for DispatchError {
    fn from(error: PluginError) -> Self {
        Self::Cooperative(error)
    }
}

impl From<FacadeError> for DispatchError {
    fn from(error: FacadeError) -> Self {
        Self::Cooperative(error.into())
    }
}

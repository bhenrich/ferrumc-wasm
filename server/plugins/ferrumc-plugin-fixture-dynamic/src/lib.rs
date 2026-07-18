//! A real ABI-v1 dynamic fixture for loader and host integration tests.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(not(panic = "unwind"))]
compile_error!("the dynamic plugin fixture requires panic=unwind");

use ferrumc_plugin_abi::{
    FcCommandKind, FcEventKind, FcPluginDescriptorV1, FcPluginFunctionsV1, FcResourceHandle,
    FcSemanticVersion, FcStatus, ABI_MAJOR, ABI_MINOR, FC_CAPABILITY_RECEIVE_EVENTS,
    FC_CAPABILITY_SUBMIT_INTENTS, FC_ERROR, FC_INVALID_ARGUMENT, FC_OK, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::{
    export_plugin_v1, plugin_descriptor_v1, plugin_functions_v1, PluginBridge, PluginCall,
    PluginCallError, PluginEvent,
};

const FIXTURE_ID: &str = "ferrumc-fixture-dynamic";
const FIXTURE_NAME: &str = "FerrumC Dynamic Fixture";
const FIXTURE_TARGET: &str = env!("FERRUMC_FIXTURE_TARGET");
const FIXTURE_VERSION: FcSemanticVersion = FcSemanticVersion::new(1, 0, 0);
const FIXTURE_CAPABILITIES: u64 = FC_CAPABILITY_RECEIVE_EVENTS | FC_CAPABILITY_SUBMIT_INTENTS;
const PLAYER_ID_BYTES: usize = 16;
const BLOCK_BREAK_PAYLOAD_BYTES: usize = 36;
const MESSAGE_TEXT: &str = "dynamic fixture handled event";

struct FixtureBridge;

#[derive(Debug, Default)]
struct FixtureInstance {
    delivered_events: u64,
}

static FUNCTIONS: FcPluginFunctionsV1 = plugin_functions_v1::<FixtureBridge>();
static DESCRIPTOR: FcPluginDescriptorV1 = plugin_descriptor_v1::<FixtureBridge>();

impl PluginBridge for FixtureBridge {
    type Instance = FixtureInstance;

    const ID: &'static str = FIXTURE_ID;
    const NAME: &'static str = FIXTURE_NAME;
    const TARGET: &'static str = FIXTURE_TARGET;
    const VERSION: FcSemanticVersion = FIXTURE_VERSION;
    const REQUESTED_CAPABILITIES: u64 = FIXTURE_CAPABILITIES;

    fn functions() -> &'static FcPluginFunctionsV1 {
        &FUNCTIONS
    }

    fn descriptor() -> &'static FcPluginDescriptorV1 {
        &DESCRIPTOR
    }

    fn initialize(
        call: &mut PluginCall<'_>,
        granted_capabilities: u64,
    ) -> Result<Self::Instance, FcStatus> {
        if granted_capabilities != FIXTURE_CAPABILITIES {
            return Err(FC_INVALID_ARGUMENT);
        }

        for event in [
            FcEventKind::BLOCK_BREAK,
            FcEventKind::AFTER_BLOCK_BREAK,
            FcEventKind::PLAYER_JOIN,
        ] {
            call.emit(
                FcCommandKind::SUBSCRIBE_EVENT,
                FcResourceHandle::INVALID,
                &event.raw().to_le_bytes(),
            )
            .map_err(call_error_status)?;
        }

        Ok(FixtureInstance::default())
    }

    fn on_event(
        instance: &mut Self::Instance,
        call: &mut PluginCall<'_>,
        event: PluginEvent<'_>,
    ) -> FcStatus {
        instance.delivered_events = instance.delivered_events.saturating_add(1);

        if event.kind() == FcEventKind::BLOCK_BREAK {
            return emit_block_break_message(call, event.payload());
        }
        if event.kind() == FcEventKind::AFTER_BLOCK_BREAK {
            let status = emit_block_break_message(call, event.payload());
            if status != FC_OK {
                return status;
            }

            return match call.request(
                ferrumc_plugin_abi::FcHostRequestKind::DIMENSION,
                FcResourceHandle::INVALID,
                &[],
            ) {
                Ok(_) => FC_ERROR,
                Err(error) => call_error_status(error),
            };
        }
        if event.kind() == FcEventKind::PLAYER_JOIN {
            let status = emit_player_message(call, event.payload());
            return if status == FC_OK {
                FC_PLUGIN_PANIC
            } else {
                status
            };
        }

        FC_INVALID_ARGUMENT
    }

    fn shutdown(_instance: Self::Instance, _call: &mut PluginCall<'_>) -> FcStatus {
        FC_OK
    }
}

export_plugin_v1!(FixtureBridge);

fn emit_block_break_message(call: &mut PluginCall<'_>, payload: &[u8]) -> FcStatus {
    let Some(player) = block_break_player(payload) else {
        return FC_INVALID_ARGUMENT;
    };
    emit_message(call, player)
}

fn emit_player_message(call: &mut PluginCall<'_>, payload: &[u8]) -> FcStatus {
    let Ok(player) = <[u8; PLAYER_ID_BYTES]>::try_from(payload) else {
        return FC_INVALID_ARGUMENT;
    };
    emit_message(call, player)
}

fn emit_message(call: &mut PluginCall<'_>, player: [u8; PLAYER_ID_BYTES]) -> FcStatus {
    let Ok(text_len) = u32::try_from(MESSAGE_TEXT.len()) else {
        return FC_ERROR;
    };
    let mut payload = Vec::with_capacity(PLAYER_ID_BYTES + 4 + MESSAGE_TEXT.len());
    payload.extend_from_slice(&player);
    payload.extend_from_slice(&text_len.to_le_bytes());
    payload.extend_from_slice(MESSAGE_TEXT.as_bytes());

    match call.emit(FcCommandKind::MESSAGE, FcResourceHandle::INVALID, &payload) {
        Ok(()) => FC_OK,
        Err(error) => call_error_status(error),
    }
}

fn block_break_player(payload: &[u8]) -> Option<[u8; PLAYER_ID_BYTES]> {
    if payload.len() != BLOCK_BREAK_PAYLOAD_BYTES {
        return None;
    }
    if read_u32(payload, 0)? != BLOCK_BREAK_PAYLOAD_BYTES as u32
        || read_u16(payload, 4)? != ABI_MAJOR
        || read_u16(payload, 6)? != ABI_MINOR
    {
        return None;
    }
    <[u8; PLAYER_ID_BYTES]>::try_from(payload.get(8..24)?).ok()
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    let bytes = <[u8; 2]>::try_from(payload.get(offset..offset.checked_add(2)?)?).ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    let bytes = <[u8; 4]>::try_from(payload.get(offset..offset.checked_add(4)?)?).ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn call_error_status(error: PluginCallError) -> FcStatus {
    match error {
        PluginCallError::HostStatus(status) => status,
        PluginCallError::PayloadTooLong | PluginCallError::InvalidHostOutput => FC_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_break_player, FixtureBridge, BLOCK_BREAK_PAYLOAD_BYTES, FIXTURE_CAPABILITIES,
    };
    use ferrumc_plugin_abi::{
        FcSemanticVersion, ABI_MAJOR, ABI_MINOR, FC_CAPABILITY_RECEIVE_EVENTS,
        FC_CAPABILITY_SUBMIT_INTENTS,
    };
    use ferrumc_plugin_abi_sys::PluginBridge;

    fn block_break_payload(player: [u8; 16]) -> [u8; BLOCK_BREAK_PAYLOAD_BYTES] {
        let mut payload = [0_u8; BLOCK_BREAK_PAYLOAD_BYTES];
        payload[0..4].copy_from_slice(&(BLOCK_BREAK_PAYLOAD_BYTES as u32).to_le_bytes());
        payload[4..6].copy_from_slice(&ABI_MAJOR.to_le_bytes());
        payload[6..8].copy_from_slice(&ABI_MINOR.to_le_bytes());
        payload[8..24].copy_from_slice(&player);
        payload
    }

    #[test]
    fn descriptor_metadata_and_capabilities_are_pinned() {
        assert_eq!(FixtureBridge::ID, "ferrumc-fixture-dynamic");
        assert_eq!(FixtureBridge::NAME, "FerrumC Dynamic Fixture");
        assert!(!FixtureBridge::TARGET.is_empty());
        assert_eq!(FixtureBridge::VERSION, FcSemanticVersion::new(1, 0, 0));
        assert_eq!(
            FIXTURE_CAPABILITIES,
            FC_CAPABILITY_RECEIVE_EVENTS | FC_CAPABILITY_SUBMIT_INTENTS
        );
    }

    #[test]
    fn block_break_payload_requires_exact_current_record() {
        let player = [0x5a; 16];
        let valid = block_break_payload(player);
        assert_eq!(block_break_player(&valid), Some(player));
        assert_eq!(block_break_player(&valid[..valid.len() - 1]), None);

        let mut trailing = valid.to_vec();
        trailing.push(0);
        assert_eq!(block_break_player(&trailing), None);

        for offset in [0, 4, 6] {
            let mut malformed = valid;
            malformed[offset] ^= 0xff;
            assert_eq!(block_break_player(&malformed), None);
        }
    }
}

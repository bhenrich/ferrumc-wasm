use core::mem::{align_of, offset_of, size_of};
use core::ptr;

use crate::{
    AbiVersion, FcAbiHeader, FcBlockBreakEventPayloadV1, FcBlockPosV1,
    FcBlockStateRequestPayloadV1, FcBytesView, FcCallHandle, FcCommandKind, FcCommandV1,
    FcEventKind, FcEventV1, FcHostFunctionsV1, FcHostHandle, FcHostRequestKind, FcHostRequestV1,
    FcOutputBufferV1, FcPlayerIdV1, FcPluginDescriptorV1, FcPluginEntryV1Fn, FcPluginFunctionsV1,
    FcPluginHandle, FcResourceHandle, FcSemanticVersion, FcSetBlockCommandPayloadV1, FcStatus,
    FcStrView, ABI_MAJOR, ABI_MINOR, FC_BUFFER_TOO_SMALL, FC_CAPABILITIES_V1, FC_CAPABILITY_DENIED,
    FC_COMMAND_BUFFER_FULL, FC_ERROR, FC_INVALID_ARGUMENT, FC_OK, FC_PLUGIN_PANIC,
};

fn assert_layout<T>(name: &str, expected_size: usize, expected_align: usize) {
    assert_eq!(
        size_of::<T>(),
        expected_size,
        "{name} size drifted from its ABI commitment"
    );
    assert_eq!(
        align_of::<T>(),
        expected_align,
        "{name} alignment drifted from its ABI commitment"
    );
}

fn assert_field_offset(name: &str, actual: usize, expected: usize) {
    assert_eq!(
        actual, expected,
        "{name} offset drifted from its ABI commitment"
    );
}

fn assert_entry_type(_entry: FcPluginEntryV1Fn) {}

extern "C" fn init(
    _host: FcHostHandle,
    _call: FcCallHandle,
    _host_functions: *const FcHostFunctionsV1,
    _granted_capabilities: u64,
    _output: *mut FcOutputBufferV1,
    _plugin_out: *mut FcPluginHandle,
) -> FcStatus {
    FC_OK
}

extern "C" fn on_event(
    _host: FcHostHandle,
    _plugin: FcPluginHandle,
    _call: FcCallHandle,
    _host_functions: *const FcHostFunctionsV1,
    _event: *const FcEventV1,
    _output: *mut FcOutputBufferV1,
) -> FcStatus {
    FC_OK
}

extern "C" fn shutdown(
    _host: FcHostHandle,
    _plugin: FcPluginHandle,
    _call: FcCallHandle,
    _host_functions: *const FcHostFunctionsV1,
    _output: *mut FcOutputBufferV1,
) -> FcStatus {
    FC_OK
}

extern "C" fn host_call(
    _host: FcHostHandle,
    _call: FcCallHandle,
    _request: *const FcHostRequestV1,
    _output: *mut FcOutputBufferV1,
) -> FcStatus {
    FC_OK
}

extern "C" fn host_emit(
    _host: FcHostHandle,
    _call: FcCallHandle,
    _command: *const FcCommandV1,
) -> FcStatus {
    FC_OK
}

extern "C" fn host_diagnostic(
    _host: FcHostHandle,
    _call: FcCallHandle,
    _level: u32,
    _message: FcStrView,
) -> FcStatus {
    FC_OK
}

extern "C" fn text() -> FcStrView {
    FcStrView::empty()
}

static PLUGIN_FUNCTIONS: FcPluginFunctionsV1 = FcPluginFunctionsV1::new(init, on_event, shutdown);

extern "C" fn plugin_functions_ptr() -> *const FcPluginFunctionsV1 {
    ptr::addr_of!(PLUGIN_FUNCTIONS)
}

static PLUGIN_DESCRIPTOR: FcPluginDescriptorV1 = FcPluginDescriptorV1::new(
    FcSemanticVersion::new(1, 2, 3),
    FC_CAPABILITIES_V1,
    text,
    text,
    text,
    plugin_functions_ptr,
);

extern "C" fn plugin_entry() -> *const FcPluginDescriptorV1 {
    ptr::addr_of!(PLUGIN_DESCRIPTOR)
}

#[test]
fn fixed_abi_v1_records_have_exact_64_bit_layout() {
    assert_eq!(size_of::<*const u8>(), 8, "ABI v1 requires 64-bit pointers");
    assert_eq!(
        size_of::<extern "C" fn()>(),
        8,
        "ABI v1 requires 64-bit function pointers"
    );

    assert_layout::<FcAbiHeader>("FcAbiHeader", 8, 4);
    assert_field_offset(
        "FcAbiHeader.struct_size",
        offset_of!(FcAbiHeader, struct_size),
        0,
    );
    assert_field_offset(
        "FcAbiHeader.abi_major",
        offset_of!(FcAbiHeader, abi_major),
        4,
    );
    assert_field_offset(
        "FcAbiHeader.abi_minor",
        offset_of!(FcAbiHeader, abi_minor),
        6,
    );

    assert_layout::<FcBytesView>("FcBytesView", 16, 8);
    assert_field_offset("FcBytesView.data", offset_of!(FcBytesView, data), 0);
    assert_field_offset("FcBytesView.len", offset_of!(FcBytesView, len), 8);

    assert_layout::<FcStrView>("FcStrView", 16, 8);
    assert_field_offset("FcStrView.data", offset_of!(FcStrView, data), 0);
    assert_field_offset("FcStrView.len", offset_of!(FcStrView, len), 8);

    assert_layout::<FcSemanticVersion>("FcSemanticVersion", 16, 4);
    assert_field_offset(
        "FcSemanticVersion.major",
        offset_of!(FcSemanticVersion, major),
        0,
    );
    assert_field_offset(
        "FcSemanticVersion.minor",
        offset_of!(FcSemanticVersion, minor),
        4,
    );
    assert_field_offset(
        "FcSemanticVersion.patch",
        offset_of!(FcSemanticVersion, patch),
        8,
    );
    assert_field_offset(
        "FcSemanticVersion.reserved",
        offset_of!(FcSemanticVersion, reserved),
        12,
    );

    assert_layout::<FcPlayerIdV1>("FcPlayerIdV1", 16, 1);
    assert_field_offset("FcPlayerIdV1.bytes", offset_of!(FcPlayerIdV1, bytes), 0);

    assert_layout::<FcBlockPosV1>("FcBlockPosV1", 12, 4);
    assert_field_offset("FcBlockPosV1.x", offset_of!(FcBlockPosV1, x), 0);
    assert_field_offset("FcBlockPosV1.y", offset_of!(FcBlockPosV1, y), 4);
    assert_field_offset("FcBlockPosV1.z", offset_of!(FcBlockPosV1, z), 8);
}

#[test]
fn envelope_records_have_exact_64_bit_layout() {
    assert_layout::<FcOutputBufferV1>("FcOutputBufferV1", 32, 8);
    assert_field_offset(
        "FcOutputBufferV1.header",
        offset_of!(FcOutputBufferV1, header),
        0,
    );
    assert_field_offset(
        "FcOutputBufferV1.data",
        offset_of!(FcOutputBufferV1, data),
        8,
    );
    assert_field_offset(
        "FcOutputBufferV1.capacity",
        offset_of!(FcOutputBufferV1, capacity),
        16,
    );
    assert_field_offset(
        "FcOutputBufferV1.result_len",
        offset_of!(FcOutputBufferV1, result_len),
        24,
    );

    assert_layout::<FcEventV1>("FcEventV1", 48, 8);
    assert_field_offset("FcEventV1.header", offset_of!(FcEventV1, header), 0);
    assert_field_offset("FcEventV1.kind", offset_of!(FcEventV1, kind), 8);
    assert_field_offset("FcEventV1.flags", offset_of!(FcEventV1, flags), 12);
    assert_field_offset("FcEventV1.tick", offset_of!(FcEventV1, tick), 16);
    assert_field_offset("FcEventV1.shard", offset_of!(FcEventV1, shard), 24);
    assert_field_offset("FcEventV1.payload", offset_of!(FcEventV1, payload), 32);

    assert_layout::<FcCommandV1>("FcCommandV1", 40, 8);
    assert_field_offset("FcCommandV1.header", offset_of!(FcCommandV1, header), 0);
    assert_field_offset("FcCommandV1.kind", offset_of!(FcCommandV1, kind), 8);
    assert_field_offset("FcCommandV1.flags", offset_of!(FcCommandV1, flags), 12);
    assert_field_offset("FcCommandV1.target", offset_of!(FcCommandV1, target), 16);
    assert_field_offset("FcCommandV1.payload", offset_of!(FcCommandV1, payload), 24);

    assert_layout::<FcHostRequestV1>("FcHostRequestV1", 40, 8);
    assert_field_offset(
        "FcHostRequestV1.header",
        offset_of!(FcHostRequestV1, header),
        0,
    );
    assert_field_offset("FcHostRequestV1.kind", offset_of!(FcHostRequestV1, kind), 8);
    assert_field_offset(
        "FcHostRequestV1.flags",
        offset_of!(FcHostRequestV1, flags),
        12,
    );
    assert_field_offset(
        "FcHostRequestV1.target",
        offset_of!(FcHostRequestV1, target),
        16,
    );
    assert_field_offset(
        "FcHostRequestV1.payload",
        offset_of!(FcHostRequestV1, payload),
        24,
    );
}

#[test]
fn versioned_payload_records_have_exact_layout() {
    assert_layout::<FcBlockBreakEventPayloadV1>("FcBlockBreakEventPayloadV1", 36, 4);
    assert_field_offset(
        "FcBlockBreakEventPayloadV1.header",
        offset_of!(FcBlockBreakEventPayloadV1, header),
        0,
    );
    assert_field_offset(
        "FcBlockBreakEventPayloadV1.player",
        offset_of!(FcBlockBreakEventPayloadV1, player),
        8,
    );
    assert_field_offset(
        "FcBlockBreakEventPayloadV1.pos",
        offset_of!(FcBlockBreakEventPayloadV1, pos),
        24,
    );

    assert_layout::<FcSetBlockCommandPayloadV1>("FcSetBlockCommandPayloadV1", 24, 4);
    assert_field_offset(
        "FcSetBlockCommandPayloadV1.header",
        offset_of!(FcSetBlockCommandPayloadV1, header),
        0,
    );
    assert_field_offset(
        "FcSetBlockCommandPayloadV1.pos",
        offset_of!(FcSetBlockCommandPayloadV1, pos),
        8,
    );
    assert_field_offset(
        "FcSetBlockCommandPayloadV1.block_state_id",
        offset_of!(FcSetBlockCommandPayloadV1, block_state_id),
        20,
    );

    assert_layout::<FcBlockStateRequestPayloadV1>("FcBlockStateRequestPayloadV1", 20, 4);
    assert_field_offset(
        "FcBlockStateRequestPayloadV1.header",
        offset_of!(FcBlockStateRequestPayloadV1, header),
        0,
    );
    assert_field_offset(
        "FcBlockStateRequestPayloadV1.pos",
        offset_of!(FcBlockStateRequestPayloadV1, pos),
        8,
    );
}

#[test]
fn function_tables_and_descriptor_have_exact_64_bit_layout() {
    assert_layout::<FcPluginFunctionsV1>("FcPluginFunctionsV1", 32, 8);
    assert_field_offset(
        "FcPluginFunctionsV1.header",
        offset_of!(FcPluginFunctionsV1, header),
        0,
    );
    assert_field_offset(
        "FcPluginFunctionsV1.init",
        offset_of!(FcPluginFunctionsV1, init),
        8,
    );
    assert_field_offset(
        "FcPluginFunctionsV1.on_event",
        offset_of!(FcPluginFunctionsV1, on_event),
        16,
    );
    assert_field_offset(
        "FcPluginFunctionsV1.shutdown",
        offset_of!(FcPluginFunctionsV1, shutdown),
        24,
    );

    assert_layout::<FcHostFunctionsV1>("FcHostFunctionsV1", 32, 8);
    assert_field_offset(
        "FcHostFunctionsV1.header",
        offset_of!(FcHostFunctionsV1, header),
        0,
    );
    assert_field_offset(
        "FcHostFunctionsV1.call",
        offset_of!(FcHostFunctionsV1, call),
        8,
    );
    assert_field_offset(
        "FcHostFunctionsV1.emit",
        offset_of!(FcHostFunctionsV1, emit),
        16,
    );
    assert_field_offset(
        "FcHostFunctionsV1.diagnostic",
        offset_of!(FcHostFunctionsV1, diagnostic),
        24,
    );

    assert_layout::<FcPluginDescriptorV1>("FcPluginDescriptorV1", 64, 8);
    assert_field_offset(
        "FcPluginDescriptorV1.header",
        offset_of!(FcPluginDescriptorV1, header),
        0,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.version",
        offset_of!(FcPluginDescriptorV1, version),
        8,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.requested_capabilities",
        offset_of!(FcPluginDescriptorV1, requested_capabilities),
        24,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.id",
        offset_of!(FcPluginDescriptorV1, id),
        32,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.name",
        offset_of!(FcPluginDescriptorV1, name),
        40,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.target",
        offset_of!(FcPluginDescriptorV1, target),
        48,
    );
    assert_field_offset(
        "FcPluginDescriptorV1.functions",
        offset_of!(FcPluginDescriptorV1, functions),
        56,
    );
}

#[test]
fn transparent_scalars_have_exact_layout_and_values() {
    assert_layout::<FcStatus>("FcStatus", 4, 4);
    assert_layout::<FcEventKind>("FcEventKind", 4, 4);
    assert_layout::<FcCommandKind>("FcCommandKind", 4, 4);
    assert_layout::<FcHostRequestKind>("FcHostRequestKind", 4, 4);
    assert_layout::<FcHostHandle>("FcHostHandle", 8, 8);
    assert_layout::<FcPluginHandle>("FcPluginHandle", 8, 8);
    assert_layout::<FcCallHandle>("FcCallHandle", 8, 8);
    assert_layout::<FcResourceHandle>("FcResourceHandle", 8, 8);

    assert_eq!(FC_OK.code(), 0);
    assert_eq!(FC_ERROR.code(), 1);
    assert_eq!(FC_PLUGIN_PANIC.code(), 2);
    assert_eq!(FC_CAPABILITY_DENIED.code(), 3);
    assert_eq!(FC_INVALID_ARGUMENT.code(), 4);
    assert_eq!(FC_BUFFER_TOO_SMALL.code(), 5);
    assert_eq!(FC_COMMAND_BUFFER_FULL.code(), 6);
    assert_eq!(FcStatus::from_code(i32::MIN).code(), i32::MIN);
    assert_eq!(FcEventKind::BLOCK_BREAK.raw(), 3);
    assert_eq!(FcCommandKind::SET_BLOCK.raw(), 1);
    assert_eq!(FcHostRequestKind::BLOCK_STATE.raw(), 3);
}

#[test]
fn event_command_and_request_kind_assignments_are_exact() {
    let events = [
        (FcEventKind::PLAYER_JOIN, 1),
        (FcEventKind::PLAYER_LEAVE, 2),
        (FcEventKind::BLOCK_BREAK, 3),
        (FcEventKind::AFTER_BLOCK_PLACE, 4),
        (FcEventKind::AFTER_BLOCK_BREAK, 5),
        (FcEventKind::PLAYER_MOVE, 6),
        (FcEventKind::BLOCK_PLACE_ATTEMPT, 7),
        (FcEventKind::BLOCK_BREAK_ATTEMPT, 8),
        (FcEventKind::CHAT_ATTEMPT, 9),
        (FcEventKind::INTERACT_ATTEMPT, 10),
        (FcEventKind::COMMAND, 11),
        (FcEventKind::TIMER, 12),
    ];
    let commands = [
        (FcCommandKind::SET_BLOCK, 1),
        (FcCommandKind::TELEPORT, 2),
        (FcCommandKind::MESSAGE, 3),
        (FcCommandKind::SUBSCRIBE_EVENT, 4),
        (FcCommandKind::REGISTER_COMMAND, 5),
        (FcCommandKind::STORAGE_PUT, 6),
        (FcCommandKind::STORAGE_DELETE, 7),
        (FcCommandKind::SCHEDULE_TIMER, 8),
        (FcCommandKind::CANCEL_TIMER, 9),
        (FcCommandKind::DECISION_ALLOW, 10),
        (FcCommandKind::DECISION_DENY, 11),
        (FcCommandKind::DECISION_REPLACE_BLOCK, 12),
    ];
    let requests = [
        (FcHostRequestKind::DIMENSION, 1),
        (FcHostRequestKind::CHUNK_LOADED, 2),
        (FcHostRequestKind::BLOCK_STATE, 3),
        (FcHostRequestKind::PLAYER_POSITION, 4),
        (FcHostRequestKind::PERMISSION_RESOLVE, 5),
        (FcHostRequestKind::STORAGE_GET, 6),
        (FcHostRequestKind::STORAGE_KEYS, 7),
    ];

    for (kind, expected) in events {
        assert_eq!(kind.raw(), expected);
    }
    for (kind, expected) in commands {
        assert_eq!(kind.raw(), expected);
    }
    for (kind, expected) in requests {
        assert_eq!(kind.raw(), expected);
    }
}

#[test]
fn capability_flag_and_diagnostic_assignments_are_exact() {
    assert_eq!(crate::FC_CAPABILITY_READ_WORLD, 1 << 0);
    assert_eq!(crate::FC_CAPABILITY_SUBMIT_INTENTS, 1 << 1);
    assert_eq!(crate::FC_CAPABILITY_REGISTER_COMMANDS, 1 << 2);
    assert_eq!(crate::FC_CAPABILITY_RECEIVE_EVENTS, 1 << 3);
    assert_eq!(crate::FC_CAPABILITY_READ_PERMISSIONS, 1 << 4);
    assert_eq!(crate::FC_CAPABILITY_STORAGE, 1 << 5);
    assert_eq!(crate::FC_CAPABILITY_VETO_BLOCK_EDITS, 1 << 6);
    assert_eq!(crate::FC_CAPABILITY_VETO_EVENTS, 1 << 7);
    assert_eq!(crate::FC_CAPABILITIES_V1, 0xff);

    assert_eq!(crate::FC_EVENT_FLAGS_NONE, 0);
    assert_eq!(crate::FC_COMMAND_FLAGS_NONE, 0);
    assert_eq!(crate::FC_HOST_REQUEST_FLAGS_NONE, 0);

    assert_eq!(crate::FC_DIAGNOSTIC_ERROR, 1);
    assert_eq!(crate::FC_DIAGNOSTIC_WARN, 2);
    assert_eq!(crate::FC_DIAGNOSTIC_INFO, 3);
    assert_eq!(crate::FC_DIAGNOSTIC_DEBUG, 4);
    assert_eq!(crate::FC_DIAGNOSTIC_TRACE, 5);
}

#[test]
fn associated_sizes_match_layout_and_callback_offsets() {
    assert_eq!(FcAbiHeader::BYTE_SIZE, 8);
    assert_eq!(FcOutputBufferV1::STRUCT_SIZE, 32);
    assert_eq!(FcEventV1::STRUCT_SIZE, 48);
    assert_eq!(FcCommandV1::STRUCT_SIZE, 40);
    assert_eq!(FcHostRequestV1::STRUCT_SIZE, 40);
    assert_eq!(FcBlockBreakEventPayloadV1::STRUCT_SIZE, 36);
    assert_eq!(FcSetBlockCommandPayloadV1::STRUCT_SIZE, 24);
    assert_eq!(FcBlockStateRequestPayloadV1::STRUCT_SIZE, 20);
    assert_eq!(FcPluginFunctionsV1::STRUCT_SIZE, 32);
    assert_eq!(FcHostFunctionsV1::STRUCT_SIZE, 32);
    assert_eq!(FcPluginDescriptorV1::STRUCT_SIZE, 64);

    assert_eq!(FcPluginFunctionsV1::INIT_OFFSET, 8);
    assert_eq!(FcPluginFunctionsV1::ON_EVENT_OFFSET, 16);
    assert_eq!(FcPluginFunctionsV1::SHUTDOWN_OFFSET, 24);
    assert_eq!(FcHostFunctionsV1::CALL_OFFSET, 8);
    assert_eq!(FcHostFunctionsV1::EMIT_OFFSET, 16);
    assert_eq!(FcHostFunctionsV1::DIAGNOSTIC_OFFSET, 24);
    assert_eq!(FcPluginDescriptorV1::ID_OFFSET, 32);
    assert_eq!(FcPluginDescriptorV1::NAME_OFFSET, 40);
    assert_eq!(FcPluginDescriptorV1::TARGET_OFFSET, 48);
    assert_eq!(FcPluginDescriptorV1::FUNCTIONS_OFFSET, 56);
}

#[test]
fn struct_size_round_trips_for_every_extensible_record() {
    let host_functions = FcHostFunctionsV1::new(host_call, host_emit, host_diagnostic);
    let plugin_table = FcPluginFunctionsV1::new(init, on_event, shutdown);
    let descriptor = FcPluginDescriptorV1::new(
        FcSemanticVersion::new(2, 7, 4),
        0xA5,
        text,
        text,
        text,
        plugin_functions_ptr,
    );
    let mut output_bytes = [0_u8; 8];
    let mut output = FcOutputBufferV1::new(output_bytes.as_mut_ptr(), 8);
    let event = FcEventV1::new(
        FcEventKind::from_raw(9),
        3,
        42,
        FcResourceHandle::from_raw(7),
        FcBytesView::empty(),
    );
    let command = FcCommandV1::new(
        FcCommandKind::from_raw(11),
        5,
        FcResourceHandle::from_raw(8),
        FcBytesView::empty(),
    );
    let request = FcHostRequestV1::new(
        FcHostRequestKind::BLOCK_STATE,
        0,
        FcResourceHandle::from_raw(9),
        FcBytesView::empty(),
    );
    let player = FcPlayerIdV1::new([0xAB; 16]);
    let pos = FcBlockPosV1::new(-3, 64, 7);
    let block_event = FcBlockBreakEventPayloadV1::new(player, pos);
    let set_block = FcSetBlockCommandPayloadV1::new(pos, 42);
    let block_query = FcBlockStateRequestPayloadV1::new(pos);

    let declared_sizes = [
        (output.header().struct_size(), FcOutputBufferV1::STRUCT_SIZE),
        (event.header().struct_size(), FcEventV1::STRUCT_SIZE),
        (command.header().struct_size(), FcCommandV1::STRUCT_SIZE),
        (request.header().struct_size(), FcHostRequestV1::STRUCT_SIZE),
        (
            block_event.header().struct_size(),
            FcBlockBreakEventPayloadV1::STRUCT_SIZE,
        ),
        (
            set_block.header().struct_size(),
            FcSetBlockCommandPayloadV1::STRUCT_SIZE,
        ),
        (
            block_query.header().struct_size(),
            FcBlockStateRequestPayloadV1::STRUCT_SIZE,
        ),
        (
            plugin_table.header().struct_size(),
            FcPluginFunctionsV1::STRUCT_SIZE,
        ),
        (
            host_functions.header().struct_size(),
            FcHostFunctionsV1::STRUCT_SIZE,
        ),
        (
            descriptor.header().struct_size(),
            FcPluginDescriptorV1::STRUCT_SIZE,
        ),
    ];

    for (declared, expected) in declared_sizes {
        assert_eq!(declared, expected);
    }
    assert_eq!(descriptor.header().abi_major(), ABI_MAJOR);
    assert_eq!(descriptor.header().abi_minor(), ABI_MINOR);
    assert_eq!(descriptor.version(), FcSemanticVersion::new(2, 7, 4));
    assert_eq!(block_event.player().bytes(), [0xAB; 16]);
    assert_eq!(block_event.pos(), pos);
    assert_eq!(set_block.block_state_id(), 42);
    assert_eq!(output.result_len(), 0);
    output.set_result_len(4);
    assert_eq!(output.result_len(), 4);
}

#[test]
fn size_prefix_accepts_exact_and_larger_but_rejects_short() {
    let exact = FcAbiHeader::new(32, ABI_MAJOR, ABI_MINOR);
    let short = FcAbiHeader::new(31, ABI_MAJOR, ABI_MINOR);
    let extended = FcAbiHeader::new(96, ABI_MAJOR, ABI_MINOR);

    assert!(exact.covers(32));
    assert!(!short.covers(32));
    assert!(extended.covers(32));
}

#[test]
#[should_panic(expected = "FcEventV1.payload offset drifted from its ABI commitment")]
fn deliberately_wrong_offset_fails_the_layout_guard() {
    assert_field_offset("FcEventV1.payload", offset_of!(FcEventV1, payload), 33);
}

#[test]
fn opaque_and_kind_values_round_trip_without_interpretation() {
    assert_eq!(FcHostHandle::from_raw(17).raw(), 17);
    assert_eq!(FcPluginHandle::from_raw(18).raw(), 18);
    assert_eq!(FcCallHandle::from_raw(19).raw(), 19);
    assert_eq!(FcResourceHandle::from_raw(20).raw(), 20);
    assert_eq!(FcEventKind::from_raw(u32::MAX).raw(), u32::MAX);
    assert_eq!(FcCommandKind::from_raw(u32::MAX).raw(), u32::MAX);
    assert_eq!(FcHostRequestKind::from_raw(u32::MAX).raw(), u32::MAX);
    assert!(!FcHostHandle::INVALID.is_valid());
    assert!(FcHostHandle::from_raw(1).is_valid());
    assert_eq!(AbiVersion::new(1, 3).to_string(), "1.3");
}

#[test]
fn descriptor_and_table_are_safe_immutable_statics() {
    assert_entry_type(plugin_entry);
    assert!(!plugin_entry().is_null());
    assert_eq!(PLUGIN_DESCRIPTOR.version(), FcSemanticVersion::new(1, 2, 3));
    assert_eq!(
        crate::ENTRYPOINT_V1.to_bytes_with_nul(),
        b"ferrumc_plugin_entry_v1\0"
    );
}

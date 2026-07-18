//! Test-only invalid ABI records for the Packet 52 robustness battery.

#![forbid(unsafe_code)]

const fn enabled(value: bool) -> usize {
    if value {
        1
    } else {
        0
    }
}

const _: () = assert!(
    enabled(cfg!(feature = "short-function-table"))
        + enabled(cfg!(feature = "missing-entrypoint"))
        + enabled(cfg!(feature = "missing-functions-value"))
        + enabled(cfg!(feature = "missing-init-callback"))
        + enabled(cfg!(feature = "missing-event-callback"))
        + enabled(cfg!(feature = "missing-shutdown-callback"))
        + enabled(cfg!(feature = "missing-metadata-buffer"))
        + enabled(cfg!(feature = "excess-declared-length"))
        == 1,
    "enable exactly one ABI invalid-input feature"
);

#[cfg(not(feature = "missing-entrypoint"))]
mod invalid_input {
    use std::ptr;

    use ferrumc_plugin_abi::{
        FcAbiHeader, FcCallHandle, FcEventV1, FcHostFunctionsV1, FcHostHandle, FcOutputBufferV1,
        FcPluginDescriptorV1, FcPluginFunctionsV1, FcPluginHandle, FcPluginInitFn,
        FcPluginOnEventFn, FcPluginShutdownFn, FcSemanticVersion, FcStatus, FcStrView, FC_OK,
    };
    use ferrumc_plugin_abi_sys::{
        export_plugin_v1, PluginBridge, PluginCall, PluginEvent as BridgeEvent,
    };

    const FIXTURE_ID: &[u8; 21] = b"p52-abi-invalid-input";
    const FIXTURE_NAME: &[u8; 27] = b"Packet 52 ABI Invalid Input";
    const FIXTURE_TARGET: &[u8] = env!("FERRUMC_TESTKIT_INVALID_INPUT_TARGET").as_bytes();
    const EXCESS_DECLARED_LENGTH: u64 = 4_097;

    #[repr(C)]
    struct RawFunctionTable {
        header: FcAbiHeader,
        init: Option<FcPluginInitFn>,
        on_event: Option<FcPluginOnEventFn>,
        shutdown: Option<FcPluginShutdownFn>,
    }

    const _: () = {
        assert!(std::mem::size_of::<RawFunctionTable>() == 32);
        assert!(
            std::mem::align_of::<RawFunctionTable>() == std::mem::align_of::<FcPluginFunctionsV1>()
        );
        assert!(FcPluginFunctionsV1::STRUCT_SIZE == 32);
    };

    static SHORT_FUNCTION_TABLE: RawFunctionTable = RawFunctionTable {
        header: FcAbiHeader::current(FcAbiHeader::BYTE_SIZE),
        init: None,
        on_event: None,
        shutdown: None,
    };

    static MISSING_INIT_FUNCTION_TABLE: RawFunctionTable = RawFunctionTable {
        header: FcAbiHeader::current(FcPluginFunctionsV1::STRUCT_SIZE),
        init: None,
        on_event: Some(on_event),
        shutdown: Some(shutdown),
    };

    static MISSING_EVENT_FUNCTION_TABLE: RawFunctionTable = RawFunctionTable {
        header: FcAbiHeader::current(FcPluginFunctionsV1::STRUCT_SIZE),
        init: Some(init),
        on_event: None,
        shutdown: Some(shutdown),
    };

    static MISSING_SHUTDOWN_FUNCTION_TABLE: RawFunctionTable = RawFunctionTable {
        header: FcAbiHeader::current(FcPluginFunctionsV1::STRUCT_SIZE),
        init: Some(init),
        on_event: Some(on_event),
        shutdown: None,
    };

    static VALID_FUNCTION_TABLE: FcPluginFunctionsV1 =
        FcPluginFunctionsV1::new(init, on_event, shutdown);

    static DESCRIPTOR: FcPluginDescriptorV1 = FcPluginDescriptorV1::new(
        FcSemanticVersion::new(1, 0, 0),
        0,
        fixture_id,
        fixture_name,
        fixture_target,
        fixture_functions,
    );

    struct InvalidInputBridge;

    impl PluginBridge for InvalidInputBridge {
        type Instance = ();

        const ID: &'static str = "p52-abi-invalid-input";
        const NAME: &'static str = "Packet 52 ABI Invalid Input";
        const TARGET: &'static str = env!("FERRUMC_TESTKIT_INVALID_INPUT_TARGET");
        const VERSION: FcSemanticVersion = FcSemanticVersion::new(1, 0, 0);
        const REQUESTED_CAPABILITIES: u64 = 0;

        fn functions() -> &'static FcPluginFunctionsV1 {
            &VALID_FUNCTION_TABLE
        }

        fn descriptor() -> &'static FcPluginDescriptorV1 {
            &DESCRIPTOR
        }

        fn initialize(
            _call: &mut PluginCall<'_>,
            _granted_capabilities: u64,
        ) -> Result<Self::Instance, FcStatus> {
            Ok(())
        }

        fn on_event(
            _instance: &mut Self::Instance,
            _call: &mut PluginCall<'_>,
            _event: BridgeEvent<'_>,
        ) -> FcStatus {
            FC_OK
        }

        fn shutdown(_instance: Self::Instance, _call: &mut PluginCall<'_>) -> FcStatus {
            FC_OK
        }
    }

    extern "C" fn fixture_id() -> FcStrView {
        if cfg!(feature = "missing-metadata-buffer") {
            FcStrView::new(ptr::null(), 1)
        } else if cfg!(feature = "excess-declared-length") {
            FcStrView::new(FIXTURE_ID.as_ptr(), EXCESS_DECLARED_LENGTH)
        } else {
            FcStrView::new(FIXTURE_ID.as_ptr(), 21)
        }
    }

    extern "C" fn fixture_name() -> FcStrView {
        FcStrView::new(FIXTURE_NAME.as_ptr(), 27)
    }

    extern "C" fn fixture_target() -> FcStrView {
        match u64::try_from(FIXTURE_TARGET.len()) {
            Ok(length) => FcStrView::new(FIXTURE_TARGET.as_ptr(), length),
            Err(_) => FcStrView::empty(),
        }
    }

    extern "C" fn fixture_functions() -> *const FcPluginFunctionsV1 {
        if cfg!(feature = "missing-functions-value") {
            ptr::null()
        } else if cfg!(feature = "short-function-table") {
            ptr::from_ref(&SHORT_FUNCTION_TABLE).cast()
        } else if cfg!(feature = "missing-init-callback") {
            ptr::from_ref(&MISSING_INIT_FUNCTION_TABLE).cast()
        } else if cfg!(feature = "missing-event-callback") {
            ptr::from_ref(&MISSING_EVENT_FUNCTION_TABLE).cast()
        } else if cfg!(feature = "missing-shutdown-callback") {
            ptr::from_ref(&MISSING_SHUTDOWN_FUNCTION_TABLE).cast()
        } else {
            ptr::from_ref(&VALID_FUNCTION_TABLE)
        }
    }

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

    export_plugin_v1!(InvalidInputBridge);
}

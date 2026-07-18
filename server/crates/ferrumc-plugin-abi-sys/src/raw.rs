use std::mem;
use std::ptr;
use std::slice;

use ferrumc_plugin_abi::{
    negotiate_current, AbiVersion, FcAbiHeader, FcPluginDescriptorV1, FcPluginFunctionsFn,
    FcPluginFunctionsV1, FcPluginInitFn, FcPluginOnEventFn, FcPluginShutdownFn, FcStrFn,
};

use crate::error::{AbiRecord, ValidationError};

/// Metadata is tiny by contract; this cap prevents a declaration from driving
/// an excessive allocation before the manifest layer applies field semantics.
pub(crate) const MAX_METADATA_BYTES: usize = 4 * 1024;

const _: () = {
    assert!(mem::size_of::<usize>() == 8);
    assert!(mem::size_of::<FcStrFn>() == mem::size_of::<usize>());
    assert!(mem::size_of::<FcPluginFunctionsFn>() == mem::size_of::<usize>());
    assert!(mem::size_of::<FcPluginInitFn>() == mem::size_of::<usize>());
    assert!(mem::size_of::<FcPluginOnEventFn>() == mem::size_of::<usize>());
    assert!(mem::size_of::<FcPluginShutdownFn>() == mem::size_of::<usize>());
    assert!(FcPluginDescriptorV1::STRUCT_SIZE == 64);
    assert!(FcPluginDescriptorV1::FUNCTIONS_OFFSET + mem::size_of::<usize>() == 64);
    assert!(FcPluginFunctionsV1::STRUCT_SIZE == 32);
    assert!(FcPluginFunctionsV1::SHUTDOWN_OFFSET + mem::size_of::<usize>() == 32);
};

pub(crate) fn validate_descriptor(
    descriptor: *const FcPluginDescriptorV1,
) -> Result<(FcPluginDescriptorV1, AbiVersion), ValidationError> {
    if descriptor.is_null() {
        return Err(ValidationError::NullDescriptor);
    }
    ensure_alignment(descriptor, AbiRecord::Descriptor)?;

    let base = descriptor.cast::<u8>();
    let (header, abi_version) = validate_common_header(base, AbiRecord::Descriptor)?;
    ensure_prefix(
        header,
        AbiRecord::Descriptor,
        FcPluginDescriptorV1::STRUCT_SIZE,
    )?;
    validate_required_slots(
        base,
        header,
        AbiRecord::Descriptor,
        &[
            (FcPluginDescriptorV1::ID_OFFSET, "id"),
            (FcPluginDescriptorV1::NAME_OFFSET, "name"),
            (FcPluginDescriptorV1::TARGET_OFFSET, "target"),
            (FcPluginDescriptorV1::FUNCTIONS_OFFSET, "functions"),
        ],
    )?;

    // SAFETY: the operator-trusted bootstrap pointer is non-null and live, the
    // declared prefix covers the complete v1 descriptor, and every function
    // pointer word was checked for null and the trusted callable-address
    // contract applies before this aligned typed read.
    let descriptor = unsafe { ptr::read(descriptor) };
    let reserved = descriptor.version().reserved();
    if reserved != 0 {
        return Err(ValidationError::NonZeroSemanticVersionReserved { value: reserved });
    }

    Ok((descriptor, abi_version))
}

pub(crate) fn validate_function_table(
    functions: *const FcPluginFunctionsV1,
    descriptor_version: AbiVersion,
) -> Result<FcPluginFunctionsV1, ValidationError> {
    if functions.is_null() {
        return Err(ValidationError::NullFunctionTable);
    }
    ensure_alignment(functions, AbiRecord::FunctionTable)?;

    let base = functions.cast::<u8>();
    let (header, function_table_version) = validate_common_header(base, AbiRecord::FunctionTable)?;
    if function_table_version != descriptor_version {
        return Err(ValidationError::FunctionTableVersionMismatch {
            descriptor: descriptor_version,
            function_table: function_table_version,
        });
    }
    ensure_prefix(
        header,
        AbiRecord::FunctionTable,
        FcPluginFunctionsV1::STRUCT_SIZE,
    )?;
    validate_required_slots(
        base,
        header,
        AbiRecord::FunctionTable,
        &[
            (FcPluginFunctionsV1::INIT_OFFSET, "init"),
            (FcPluginFunctionsV1::ON_EVENT_OFFSET, "on_event"),
            (FcPluginFunctionsV1::SHUTDOWN_OFFSET, "shutdown"),
        ],
    )?;

    // SAFETY: the operator-trusted getter returned a non-null live pointer, the
    // declared prefix covers the complete v1 table, and every required callback
    // word was checked for null and the trusted callable-address contract
    // applies before this aligned typed read.
    Ok(unsafe { ptr::read(functions) })
}

pub(crate) fn copy_metadata(
    getter: FcStrFn,
    field: &'static str,
) -> Result<String, ValidationError> {
    // SAFETY: `getter` was copied only after its raw descriptor slot was
    // non-null, its library is permanently resident, and the operator-trusted
    // implementation promises this exact signature and no unwind.
    let view = unsafe { getter() };
    let declared = view.len();
    let length = usize::try_from(declared)
        .map_err(|_| ValidationError::MetadataLengthNotRepresentable { field, declared })?;
    if length > MAX_METADATA_BYTES {
        return Err(ValidationError::MetadataTooLong {
            field,
            declared,
            maximum: MAX_METADATA_BYTES,
        });
    }
    if length == 0 {
        return Ok(String::new());
    }
    if view.data().is_null() {
        return Err(ValidationError::NullMetadataPointer { field });
    }
    if length > isize::MAX.unsigned_abs() {
        return Err(ValidationError::MetadataLengthNotRepresentable { field, declared });
    }
    if (view.data() as usize).checked_add(length).is_none() {
        return Err(ValidationError::MetadataAddressOverflow { field });
    }

    // SAFETY: the length is nonzero, representable, capped at 4096, and checked
    // against both `isize::MAX` and address wrap before pointer access. The
    // operator-trusted getter contract guarantees that the non-null byte extent
    // remains readable until the next getter call.
    let borrowed = unsafe { slice::from_raw_parts(view.data(), length) };
    let owned = borrowed.to_vec();
    String::from_utf8(owned).map_err(|_| ValidationError::InvalidMetadataUtf8 { field })
}

fn validate_common_header(
    base: *const u8,
    record: AbiRecord,
) -> Result<(FcAbiHeader, AbiVersion), ValidationError> {
    // SAFETY: callers reject null and misalignment first. The operator-trusted
    // ABI contract guarantees that a returned record pointer covers the leading
    // size word, whose integer type accepts every bit pattern.
    let struct_size = unsafe { ptr::read_unaligned(base.cast::<u32>()) };
    if struct_size < FcAbiHeader::BYTE_SIZE {
        return Err(ValidationError::RecordTooShort {
            record,
            declared: struct_size,
            required: FcAbiHeader::BYTE_SIZE,
        });
    }

    // SAFETY: `struct_size` now declares the complete eight-byte common header,
    // and the trusted record contract supplies readable immutable bytes for it.
    let abi_major = unsafe { ptr::read_unaligned(base.wrapping_add(4).cast::<u16>()) };
    // SAFETY: the validated common-header extent covers the two-byte minor word
    // at offset six, and the trusted record contract supplies readable bytes.
    let abi_minor = unsafe { ptr::read_unaligned(base.wrapping_add(6).cast::<u16>()) };
    let version = AbiVersion::new(abi_major, abi_minor);
    negotiate_current(version)
        .map_err(|source| ValidationError::IncompatibleAbi { record, source })?;
    let header = FcAbiHeader::new(struct_size, abi_major, abi_minor);
    Ok((header, version))
}

fn ensure_alignment<T>(pointer: *const T, record: AbiRecord) -> Result<(), ValidationError> {
    let required_alignment = mem::align_of::<T>();
    if (pointer as usize).checked_rem(required_alignment) != Some(0) {
        return Err(ValidationError::MisalignedRecord {
            record,
            required_alignment,
        });
    }
    Ok(())
}

fn ensure_prefix(
    header: FcAbiHeader,
    record: AbiRecord,
    required: u32,
) -> Result<(), ValidationError> {
    if !header.covers(required) {
        return Err(ValidationError::RecordTooShort {
            record,
            declared: header.struct_size(),
            required,
        });
    }
    Ok(())
}

fn validate_required_slots(
    base: *const u8,
    header: FcAbiHeader,
    record: AbiRecord,
    slots: &[(usize, &'static str)],
) -> Result<(), ValidationError> {
    for &(offset, name) in slots {
        let required = offset
            .checked_add(mem::size_of::<usize>())
            .and_then(|end| u32::try_from(end).ok())
            .ok_or(ValidationError::RecordTooShort {
                record,
                declared: header.struct_size(),
                required: u32::MAX,
            })?;
        ensure_prefix(header, record, required)?;

        // SAFETY: the version and declared prefix were validated before this
        // read, `required` proves this exact word lies within that prefix, and
        // the operator-trusted record pointer supplies readable provenance.
        let word = unsafe { ptr::read_unaligned(base.wrapping_add(offset).cast::<usize>()) };
        if word == 0 {
            return Err(ValidationError::NullRequiredSlot { record, slot: name });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use ferrumc_plugin_abi::{
        AbiVersionError, FcAbiHeader, FcCallHandle, FcEventV1, FcHostFunctionsV1, FcHostHandle,
        FcOutputBufferV1, FcPluginDescriptorV1, FcPluginFunctionsV1, FcPluginHandle,
        FcSemanticVersion, FcStatus, FcStrView, ABI_MAJOR, ABI_MINOR, FC_OK,
    };

    use super::{copy_metadata, validate_descriptor, validate_function_table, MAX_METADATA_BYTES};
    use crate::error::{AbiRecord, ValidationError};

    const ID: &[u8; 14] = b"fixture.plugin";
    const NAME: &[u8; 14] = b"Fixture Plugin";
    const TARGET: &[u8; 21] = b"aarch64-unknown-linux";

    unsafe extern "C" fn fixture_id() -> FcStrView {
        FcStrView::new(ID.as_ptr(), 14)
    }

    unsafe extern "C" fn fixture_name() -> FcStrView {
        FcStrView::new(NAME.as_ptr(), 14)
    }

    unsafe extern "C" fn fixture_target() -> FcStrView {
        FcStrView::new(TARGET.as_ptr(), 21)
    }

    unsafe extern "C" fn fixture_init(
        _host: FcHostHandle,
        _call: FcCallHandle,
        _host_functions: *const FcHostFunctionsV1,
        _granted_capabilities: u64,
        _output: *mut FcOutputBufferV1,
        _plugin_out: *mut FcPluginHandle,
    ) -> FcStatus {
        FC_OK
    }

    unsafe extern "C" fn fixture_on_event(
        _host: FcHostHandle,
        _plugin: FcPluginHandle,
        _call: FcCallHandle,
        _host_functions: *const FcHostFunctionsV1,
        _event: *const FcEventV1,
        _output: *mut FcOutputBufferV1,
    ) -> FcStatus {
        FC_OK
    }

    unsafe extern "C" fn fixture_shutdown(
        _host: FcHostHandle,
        _plugin: FcPluginHandle,
        _call: FcCallHandle,
        _host_functions: *const FcHostFunctionsV1,
        _output: *mut FcOutputBufferV1,
    ) -> FcStatus {
        FC_OK
    }

    static FUNCTIONS: FcPluginFunctionsV1 =
        FcPluginFunctionsV1::new(fixture_init, fixture_on_event, fixture_shutdown);

    unsafe extern "C" fn fixture_functions() -> *const FcPluginFunctionsV1 {
        ptr::from_ref(&FUNCTIONS)
    }

    static DESCRIPTOR: FcPluginDescriptorV1 = FcPluginDescriptorV1::new(
        FcSemanticVersion::new(2, 3, 5),
        7,
        fixture_id,
        fixture_name,
        fixture_target,
        fixture_functions,
    );

    #[repr(C, align(8))]
    struct RawRecord {
        bytes: [u8; 64],
    }

    impl RawRecord {
        fn with_header(struct_size: u32, major: u16, minor: u16) -> Self {
            let mut record = Self { bytes: [0; 64] };
            record.bytes[0..4].copy_from_slice(&struct_size.to_ne_bytes());
            record.bytes[4..6].copy_from_slice(&major.to_ne_bytes());
            record.bytes[6..8].copy_from_slice(&minor.to_ne_bytes());
            record
        }

        fn set_word(&mut self, offset: usize, word: usize) {
            let bytes = word.to_ne_bytes();
            self.bytes[offset..offset + bytes.len()].copy_from_slice(&bytes);
        }

        fn descriptor(&self) -> *const FcPluginDescriptorV1 {
            self.bytes.as_ptr().cast()
        }

        fn functions(&self) -> *const FcPluginFunctionsV1 {
            self.bytes.as_ptr().cast()
        }
    }

    #[test]
    fn descriptor_rejects_short_common_header_before_version() {
        let record = RawRecord::with_header(7, ABI_MAJOR.wrapping_add(1), ABI_MINOR);
        assert!(matches!(
            validate_descriptor(record.descriptor()),
            Err(ValidationError::RecordTooShort {
                record: AbiRecord::Descriptor,
                declared: 7,
                required: FcAbiHeader::BYTE_SIZE,
            })
        ));
    }

    #[test]
    fn descriptor_rejects_misalignment_before_reading_header() {
        let record =
            RawRecord::with_header(FcPluginDescriptorV1::STRUCT_SIZE, ABI_MAJOR, ABI_MINOR);
        let pointer = record.bytes.as_ptr().wrapping_add(1).cast();
        assert!(matches!(
            validate_descriptor(pointer),
            Err(ValidationError::MisalignedRecord {
                record: AbiRecord::Descriptor,
                ..
            })
        ));
    }

    #[test]
    fn descriptor_rejects_version_before_prefix_or_slots() {
        let record = RawRecord::with_header(
            FcPluginDescriptorV1::STRUCT_SIZE,
            ABI_MAJOR.wrapping_add(1),
            ABI_MINOR,
        );
        assert!(matches!(
            validate_descriptor(record.descriptor()),
            Err(ValidationError::IncompatibleAbi {
                record: AbiRecord::Descriptor,
                source: AbiVersionError::MajorMismatch { .. },
            })
        ));
    }

    #[test]
    fn descriptor_rejects_short_prefix_before_slot_words() {
        let record = RawRecord::with_header(FcAbiHeader::BYTE_SIZE, ABI_MAJOR, ABI_MINOR);
        assert!(matches!(
            validate_descriptor(record.descriptor()),
            Err(ValidationError::RecordTooShort {
                record: AbiRecord::Descriptor,
                declared: FcAbiHeader::BYTE_SIZE,
                required: FcPluginDescriptorV1::STRUCT_SIZE,
            })
        ));
    }

    #[test]
    fn descriptor_rejects_a_null_raw_required_slot() {
        let mut record =
            RawRecord::with_header(FcPluginDescriptorV1::STRUCT_SIZE, ABI_MAJOR, ABI_MINOR);
        record.set_word(FcPluginDescriptorV1::NAME_OFFSET, 1);
        record.set_word(FcPluginDescriptorV1::TARGET_OFFSET, 1);
        record.set_word(FcPluginDescriptorV1::FUNCTIONS_OFFSET, 1);
        assert!(matches!(
            validate_descriptor(record.descriptor()),
            Err(ValidationError::NullRequiredSlot {
                record: AbiRecord::Descriptor,
                slot: "id",
            })
        ));
    }

    #[test]
    fn function_table_rejects_a_null_raw_required_slot() {
        let mut record =
            RawRecord::with_header(FcPluginFunctionsV1::STRUCT_SIZE, ABI_MAJOR, ABI_MINOR);
        record.set_word(FcPluginFunctionsV1::INIT_OFFSET, 1);
        record.set_word(FcPluginFunctionsV1::SHUTDOWN_OFFSET, 1);
        assert!(matches!(
            validate_function_table(record.functions(), ferrumc_plugin_abi::CURRENT_ABI),
            Err(ValidationError::NullRequiredSlot {
                record: AbiRecord::FunctionTable,
                slot: "on_event",
            })
        ));
    }

    #[test]
    fn function_table_rejects_short_prefix_before_callback_slots() {
        let record = RawRecord::with_header(FcAbiHeader::BYTE_SIZE, ABI_MAJOR, ABI_MINOR);
        assert!(matches!(
            validate_function_table(record.functions(), ferrumc_plugin_abi::CURRENT_ABI),
            Err(ValidationError::RecordTooShort {
                record: AbiRecord::FunctionTable,
                declared: FcAbiHeader::BYTE_SIZE,
                required: FcPluginFunctionsV1::STRUCT_SIZE,
            })
        ));
    }

    #[test]
    fn function_table_rejects_wrong_major_before_prefix_or_callbacks() {
        let record =
            RawRecord::with_header(FcAbiHeader::BYTE_SIZE, ABI_MAJOR.wrapping_add(1), ABI_MINOR);
        assert!(matches!(
            validate_function_table(record.functions(), ferrumc_plugin_abi::CURRENT_ABI),
            Err(ValidationError::IncompatibleAbi {
                record: AbiRecord::FunctionTable,
                source: AbiVersionError::MajorMismatch { .. },
            })
        ));
    }

    #[test]
    fn valid_synthetic_records_and_metadata_are_owned() {
        let (descriptor, version) =
            validate_descriptor(ptr::from_ref(&DESCRIPTOR)).expect("valid descriptor");
        let functions = validate_function_table(ptr::from_ref(&FUNCTIONS), version)
            .expect("valid function table");

        assert_eq!(version, ferrumc_plugin_abi::CURRENT_ABI);
        assert_eq!(
            functions.header().struct_size(),
            FcPluginFunctionsV1::STRUCT_SIZE
        );
        assert_eq!(
            copy_metadata(descriptor.id(), "id").expect("valid id"),
            "fixture.plugin"
        );
        assert_eq!(
            copy_metadata(descriptor.name(), "name").expect("valid name"),
            "Fixture Plugin"
        );
        assert_eq!(
            copy_metadata(descriptor.target(), "target").expect("valid target"),
            "aarch64-unknown-linux"
        );
    }

    unsafe extern "C" fn oversized_metadata() -> FcStrView {
        static SENTINEL: u8 = 0;
        FcStrView::new(ptr::from_ref(&SENTINEL), 4097)
    }

    unsafe extern "C" fn null_metadata() -> FcStrView {
        FcStrView::new(ptr::null(), 1)
    }

    unsafe extern "C" fn invalid_utf8_metadata() -> FcStrView {
        static INVALID: [u8; 1] = [0xff];
        FcStrView::new(INVALID.as_ptr(), 1)
    }

    unsafe extern "C" fn wrapping_metadata() -> FcStrView {
        FcStrView::new(ptr::null::<u8>().wrapping_sub(1), 1)
    }

    #[test]
    fn metadata_is_bounded_before_pointer_access() {
        assert_eq!(MAX_METADATA_BYTES, 4096);
        assert!(matches!(
            copy_metadata(oversized_metadata, "id"),
            Err(ValidationError::MetadataTooLong {
                field: "id",
                declared: 4097,
                maximum: 4096,
            })
        ));
        assert!(matches!(
            copy_metadata(null_metadata, "name"),
            Err(ValidationError::NullMetadataPointer { field: "name" })
        ));
        assert!(matches!(
            copy_metadata(invalid_utf8_metadata, "target"),
            Err(ValidationError::InvalidMetadataUtf8 { field: "target" })
        ));
        assert!(matches!(
            copy_metadata(wrapping_metadata, "id"),
            Err(ValidationError::MetadataAddressOverflow { field: "id" })
        ));
    }
}

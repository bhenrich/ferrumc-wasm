//! Shared wire bounds, small encode helpers, and the `SetContainerContent`
//! payload builder.
//!
//! These limits bound the *serverbound* (untrusted) slot decoder so a hostile
//! creative packet cannot drive an oversized allocation; they reuse the same
//! philosophy as `ferrumc_codec`/`ferrumc_nbt` rather than inventing new
//! unbounded parsing.

use ferrumc_codec::write_var_int;
use ferrumc_nbt::NbtLimits;

use crate::stack::ItemStack;
use crate::untrusted::ItemValidationError;

/// Maximum number of component entries (added + removed) on one untrusted slot.
///
/// Vanilla items carry only a handful of components; 64 is a generous cap that
/// still bounds the per-slot work.
pub const MAX_COMPONENTS: usize = 64;

/// Maximum size of a single untrusted component's length-prefixed data blob.
pub const MAX_COMPONENT_BYTES: usize = 8192;

/// Maximum combined size of all component data blobs on one untrusted slot.
pub const MAX_COMPONENTS_TOTAL_BYTES: usize = 256 * 1024;

/// Maximum number of item slots a `SetContainerContent` body may carry.
///
/// The largest vanilla window (a double chest plus the player inventory) is
/// under 100 slots; 256 bounds the array without rejecting any real container.
pub const MAX_WINDOW_SLOTS: usize = 256;

/// The NBT decode/encode limits applied to NBT-valued components (`custom_data`,
/// `custom_name`). Uses the crate-wide [`NbtLimits`] defaults.
pub(crate) fn nbt_limits() -> NbtLimits {
    NbtLimits::default()
}

/// Writes a non-negative length/count as a `VarInt`.
///
/// Counts here are always bounded well below `i32::MAX` by the caller's caps, so
/// this saturates defensively rather than panicking on an impossible overflow.
pub(crate) fn write_count(out: &mut Vec<u8>, n: usize) {
    write_var_int(out, i32::try_from(n).unwrap_or(i32::MAX));
}

/// Builds the `SetContainerContent` (0x12) body as opaque `remaining_bytes`.
///
/// The body is: a `VarInt` slot count, then each slot as a *trusted* slot, then
/// the carried (cursor) item as a trusted slot. The proto layer carries this as
/// one opaque payload because `remaining_bytes` must be a packet's final field
/// and the items array is not last; all per-slot encoding therefore lives here.
///
/// Returns [`ItemValidationError::WindowTooLarge`] if `items` exceeds
/// [`MAX_WINDOW_SLOTS`], and propagates any NBT encoding error from a component.
///
/// # Examples
///
/// ```
/// use ferrumc_items::{encode_container_content_payload, ItemStack};
///
/// let body = encode_container_content_payload(&[ItemStack::empty()], &ItemStack::empty()).unwrap();
/// // count (1) + one empty slot (0) + carried empty slot (0).
/// assert_eq!(body, vec![1, 0, 0]);
/// ```
pub fn encode_container_content_payload(
    items: &[ItemStack],
    carried: &ItemStack,
) -> Result<Vec<u8>, ItemValidationError> {
    if items.len() > MAX_WINDOW_SLOTS {
        return Err(ItemValidationError::WindowTooLarge {
            count: items.len(),
            max: MAX_WINDOW_SLOTS,
        });
    }
    let mut out = Vec::new();
    write_count(&mut out, items.len());
    for item in items {
        item.encode_slot(&mut out)?;
    }
    carried.encode_slot(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_payload_layout() {
        let body = encode_container_content_payload(
            &[ItemStack::empty(), ItemStack::empty()],
            &ItemStack::empty(),
        )
        .unwrap();
        // count=2, slot0=0, slot1=0, carried=0.
        assert_eq!(body, vec![2, 0, 0, 0]);
    }

    #[test]
    fn container_payload_rejects_oversized_window() {
        let items = vec![ItemStack::empty(); MAX_WINDOW_SLOTS + 1];
        let err = encode_container_content_payload(&items, &ItemStack::empty()).unwrap_err();
        assert!(matches!(
            err,
            ItemValidationError::WindowTooLarge {
                count,
                max
            } if count == MAX_WINDOW_SLOTS + 1 && max == MAX_WINDOW_SLOTS
        ));
    }
}

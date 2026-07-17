# ferrumc-items

The shared item-stack model and slot wire codec for Minecraft Java 1.21.8
(protocol 772, data-component items).

This crate sits *below* the proto and sim lanes so neither has to depend on the
other for a common item type. The generated protocol packets carry every
slot-bearing field as opaque `remaining_bytes` (the same precedent as the
Brigadier `Commands` packet); this crate produces and consumes those bytes.

## What it provides

- `ItemId` — a registry-validated protocol item id (`id()`, `name()`,
  `max_stack()`, `placeable_block()`).
- `ItemStack` — the canonical, *trusted* item stack (clientbound form), with a
  registry-checked constructor and a fail-closed trusted slot encoder.
- `UntrustedItemStack` — the *untrusted* serverbound wire form (creative slot),
  with `decode` and `into_validated` for hostile-input normalization.
- `ComponentValue` / `ComponentPatch` / `ComponentTypeId` — the data-component
  model, with a few typed variants and an `Opaque` passthrough.
- `encode_container_content_payload` — builds the `SetContainerContent` body.

## Trusted vs untrusted slots

The two directions share the same outer framing (`itemCount` varint; if non-zero,
`itemId`, added/removed counts, the component arrays) but differ in **component
data framing**:

- **Trusted (clientbound)** component data is *typed and unprefixed* — a switch on
  the component type id (e.g. `max_stack_size` => varint, `unbreakable` => void,
  `custom_data` => network NBT). This is what the server emits.
- **Untrusted (serverbound)** component data is a *varint-length-prefixed blob*
  (`ByteArray`), so unknown or dangerous components are bounded, skippable, and
  strippable without parsing their internals — exactly what hostile-input
  handling needs.

They are therefore two separate, self-consistent codecs (each round-trips for its
own direction), **not** a cross-direction round trip.

## Trusted construction and emission

`ItemStack::try_new` is the checked path for data-driven counts and accepts only
values in `1..=ItemId::max_stack()`. The source-compatible `ItemStack::new`
retains its caller-validated `NonZeroU8` contract while callers migrate; it must
not be used as proof that the registry maximum was checked.

Every trusted `encode_slot` call revalidates the registry count before emission.
A `max_stack_size` component cannot raise that registry ceiling, and any count
or component-encoding error restores the output buffer to its original length.
Container-content encoding routes every list and carried stack through the same
check.

## Hostile-input normalization

`UntrustedItemStack::into_validated` rejects an unknown item id, clamps the count
to `1..=max_stack`, bounds the component count and total bytes, strips
`block_entity_data` (51), `container` (66), and out-of-range component types, and
validates NBT component data against `ferrumc_nbt::NbtLimits`. Anything malformed
returns an `Err` rather than panicking.

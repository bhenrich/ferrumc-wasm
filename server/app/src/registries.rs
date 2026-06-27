//! Configuration-phase registry payloads.
//!
//! A real 1.21.8 client refuses to leave configuration unless the server
//! enumerates every registry entry the world references. The trick that keeps
//! this cheap is the Known Packs handshake: the server advertises the built-in
//! `minecraft:core` data pack and, once the client echoes that it has the same
//! pack, every registry entry can be sent with its NBT *omitted*
//! ([`RegistryEntry::data`] = `None`) — the client fills each entry from its own
//! copy of the core pack. The server still has to list the entries (and in the
//! right order: numeric ids are assigned by send order), but it ships no NBT.
//!
//! [`ConfigRegistries`] holds the two halves of that exchange, built once at
//! startup and shared across connections: the [`KnownPack`] list to advertise and
//! the ordered [`RegistryData`] packets to send after the client's echo arrives.
//!
//! The enumerated set is the minimum a flat overworld needs:
//! `dimension_type:[overworld]` (index `0`, referenced by `JoinGame`),
//! `worldgen/biome:[plains]` (id `0`, referenced by every chunk biome palette),
//! all 49 `damage_type`s, and one entry in each of the mob/painting variant
//! registries the client validates on join.

use ferrumc_codec::BoundedString;
use ferrumc_proto::generated::configuration::{KnownPack, RegistryData, RegistryEntry};

/// The vanilla namespace every built-in registry entry lives under.
const NAMESPACE: &str = "minecraft";

/// The built-in data pack id advertised in Known Packs.
const CORE_PACK_ID: &str = "core";

/// The data-pack version advertised in Known Packs: the target game version.
///
/// Advertising the matching version is what lets the client accept NBT-omitted
/// registry entries — it knows it holds the same `core` pack.
const CORE_PACK_VERSION: &str = "1.21.8";

/// All 49 `minecraft:damage_type` keys, in the pinned `minecraft-data` order.
///
/// The client requires the damage-type registry to be fully enumerated even
/// though the NBT is omitted; a missing key is a hard kick.
const DAMAGE_TYPES: [&str; 49] = [
    "arrow",
    "bad_respawn_point",
    "cactus",
    "campfire",
    "cramming",
    "dragon_breath",
    "drown",
    "dry_out",
    "ender_pearl",
    "explosion",
    "fall",
    "falling_anvil",
    "falling_block",
    "falling_stalactite",
    "fireball",
    "fireworks",
    "fly_into_wall",
    "freeze",
    "generic",
    "generic_kill",
    "hot_floor",
    "in_fire",
    "in_wall",
    "indirect_magic",
    "lava",
    "lightning_bolt",
    "mace_smash",
    "magic",
    "mob_attack",
    "mob_attack_no_aggro",
    "mob_projectile",
    "on_fire",
    "out_of_world",
    "outside_border",
    "player_attack",
    "player_explosion",
    "sonic_boom",
    "spit",
    "stalagmite",
    "starve",
    "sting",
    "sweet_berry_bush",
    "thorns",
    "thrown",
    "trident",
    "unattributed_fireball",
    "wind_charge",
    "wither",
    "wither_skull",
];

/// The configuration-phase registry payloads, built once and shared.
///
/// Construct with [`ConfigRegistries::build`]; read the advertised packs with
/// [`known_packs`](Self::known_packs) and the ordered registry packets with
/// [`registries`](Self::registries).
#[derive(Debug, Clone)]
pub(crate) struct ConfigRegistries {
    /// The Known Packs advertised before any registry data is sent.
    known_packs: Vec<KnownPack>,
    /// The ordered `RegistryData` packets sent after the client's echo.
    registries: Vec<RegistryData>,
}

impl ConfigRegistries {
    /// Builds the Known Packs advertisement and the ordered registry packets.
    ///
    /// # Errors
    ///
    /// Returns an error only if a registry or entry identifier exceeds the
    /// protocol string bound — impossible for the fixed identifiers used here, so
    /// this never fails in shipped builds.
    pub(crate) fn build() -> anyhow::Result<Self> {
        let known_packs = vec![KnownPack::new(
            id(NAMESPACE)?,
            id(CORE_PACK_ID)?,
            id(CORE_PACK_VERSION)?,
        )];

        // Order matters: entry numeric ids are assigned by send order, so
        // `dimension_type:[overworld]` is index 0 (`JoinGame.dimension_type`) and
        // `worldgen/biome:[plains]` is biome id 0 (the chunk biome palette value).
        let registries = vec![
            registry("minecraft:dimension_type", &["overworld"])?,
            registry("minecraft:worldgen/biome", &["plains"])?,
            registry("minecraft:damage_type", &DAMAGE_TYPES)?,
            registry("minecraft:painting_variant", &["alban"])?,
            registry("minecraft:wolf_variant", &["pale"])?,
            registry("minecraft:wolf_sound_variant", &["classic"])?,
            registry("minecraft:cat_variant", &["black"])?,
            registry("minecraft:cow_variant", &["temperate"])?,
            registry("minecraft:pig_variant", &["temperate"])?,
            registry("minecraft:chicken_variant", &["temperate"])?,
            registry("minecraft:frog_variant", &["temperate"])?,
        ];

        Ok(Self {
            known_packs,
            registries,
        })
    }

    /// The Known Packs to advertise in `ClientboundKnownPacks`.
    pub(crate) fn known_packs(&self) -> &[KnownPack] {
        &self.known_packs
    }

    /// The ordered `RegistryData` packets to send after the client's echo.
    pub(crate) fn registries(&self) -> &[RegistryData] {
        &self.registries
    }
}

/// Builds one `RegistryData` packet for `registry_id`, listing each `key` as a
/// fully-qualified `minecraft:<key>` entry with its NBT omitted.
fn registry(registry_id: &str, keys: &[&str]) -> anyhow::Result<RegistryData> {
    let entries = keys
        .iter()
        .map(|key| Ok(RegistryEntry::new(id(&format!("{NAMESPACE}:{key}"))?, None)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(RegistryData::new(id(registry_id)?, entries))
}

/// Wraps `value` in the protocol's identifier string type.
fn id(value: &str) -> anyhow::Result<BoundedString<32_767>> {
    BoundedString::new(value.to_string())
        .map_err(|err| anyhow::anyhow!("registry identifier {value:?} invalid: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_the_core_pack() {
        let registries = ConfigRegistries::build().expect("registries build");
        let packs = registries.known_packs();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].namespace().as_str(), "minecraft");
        assert_eq!(packs[0].id().as_str(), "core");
        assert_eq!(packs[0].version().as_str(), "1.21.8");
    }

    #[test]
    fn enumerates_eleven_registries() {
        let registries = ConfigRegistries::build().expect("registries build");
        assert_eq!(registries.registries().len(), 11);
    }

    #[test]
    fn dimension_type_is_first_with_overworld_at_index_zero() {
        let registries = ConfigRegistries::build().expect("registries build");
        let first = &registries.registries()[0];
        assert_eq!(first.registry_id().as_str(), "minecraft:dimension_type");
        assert_eq!(first.entries().len(), 1);
        assert_eq!(
            first.entries()[0].entry_id().as_str(),
            "minecraft:overworld"
        );
        // NBT omitted: the client fills it from the advertised core pack.
        assert!(first.entries()[0].data().is_none());
    }

    #[test]
    fn damage_type_enumerates_all_forty_nine() {
        let registries = ConfigRegistries::build().expect("registries build");
        let damage = registries
            .registries()
            .iter()
            .find(|r| r.registry_id().as_str() == "minecraft:damage_type")
            .expect("damage_type registry present");
        assert_eq!(damage.entries().len(), 49);
        assert_eq!(damage.entries()[0].entry_id().as_str(), "minecraft:arrow");
        assert_eq!(
            damage.entries()[48].entry_id().as_str(),
            "minecraft:wither_skull"
        );
    }

    #[test]
    fn every_entry_omits_its_nbt() {
        let registries = ConfigRegistries::build().expect("registries build");
        for data in registries.registries() {
            for entry in data.entries() {
                assert!(
                    entry.data().is_none(),
                    "{} entry {} unexpectedly carries NBT",
                    data.registry_id().as_str(),
                    entry.entry_id().as_str(),
                );
            }
        }
    }
}

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod biome;
pub mod block_state;
pub mod dimension;
pub mod item;

/// Human-readable Minecraft release this registry data targets.
pub const MINECRAFT_VERSION: &str = "1.21.8";

/// Protocol (network) version negotiated in the handshake for [`MINECRAFT_VERSION`].
///
/// Shared with 1.21.7; disambiguate releases by [`DATA_VERSION`], not this value.
pub const PROTOCOL_VERSION: i32 = 772;

/// World data version for [`MINECRAFT_VERSION`].
///
/// Uniquely identifies the release (1.21.7 also uses protocol `772` but data
/// version `4438`), and is written into saved world NBT.
pub const DATA_VERSION: i32 = 4440;

/// Returns the default block-state id for a block resource location, or `None`
/// if the block is not part of the minimal flat-world set this crate pins.
///
/// The leading `minecraft:` namespace is optional, so both `"minecraft:stone"`
/// and `"stone"` resolve to the same id. The returned value is the block's
/// *default* state — the state a freshly placed block takes.
///
/// Lookups are an exhaustive `match` over a handful of names, so this is
/// allocation-free and deterministic with zero boot cost.
///
/// # Examples
///
/// ```
/// use ferrumc_registry::default_block_state_id;
///
/// assert_eq!(default_block_state_id("minecraft:stone"), Some(1));
/// assert_eq!(default_block_state_id("grass_block"), Some(9));
/// assert_eq!(default_block_state_id("minecraft:diamond_block"), None);
/// ```
#[must_use]
pub fn default_block_state_id(name: &str) -> Option<u32> {
    // Accept the canonical namespaced form and the bare form interchangeably.
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    match bare {
        "air" => Some(block_state::AIR),
        "stone" => Some(block_state::STONE),
        "grass_block" => Some(block_state::GRASS_BLOCK),
        "dirt" => Some(block_state::DIRT),
        "bedrock" => Some(block_state::BEDROCK),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    use super::{
        biome, block_state, default_block_state_id, dimension, DATA_VERSION, MINECRAFT_VERSION,
        PROTOCOL_VERSION,
    };

    /// Absolute path to a vendored protocol fixture for this Minecraft version.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../fixtures/protocol/1_21_8")
            .join(name)
    }

    #[test]
    fn lookup_accepts_namespaced_and_bare_names() {
        assert_eq!(default_block_state_id("air"), Some(block_state::AIR));
        assert_eq!(
            default_block_state_id("minecraft:air"),
            Some(block_state::AIR)
        );
        assert_eq!(
            default_block_state_id("minecraft:grass_block"),
            Some(block_state::GRASS_BLOCK)
        );
    }

    #[test]
    fn lookup_rejects_unknown_and_malformed_names() {
        assert_eq!(default_block_state_id(""), None);
        assert_eq!(default_block_state_id("minecraft:"), None);
        assert_eq!(default_block_state_id("STONE"), None); // case-sensitive
        assert_eq!(default_block_state_id("minecraft:diamond_block"), None);
        assert_eq!(default_block_state_id("notch:stone"), None); // wrong namespace
    }

    #[test]
    fn resource_location_constants_are_namespaced() {
        assert_eq!(dimension::OVERWORLD, "minecraft:overworld");
        assert_eq!(biome::PLAINS, "minecraft:plains");
    }

    #[test]
    fn overworld_height_geometry_is_consistent() {
        // Buildable range must be MIN_Y ..= 319 for a 384-tall overworld.
        let max_y = dimension::MIN_Y + i32::try_from(dimension::HEIGHT).unwrap() - 1;
        assert_eq!(dimension::MIN_Y, -64);
        assert_eq!(max_y, 319);
        // Height must be a whole number of 16-block sections.
        assert_eq!(dimension::HEIGHT % 16, 0);
    }

    /// Drift guard: the hardcoded block-state ids must match the *default* state
    /// of each block in the vendored `blocks.json` snapshot. If a re-pin to a
    /// newer minecraft-data version shifts an id, this test fails loudly instead
    /// of silently shipping the wrong palette ids to clients.
    #[test]
    fn block_state_constants_match_vendored_blocks_json() {
        #[derive(serde::Deserialize)]
        struct BlockEntry {
            name: String,
            #[serde(rename = "defaultState")]
            default_state: u32,
            #[serde(rename = "minStateId")]
            min_state_id: u32,
            #[serde(rename = "maxStateId")]
            max_state_id: u32,
        }

        let raw = std::fs::read_to_string(fixture("blocks.json"))
            .expect("vendored blocks.json fixture must exist");
        let blocks: Vec<BlockEntry> =
            serde_json::from_str(&raw).expect("blocks.json must parse as the expected schema");

        let by_name: BTreeMap<&str, &BlockEntry> =
            blocks.iter().map(|b| (b.name.as_str(), b)).collect();

        let expectations = [
            ("air", block_state::AIR),
            ("stone", block_state::STONE),
            ("grass_block", block_state::GRASS_BLOCK),
            ("dirt", block_state::DIRT),
            ("bedrock", block_state::BEDROCK),
        ];

        for (name, constant) in expectations {
            let entry = by_name
                .get(name)
                .unwrap_or_else(|| panic!("block {name} missing from blocks.json"));
            assert_eq!(
                entry.default_state, constant,
                "default-state id drift for {name}: snapshot={}, constant={constant}",
                entry.default_state
            );
            // The default state must lie within the block's own state range.
            assert!(
                (entry.min_state_id..=entry.max_state_id).contains(&constant),
                "{name} default {constant} outside [{}, {}]",
                entry.min_state_id,
                entry.max_state_id
            );
            // And the lookup must agree with the constant.
            assert_eq!(default_block_state_id(name), Some(constant));
        }
    }

    /// Drift guard: the pinned biome must still exist in the vendored
    /// `biomes.json` snapshot under its expected resource location.
    #[test]
    fn pinned_biome_exists_in_vendored_biomes_json() {
        #[derive(serde::Deserialize)]
        struct BiomeEntry {
            name: String,
        }

        let raw = std::fs::read_to_string(fixture("biomes.json"))
            .expect("vendored biomes.json fixture must exist");
        let biomes: Vec<BiomeEntry> =
            serde_json::from_str(&raw).expect("biomes.json must parse as the expected schema");

        let plains_bare = biome::PLAINS.strip_prefix("minecraft:").unwrap();
        assert!(
            biomes.iter().any(|b| b.name == plains_bare),
            "pinned biome {} not found in biomes.json",
            biome::PLAINS
        );
    }

    /// Drift guard: the version constants must match the vendored `version.json`
    /// and the cross-checked data version for this release.
    #[test]
    fn version_constants_match_vendored_version_json() {
        #[derive(serde::Deserialize)]
        struct Version {
            version: i32,
            #[serde(rename = "minecraftVersion")]
            minecraft_version: String,
        }

        let raw = std::fs::read_to_string(fixture("version.json"))
            .expect("vendored version.json fixture must exist");
        let v: Version =
            serde_json::from_str(&raw).expect("version.json must parse as the expected schema");

        assert_eq!(v.version, PROTOCOL_VERSION);
        assert_eq!(v.minecraft_version, MINECRAFT_VERSION);
        // version.json does not carry the data version; pin it explicitly.
        assert_eq!(DATA_VERSION, 4440);
    }

    /// Drift guard: every fixture's bytes must match the sha256 + size recorded
    /// in `manifest.toml`, so the manifest we ship is a faithful description of
    /// the vendored data and tampering or a botched re-vendor is caught.
    #[test]
    fn fixtures_match_manifest_checksums() {
        #[derive(serde::Deserialize)]
        struct Manifest {
            files: BTreeMap<String, FileEntry>,
        }
        #[derive(serde::Deserialize)]
        struct FileEntry {
            sha256: String,
            bytes: u64,
        }

        let raw = std::fs::read_to_string(fixture("manifest.toml"))
            .expect("manifest.toml fixture must exist");
        let manifest: Manifest = toml::from_str(&raw).expect("manifest.toml must parse");

        assert!(
            !manifest.files.is_empty(),
            "manifest must describe at least one file"
        );

        for (name, entry) in &manifest.files {
            let bytes = std::fs::read(fixture(name))
                .unwrap_or_else(|_| panic!("fixture {name} listed in manifest must exist"));
            assert_eq!(
                bytes.len() as u64,
                entry.bytes,
                "byte-count drift for {name}"
            );
            let digest = Sha256::digest(&bytes);
            let mut hex = String::with_capacity(digest.len() * 2);
            for byte in digest {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
            }
            assert_eq!(hex, entry.sha256, "sha256 drift for {name}");
        }
    }
}

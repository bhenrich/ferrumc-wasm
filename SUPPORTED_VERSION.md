# Supported Version

FerrumC v2 supports **exactly one** Minecraft client. There is no version range, no
backwards-compatibility shim, and no auto-detection of other protocols.

| Field | Value |
|-------|-------|
| Edition | Minecraft: Java Edition |
| Version | **1.21.8** |
| Protocol | **772** |
| Data version | **4440** |
| Mode | Offline / local only |

A client on any other version will be rejected. This is intentional.

## Why pin to exactly one version

A Minecraft server's wire format is tied to a specific protocol number. Packet IDs,
field layouts, registry contents, and the connection state machine all change between
versions — sometimes subtly, sometimes wholesale. Supporting multiple protocols at once
means either:

- branching every decoder/encoder on the negotiated protocol (a combinatorial mess that
  is hard to test and easy to get silently wrong), or
- a translation layer that converts foreign packets to the native version (its own
  large, bug-prone subsystem).

Neither is worth it for an alpha. Pinning to one protocol keeps the codebase honest:
the generated packet definitions, the registry data, and the tests all describe **one**
real client, and we can verify byte-for-byte against it. A single supported version is a
feature here, not a limitation — it's what makes "deterministic" and "verified against a
real client" true claims instead of aspirations.

## What about the newer Minecraft releases?

Minecraft moved to year-based versioning in 2026. As of this writing the current release
is **26.2 ("Chaos Cubed", protocol 776)**, with **26.3** in snapshots. These are **out of
scope** for FerrumC v2.

FerrumC pins one version and treats upgrades as tracked work, not a moving target we
chase. When a version bump happens it will be a deliberate, verified migration (regenerate
protocol/registry data, update fixtures, re-test against the new client), not a "support
everything" effort. Until then, 1.21.8 is the line.

## How to select 1.21.8 in the launcher

Using the official Minecraft Launcher:

1. Open the launcher and go to the **Installations** tab.
2. Click **New installation**.
3. In the **Version** dropdown, select **release 1.21.8**.
4. Give it a name (e.g. `FerrumC 1.21.8`), then **Create**.
5. Go back to **Play**, pick that installation, and launch.
6. Add the server (default `127.0.0.1` for a local instance) and connect.

FerrumC runs in **offline / local mode** — there is no online-mode authentication or
encryption yet. Run it for local/LAN testing, not on the open internet.

---

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

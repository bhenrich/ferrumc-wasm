# Safety: `ferrumc-plugin-host`

> Every `unsafe` block in this crate, what it assumes, and why the assumption
> holds. Required reading before changing anything in `src/dynamic/ffi.rs`.

## Why this crate is not `forbid(unsafe_code)`

Dynamic plugin loading (ADR-0006) requires FFI: `libloading` to `dlopen`/`dlsym`
a plugin `cdylib`, and `extern "C"` function pointers to call into it. That is
inherently `unsafe`. The rest of the workspace is `forbid(unsafe_code)`; this
crate downgrades to **`deny(unsafe_code)`** (in `Cargo.toml`'s `[lints.rust]`
and at the top of `lib.rs`) and confines every unsafe operation to a single
module:

```rust
// src/dynamic/mod.rs
#[allow(unsafe_code)]
mod ffi;
```

No other module in the crate contains `unsafe`. In particular the adapter
(`dynamic::adapter`) that bridges a loaded plugin to the `Plugin` trait is pure
safe Rust: calling a *validated* `extern "C"` function pointer is a safe
operation in Rust, so the lifecycle forwarding needs no `unsafe`.

## The trust boundary

The loader runs arbitrary native code from the plugins directory. This is the
same trust model as every `dlopen`-based plugin system (Bukkit, Postgres
extensions, etc.): **the operator is trusted to put only vetted libraries in the
plugins folder.** Loading a hostile `cdylib` is out of scope and cannot be made
sound by any host. What the host *does* defend against is *accidental*
incompatibility and *misbehaving-but-not-malicious* plugins:

- an ABI-version check rejects libraries built against a different host;
- a null-pointer check rejects a vtable the plugin declined to provide;
- UTF-8 validation rejects malformed metadata;
- panics are expected to be caught plugin-side and reported as status codes; a
  plugin that unwinds across the `extern "C"` boundary aborts the process (this
  is rustc's defined behavior for `extern "C"`, not UB).

## The unsafe blocks in `dynamic/ffi.rs`

### 1. `Library::new(path)`

```rust
let library = unsafe { Library::new(path) };
```

**Invariant:** `path` refers to a file the operator placed in the trusted
plugins directory. Opening it runs the library's initializers (native code
execution). **Soundness:** guaranteed only by the trust model above; no
`dlopen`-based loader can do better. The returned `Library` is kept alive for as
long as any pointer into it is used (it is moved into the `LoadedPlugin`, or
dropped on an error path after we finish reading).

### 2. `library.get::<PluginEntryFn>(symbol)`

```rust
let entry: Symbol<PluginEntryFn> = unsafe { library.get(symbol.to_bytes_with_nul()) };
```

**Invariant:** the resolved symbol actually has the `PluginEntryFn` signature
(`extern "C" fn() -> *const PluginVTable`). **Soundness:** the symbol name
(`ferrumc_plugin_entry`) and signature are fixed by `ferrumc_plugin_api::abi`;
any plugin built against that crate exports exactly this. The ABI-version field,
read immediately after, is the cross-check that catches a library built against
an incompatible version of the contract *before* any other field is trusted. The
`Symbol` borrows `library`; it is created and consumed inside a block so the
borrow ends before `library` is moved.

### 3. `&*vtable_ptr`

```rust
let vtable: &PluginVTable = unsafe { &*vtable_ptr };
```

**Invariant:** `vtable_ptr` is non-null (explicitly checked first) and points to
a properly-aligned, initialized `PluginVTable` that lives for at least as long as
`library` is loaded. **Soundness:** the ABI contract (`abi` module docs)
requires the entrypoint to return a pointer to a `'static` vtable owned by the
plugin; such a value lives in the library image and is valid until unload. We
only read through this reference; we never write to or free plugin memory. After
this we read `abi_version` and reject on mismatch before using any other field.

### 4. `CStr::from_ptr(ptr)` (in `read_str`)

```rust
let cstr = unsafe { CStr::from_ptr(ptr) };
```

**Invariant:** `ptr` is non-null (checked) and points to a nul-terminated byte
string valid for the duration of the read. **Soundness:** the ABI requires the
`id`/`name` function pointers to return nul-terminated `'static` UTF-8 strings
owned by the plugin. We read the string immediately, validate it is UTF-8 (a
non-UTF-8 string is rejected as `InvalidMetadata`), copy it into an owned
`String`, and never free it. A plugin returning a non-nul-terminated pointer is
the one residual trust we place in `extern "C"` plugin code — unavoidable for a
C string ABI, and bounded to this single read.

## Ownership rules across the boundary

- **Host never frees plugin memory; plugin never frees host memory.** All
  strings and the vtable are plugin-owned and live in the loaded image. The host
  copies metadata out at load time and thereafter holds only `Copy` function
  pointers plus the `Library` handle — never a borrowed pointer into the image.
- **The `Library` outlives every call into it.** `LoadedPlugin` stores the
  `Library` as its last field and implements `Drop` to run the plugin's
  `shutdown` (if it was initialized) *before* field drop unloads the library.
- **No unwinding across FFI.** `init`/`shutdown` are `extern "C"`; a plugin that
  might panic must `catch_unwind` and return a status. Once registered, the
  plugin is also wrapped by the host's existing `catch_unwind`-based isolation
  for the *host-side* calls, so a panic in safe adapter code cannot escape
  either.

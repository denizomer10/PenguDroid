# Device compatibility profile

`pengudroid-device` supplies a small, dependency-free, safe Rust property table.
Its default **Pixel 11** profile is Android-facing compatibility metadata, not
an identity for an actual Pixel 11 or a claim about real Google hardware. It
invents no Pixel codename, Tensor/ARM SoC, Google build fingerprint, serial,
IMEI, or security-patch level.

## API and exact properties

- `PIXEL_11_PROFILE`: a constant `&[(&str, &str)]` of 43 properties, in fixed
  order with no duplicates.
- `DEFAULT_PROFILE`: an alias of that same slice. Use it directly or iterate
  with `DEFAULT_PROFILE.iter().copied()`; no allocation is required.
- `is_protected_profile_key(&str) -> bool`: exact, case-sensitive membership in
  that table after stripping any number of `ro.appcompat_override.` wrappers.
  It classifies keys only; it does not install defaults or enforce a lock.

The following six fields appear both as `ro.product.<field>` and as
`ro.product.<partition>.<field>` for **each** of `system`, `vendor`, `product`,
`odm`, and `system_ext` (36 properties total):

| Field | Value |
| --- | --- |
| `model` | `Pixel 11` |
| `manufacturer` | `Google` |
| `brand` | `google` |
| `name` | `pixel11` |
| `device` | `pixel11` |
| `board` | `pixel11` |

The remaining properties are:

| Key | Value |
| --- | --- |
| `ro.hardware` | `pengudroid` |
| `ro.debuggable` | `0` |
| `ro.secure` | `1` |
| `ro.adb.secure` | `1` |
| `ro.build.type` | `user` |
| `ro.build.tags` | `release-keys` |
| `service.adb.root` | `0` |

The product/device/board names are explicitly synthetic. The release-like
strings describe the compatibility profile: `release-keys` is not evidence of
release signing, and `ro.secure=1` is not a security or isolation guarantee.

## Scope and non-guarantees

The actual **Android 17 / API 37, x86_64** runtime identity remains the parent's
responsibility and must stay truthful. This crate emits no API/SDK, CPU/ABI,
Android-version, or fingerprint properties, and its key helper does not replace
the existing platform-identity guard.

It supplies no verified-boot `green` state, bootloader-lock claims, attestation
certificates, or attestation forgery. It promises no CTS pass, Play Integrity
verdict, Google certification, actual Pixel 11 behavior, or root safety.

The non-root/release-like presentation is **metadata only**. This crate has no
I/O, hooks, FFI, or host-OS interception. It neither grants nor suppresses host
privileges, changes UIDs, conceals superuser access, intercepts root detectors,
nor configures an actual ADB service. Existing UID handling is outside its
boundary. Applications may still detect the compatibility layer or reject it.

## Separate Java process-identity boundary

`crates/pengudroid-cli/src/process_identity.rs` is independent of this property
profile. Before application initialization it maps the four API-37 Java/libcore
UID/GID query natives to the app UID already used by `ApplicationInfo.uid`
(currently 10000). This prevents an ordinary Linux user such as UID 1000 from
being misclassified as Android's privileged `SYSTEM_UID`.

The adapter does not interpose host libc/native identity queries, change Linux
credentials, grant or remove privileges, or provide a security sandbox. It
refuses a real/effective root host UID and does not conceal later root
credentials. Native code still observes host identities; this is not universal
identity virtualization, root detection evasion, or attestation. WebView's
privilege/provider checks remain intact, and WebView is not advertised without
a working hosted provider. The API-37 CheckJNI gate checks the Java mapping and
unchanged host identities separately; see [STATUS.md](STATUS.md).

Removing `pengudroid-device` does not remove this process-identity adapter, just
as removing that adapter does not alter the profile's properties.

## Parent integration and removal boundary

Workspace membership, dependency wiring, and daemon/shim consumers are left to
the parent integration; merely adding this crate changes no running behavior.
Populate the table after other base/image defaults, then guard every later
override path, including persisted-property ingestion and appcompat aliases.
For a protected key, compare against the raw store: reject differing values;
an identical write can be accepted. An absent wrapped key must stay absent,
rather than accepting a new alias that bypasses the base value. Keep the
platform-identity guard independently active. Near matches such as
`ro.product.model.extra` are not profile keys.

The crate owns only its constant table and key classifier: no property store,
persistence, privilege policy, or process-global state. To remove the feature,
remove the parent's table-population and profile-lock calls, its dependency and
workspace entry, then this crate and document. Any copied/persisted profile
values must be discarded by their owning consumer; removing this crate alone
does not clean another component's store. Runtime API/ABI protection and host
privilege behavior do not belong to this removal boundary.

`crates/pengudroid-device/tests/profile.rs` checks the exact deterministic table,
partition consistency, protected-key matching (including nested wrappers), and
absence of platform-identity and security/attestation claims. Once integrated,
run `cargo test -p pengudroid-device`; direct `rustc` compilation can validate the
library and integration tests without changing workspace files or resolving
any dependencies.

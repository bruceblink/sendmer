# sendmer v0.10 Manifest v1 Design

## Terminology and Naming

| 规范名 | English / Acronym | 本方案中的职责 | 不代表什么 |
| --- | --- | --- | --- |
| 传输清单 | Transfer Manifest / TM1 | 描述逻辑目录树、路径编码、文件类型和可选文件元数据 | 不是 iroh 的 `Collection` wire metadata，也不是缓存 `manifest.json` |
| 旧集合 | Legacy Collection / Collection V0 | iroh-blobs 当前的 UTF-8 名称到 payload hash 映射 | 不承载空目录、非 UTF-8 名称或平台权限语义 |
| 载荷 Blob | Payload Blob | 单个普通文件的原始字节内容 | 不包含路径、权限或时间戳 |
| 清单条目 ID | Manifest Entry ID | 清单模式中映射到 Collection 的稳定 ASCII 名称 | 不代表用户文件名，也不是安全凭据 |
| 路径编码 | Path Encoding | 说明路径组件如何在 wire 上表达和在目标平台还原 | 不保证任意平台都能还原另一平台的原生字节名 |

## Scope

M10.2 first introduces a versioned manifest contract for directory transfers. The
legacy file-only collection remains readable and remains the default wire format
until the paired sender/receiver compatibility gate is complete. Manifest mode is
the only mode allowed to represent empty directories or names that cannot be
represented by the legacy UTF-8 collection.

The manifest is deliberately separate from the receive-cache `manifest.json`.
The former travels with a transfer; the latter records local cache ownership and
TTL. Neither document contains a ticket, endpoint secret, or connection state.

## Wire Shape

The manifest is a UTF-8 JSON payload stored as a dedicated raw Payload Blob. The
collection contains that blob under the reserved ASCII name
`.sendmer/manifest-v1` and maps each file payload to a deterministic Entry ID:
`.sendmer/entry/<lowercase-hex-index>`. A manifest-mode collection is therefore
identified by the reserved manifest entry before any user path is exported.

```json
{
  "schema_version": 1,
  "root": { "kind": "directory", "path": { "encoding": "utf8", "components": ["share"] } },
  "entries": [
    {
      "id": "00000000",
      "kind": "directory",
      "path": { "encoding": "utf8", "components": ["share", "empty"] },
      "payload": null,
      "metadata": { "permissions": null, "modified": null }
    },
    {
      "id": "00000001",
      "kind": "file",
      "path": { "encoding": "utf8", "components": ["share", "readme.txt"] },
      "payload": { "collection_name": ".sendmer/entry/00000001", "size": 12 },
      "metadata": { "permissions": { "posix_mode": 420 }, "modified": { "seconds": 0, "nanos": 0 } }
    }
  ]
}
```

Normative rules:

- `schema_version` is an integer, and a receiver rejects a version greater than
  the highest version it implements with a stable unsupported-manifest error.
- `root` is exactly one top-level path. Every entry is relative to that root and
  appears once; duplicate paths, empty components, `.`/`..`, separators inside a
  component, and a payload attached to a directory are invalid.
- `id` is lowercase hexadecimal, unique, contiguous from zero, and is only an
  internal Collection key. The receiver never uses it as a filesystem path.
- `payload` is required for a file and forbidden for a directory. A payload size
  is checked against the received blob before export; it is not trusted for
  allocation or progress accounting.
- `metadata` is optional. `modified` uses signed Unix seconds plus a nanosecond
  remainder so pre-1970 timestamps are representable without platform-specific
  integer widths. `permissions` is capability-limited: POSIX mode bits and the
  Windows read-only attribute may be carried; ACLs, owners, and security
  descriptors are explicitly unsupported in v1.

## Path Encoding

`utf8` is the interoperable default. A Unix sender that encounters a non-UTF-8
component may use `unix_bytes_hex`, where each component is the lowercase hex of
its raw `OsStrExt` bytes. A Windows sender may use `windows_utf16le_hex`, where
each component is the lowercase hex of its UTF-16 code units in little-endian
order. The receiver must reject an encoding it cannot safely materialize on the
target platform; it must never lossy-decode or substitute a different name.

Path components are sorted by their encoded wire form before serialization. This
makes the manifest hash stable for the same source tree and avoids filesystem
iteration order becoming a protocol detail.

## Compatibility and Migration

1. A receiver first loads the iroh Collection and checks for the reserved
   `.sendmer/manifest-v1` entry.
2. Without that entry, it uses the existing Collection V0 export path. Existing
   tickets, cache entries, and file-only collections remain readable.
3. With the entry, it fetches and validates TM1 before creating any destination
   path, then exports only validated payload mappings. The reserved entry is
   never visible to the user.
4. A future schema uses a new reserved entry and a new decoder; it never changes
   the meaning of TM1 fields in place.

Manifest-mode tickets require a receiver that advertises TM1 support through the
release contract. Until that contract is shipped, the sender must keep legacy
mode as the default and expose manifest mode only behind an explicit option or
an automatically detected feature that has a paired receiver.

## Safety and Acceptance Matrix

| Case | Expected result |
| --- | --- |
| Empty directory | TM1 directory entry is created and materialized without a payload Blob |
| Non-UTF-8 Unix name | `unix_bytes_hex` round-trips on Unix; other targets fail closed with an unsupported-path error |
| POSIX mode / modified time | Applied only after all payloads verify; failures remove staging and report a filesystem error |
| Existing destination or symlink | Rejected before export; no merge or replacement |
| Malformed, duplicate, traversal, future-version manifest | Rejected before destination creation; no partial export |
| Legacy Collection V0 | Existing export behavior and no-replace guarantees remain unchanged |

The implementation must add Unix and Windows-compatible fixtures for the common
UTF-8 path form, plus platform-specific tests for raw-byte and permission
representations. Native filesystem acceptance remains a release gate; JSON and
unit tests alone do not prove timestamp or permission behavior.

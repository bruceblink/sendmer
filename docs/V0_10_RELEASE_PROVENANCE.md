# sendmer v0.10 Release Provenance

## Terminology and Naming

| 规范名 | English / Acronym | 本方案中的职责 | 不代表什么 |
| --- | --- | --- | --- |
| 发布资产 | Release Asset | 某个 target 的 archive、checksum、SBOM 或证明文件 | 不是源码 tag 本身，也不是 crates.io 包的替代物 |
| 软件物料清单 | Software Bill of Materials / SBOM | SPDX JSON，描述对应打包目录中的二进制和构建元数据 | 不是签名，也不是漏洞扫描结论 |
| Blob 签名 | Sigstore Blob Signature | 对 archive 和 SBOM 做 keyless 签名，输出 Sigstore bundle | 不是长期私钥签名或 Git commit 签名 |
| 构建出处证明 | Build Provenance Attestation | GitHub Artifact Attestation，绑定 archive SHA-256 与 tag workflow 身份 | 不是运行时授权或下载器信任策略 |
| 资产索引 | Asset Index | 每个 target 的稳定文件命名与关联关系 | 不是业务 manifest 或传输清单 TM1 |

## Asset Contract

For a release version `vX.Y.Z` and Rust target `TARGET`, the matrix job publishes
the following set as one atomic target group:

```text
sendmer-vX.Y.Z-TARGET.tar.gz      # Linux/macOS
sendmer-vX.Y.Z-TARGET.zip         # Windows
<archive>.sha256
<archive>.spdx.json
<archive>.sigstore.json
<archive>.spdx.json.sigstore.json
<archive>.intoto.jsonl
```

`BUILD-METADATA.txt` remains inside the archive and records the tag and source
commit. The `.sha256` file names the archive exactly. The archive's Sigstore
bundle signs the bytes that the checksum names; the SBOM bundle signs the
corresponding SPDX JSON. The `.intoto.jsonl` file is the serialized GitHub
provenance bundle emitted for the archive subject.

No upload step may publish only part of a target group. The package step must
create and validate every file before the release upload step runs, and the
upload step uses `--clobber` so rerunning a tag is idempotent.

## Trust and Permissions

The build matrix uses GitHub OIDC for keyless Sigstore signing and Artifact
Attestations. Required job permissions are limited to `contents: write` for the
release, `id-token: write` for the OIDC identity, `attestations: write` for the
provenance record, and `artifact-metadata: write` for the attestation subject
record. No private signing key or long-lived release credential is introduced.

The workflow must fail closed when any of these conditions is false:

- the archive, checksum, SBOM, signature bundles, or provenance bundle is empty;
- the checksum does not name the archive or does not contain a 64-character
  lowercase SHA-256 digest;
- a signature command or provenance action fails;
- a generated filename does not contain the exact release tag and target;
- a rerun cannot replace the complete target group.

Consumers can verify the checksum first, then verify the Sigstore bundle against
the expected GitHub workflow identity and OIDC issuer. GitHub's attestation
record provides an independent provenance lookup for the archive subject.

## SBOM Scope and Reproducibility

Syft scans the unpacked target directory after the release binary and
`BUILD-METADATA.txt` are created but before the archive is compressed. This keeps
the SBOM tied to the bytes shipped inside the archive while avoiding archive
format-specific scanner behavior. The SBOM document itself is uploaded and
signed as a release asset.

The release tag remains the source of truth for version, and `source_commit` in
the archive must equal the commit resolved from that tag. A rerun may refresh
assets for the same tag but must not silently attest a different commit.

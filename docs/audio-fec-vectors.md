# 音频 FEC 独立上游向量

`src/audio/fec/tests.rs` 的 `UPSTREAM_PARITY` 来自真实上游 nanors C 编码器. 生成器只构造输入, 设置音频矩阵并调用上游 API, 不实现 GF 运算. Rust 编码逐字节对照固定校验片, 恢复测试直接使用固定校验片, 不先调用 Rust 编码器.

## 来源

| 组件 | 固定版本 | 依据 |
| --- | --- | --- |
| Sunshine | `40b36212886a914082bfe69cea35210057fc98a1` | [音频初始化与编码](https://github.com/LizardByte/Sunshine/blob/40b36212886a914082bfe69cea35210057fc98a1/src/stream.cpp#L1855) |
| moonlight-common-c | `62e066388f1a1b133e0bee947b9a374311a3354b` | [音频矩阵覆盖](https://github.com/moonlight-stream/moonlight-common-c/blob/62e066388f1a1b133e0bee947b9a374311a3354b/src/RtpAudioQueue.c#L48) |
| nanors | `b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a` | [原始 C 源码](https://github.com/sleepybishop/nanors/tree/b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a), Moonlight 的 `nanors` gitlink |

Sunshine 的 `third-party/moonlight-common-c` gitlink 指向上表 Moonlight 版本. Moonlight 的 `nanors` gitlink 指向上表 nanors 版本. 这条依赖链通过 `git ls-tree` 读取 pinned Git 对象确定, 不依赖构建配置. 已克隆的 Sunshine 和 Moonlight 源码中没有 `rs.c` / `rs.h`, 因此从公开 nanors 仓库取得对应 commit 的源码归档.

Sunshine 和 Moonlight 都先创建 4 数据片 + 2 校验片的 RS 对象, 再覆盖 `rs->p`:

```text
77 40 38 0e
c7 a7 0d 6c
```

生成器保持同样的顺序, 链接未修改的 `rs.c`, `deps/obl/oblas_lite.c`, `deps/obl/oblas_common.c`. 上游默认生成的矩阵不能直接代表这条音频路径. 这里验证的是该 pinned 音频矩阵与 nanors 的独立 C 实现, 不声称执行过 OpenFEC, NVIDIA 实机或完整网络端到端互操作.

nanors 的 MIT 许可证保留在源码归档内, 并逐字节复制到 [audio-fec-nanors-LICENSE.txt](../native/tests/audio-fec-nanors-LICENSE.txt). 上游运算源码通过外部路径编译, 不作为项目生产依赖.

## 固定输入和覆盖

片序固定为 `D0 D1 D2 D3 P0 P1`. 每个片内字节偏移从 0 开始.

| 向量 | 每片长度 | 输入定义 | 用途 |
| --- | --- | --- | --- |
| `basis` | 4 | `D[shard][byte] = (shard == byte ? 1 : 0)` | 独立定位每列系数和校验片顺序 |
| `sweep` | 257 | `D[shard][byte] = (byte * 73 + shard * 41) mod 256` | 每个输入片前 256 字节遍历全部字节值, 包括 0 和高位字节, 并覆盖非对齐长度 |

`basis` 的 `P0 = 7740380e`, `P1 = c7a70d6c`. `sweep` 的完整固定输出在 [tests.rs](../src/audio/fec/tests.rs) 的常量块中. C 生成器输出与该常量块完全一致.

每个向量的 C 自检包含 6 种单丢片和 15 种双丢片组合, 共 21 种. 缺失片先用 `0xa5` 覆盖, 再用 `reed_solomon_decode` 恢复, 对照全部原始数据片. 校验片丢失只要求数据正确, 不要求重新生成校验片.

Rust 测试共验证 2 次完整编码, 24 次单数据片恢复和 12 次双数据片恢复. 单丢分别保留 `P0`, `P1`, 或两者. 双丢遍历 4 个数据片的全部 6 种组合. 每次检查恢复数量及全部数据片, 确保未丢片也保持原值.

## 复现

生成器也可使用 `just audio-fec-vectors <nanors-source-dir>` 构建并运行. 常规 `just audio-test` 会执行同一份 Rust 固定向量测试, 不依赖外部源码. 独立复现需要已有 `clang`, `rustc`, `curl`, `tar`, `shasum`. 在项目根目录执行. 下面示例使用 fish, 外部源码目录通过脚本参数显式传入, 可替换为任何目录内的同版本源码. 不安装依赖.

```shell
mkdir -p .tmp/audio-fec
curl -fL https://codeload.github.com/sleepybishop/nanors/tar.gz/b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a -o .tmp/audio-fec/nanors.tar.gz
shasum -a 256 .tmp/audio-fec/nanors.tar.gz
tar -xzf .tmp/audio-fec/nanors.tar.gz -C .tmp/audio-fec
set upstream .tmp/audio-fec/nanors-b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a
bash native/tests/build-audio-fec-vectors.sh "$upstream" .tmp/audio-fec/generate
.tmp/audio-fec/generate > .tmp/audio-fec/upstream-parity.rs
sed -n '/^const UPSTREAM_PARITY:/,/^];/p' src/audio/fec/tests.rs > .tmp/audio-fec/committed-parity.rs
cmp .tmp/audio-fec/upstream-parity.rs .tmp/audio-fec/committed-parity.rs
shasum -a 256 .tmp/audio-fec/upstream-parity.rs
rustc --edition=2024 --test native/tests/audio-fec-standalone.rs -o .tmp/audio-fec/rust-tests
.tmp/audio-fec/rust-tests
```

生成器 stdout 仅包含固定常量, 进度及上游恢复检查结果写入 stderr. `cmp` 成功时没有输出. 独立 Rust 入口直接加载生产 `error`, `protocol`, `fec` 模块和同一份 FEC 测试, 不复制生产算法或片数常量.

已验证环境: Darwin arm64, Apple clang 17.0.0 (`clang-1700.6.4.2`), rustc 1.98.1. 结果:

```text
basis: 每片编码 4 字节, 21 种丢片组合通过
sweep: 每片编码 257 字节, 21 种丢片组合通过
test result: ok. 3 passed; 0 failed
```

## SHA-256

归档哈希记录实际下载内容, 若归档封装发生变化, 可进一步对照解包后的各源码文件. 未修改的上游文件哈希如下.

| 内容 | SHA-256 |
| --- | --- |
| nanors commit 源码归档 | `41edc0309b255b0eeb5e8eb1ad79f7c7e9e6c31db1bd79a16d73271c62867003` |
| 生成器 stdout / Rust 常量块 | `dc2992b90a346fd40b2f834223f345b6d178c15f110c7c0c1f0787955475c999` |
| `rs.c` | `e650cec353c2d3430e48c3a171d86622495d9ec1023bb7793d17bb5f05ba50c9` |
| `rs.h` | `cc17817b06ddd89b8bb438fe1c8b61e1f6c639670e98f9d035839c86340c5164` |
| `deps/obl/oblas_lite.c` | `34c357667ffd1c43e28dbd53c9da7ba6e6317c46d3afe09d0066252eb46172d3` |
| `deps/obl/oblas_lite.h` | `ff68e76d40146de1273b7ea013fc6b741096681664df885938235f6fbc0a38c7` |
| `deps/obl/oblas_common.c` | `187336b73b29d89ffc52d19566047618b03251aa4a35a05d685111fd57dc2397` |
| `deps/obl/oblas_common.h` | `06e632ab5a2f781e887ac539e0804add644dfa367c3086b1972af7c74085552c` |
| `deps/obl/gf2_8_tables.h` | `ed4480ba7cb72217724b24ae2cb51c58c74a10456bb0d7c8317f9a5e613a31db` |
| `deps/obl/gf2_8_affine_mat.h` | `7742dcb53c95a6c04771b60ebd60f281a50684faf4599b4a9b0962daa5161d2a` |
| `LICENSE` | `3fdda5f011d8490331950398e86427d67dfae05e048681476c2c6b8c34bdd033` |

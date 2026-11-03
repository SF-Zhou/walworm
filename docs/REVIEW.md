# walworm 代码评审报告

> 评审对象：`walworm` v0.1.0（单文件 WAL，单 writer / 多 reader，CRC32C，可选 buffered 与 O_DIRECT）
> 评审方式：三名独立审计（写路径/持久性、读路径/格式、性能/API）+ 主评审交叉验证。
> 所有结论都在本机实测复现过；仓库代码未被修改（`git status` 干净），本文件是唯一新增文件。
> 复现脚本位于 `/tmp/walworm-review/{lead,durability,reader,perf}`。

> **更新（2026-11-03）：初轮修复及后续复核结果见下文。** 下方的逐条分析保留为历史评审记录，
> 每条对应的实现位置与状态见下面的「修复状态」表。初轮修复的验证结果：
> `cargo test --all-targets`（46 单测 + 31 集成 + 6 健壮性）、`cargo test --doc`（2）、
> `cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`、
> `RUSTDOCFLAGS="-D warnings" cargo doc`、`cargo +nightly miri test --lib`（42 通过 / 1 跳过）、
> `cargo check --target x86_64-pc-windows-gnu`、`cargo +1.89.0 check --all-targets`、
> `cargo bench --bench throughput` 全部通过。

## 后续复核与修复

- **恢复水位：** `logical_position()` 包含尚未写入文件的 buffered 记录，不能用于保证恢复。
  新增 `Writer::durable_position()`，仅在文件同步成功后更新；写入或同步失败保留此前的
  durable 位置。错误注入测试覆盖丢弃 writer 后按此位置截断、重新读取的恢复流程。
- **EOF 检查点：** 读到 EOF 后，`Reader::position()` 保留在 EOF 标记起点；从该位置
  `open_at()` 会再次读到 EOF，不再永久返回 `Incomplete`。
- **读缓冲回收：** 大记录消费后的压缩会按剩余数据缩减过大的容量，覆盖 Direct I/O
  尾部仍有零填充的情形，同时保留日常读取容量，避免轮询时反复分配。
- **错误契约：** `close()` 失败保持可重试，不 poison；EOF 可能在同步成功前已可见。
  `RecordTooLarge` 在 CRC 校验之前产生，不能据此断言数据完好。
- **Direct I/O 测试：** 探测中的非预期错误必须使测试失败；活跃 Direct I/O writer
  使用 Direct I/O reader 测试，CI 要求相应场景实际执行。
- **接受的模型假设：** 保留 Direct I/O 的尾块重写策略；持久性模型假设重写会保留
  已提交的前缀字节。本次没有更改文件布局或实现防撕裂的尾块提交协议。

本次验证：debug/release 各 76 单测 + 34 集成测试 + 6 健壮性测试、2 doctest、
Clippy、格式检查、严格 rustdoc、Rust 1.89 与 Windows target 编译检查均通过；
Miri 为 69 通过、3 项按配置跳过。独立复现确认 EOF 检查点恢复后返回 `None`；
8 MiB 记录后的等待阶段存活分配从约 16.1 MiB 降至约 1.1 MiB。

## 修复状态

| 编号 | 状态 | 落地位置 / 说明 |
|---|---|---|
| C1 | ✅ | `Writer::flush`/`close` 无条件 `fdatasync`（不再有 `if !direct`）；README/lib/writer 文档修正为「O_DIRECT 只绕过 page cache，不是持久化」 |
| C2 | ✅ | `write_batch` 所有错误路径回滚 `buf`+`data_end`，并增量检查上限；真实 4.2 GiB batch 复现验证：失败后日志仍完整可读 |
| C3 | ✅ | reader 保留 header `block_size`；全零 header 仅在 direct 文件、零串到 EOF 且长度 < block 时算 padding，否则 `Corruption`；非 direct 文件零 header 一律损坏 |
| C4 | ✅ | Writer 状态机 `Open/EofWritten/Closed/Poisoned`；EOF 写入后 `write` 返回 `Closed`；重试 `close` 只重试 I/O、不重复写 EOF |
| C5 | ✅ | 写失败回滚 `data_end`+`buf` 并 poison；恢复使用新增的 `durable_position()`，`logical_position()` 只表示逻辑长度；`Drop` 在 poison 后不再 I/O |
| C6 | ✅ | `create` 顺序：写 header → `fdatasync(file)` → `fsync(parent dir)`；裸相对路径 map 到 `"."`；任一步失败删除本次创建的文件 |
| C7 | ✅ | 共享 `MAX_RECORD_SIZE`=1 GiB 作为两侧默认；reader 超限返回 `RecordTooLarge{len,max}` 而非 `Corruption` |
| C8 | ✅ | Windows 用 `seek_read`/`seek_write`；其它平台返回 `Unsupported`；`cargo check --target x86_64-pc-windows-gnu` 通过 |
| C9 | ✅ | batch 改为 O(1) 惰性 `BatchCursor`（只存绝对偏移，逐项按需解析），不再 `Vec<Vec<u8>>` / `Vec<(u64,u64)>` |
| C10 | ✅ | `pbuf` 消费完或压缩后按需缩减，覆盖尾部仍有 Direct I/O padding 的情形；EOF 时释放 |
| C11 | ✅ | 零 item 的 BATCH 记录（len=0）判为 `Corruption` |
| C12 | ✅ | `create` 失败清理本次文件（保留预存文件）；`O_DIRECT` 的 `EINVAL` 映射为 `Unsupported` |
| P1 | ✅ | README 增加 57× 写放大警告；`benches/throughput.rs` 可复现（69k vs 21.9M rec/s） |
| P2 | ✅ | `next_record_into()` / `next_record_borrowed()`；每条记录分配 1.001 → ~0；`Reader::open` 0 分配 |
| P3 | ✅ | `record_crc` 由 3 次 update 改 2 次（CRC 值不变，单测保证）；CRCs-at-end 的格式 v2 属于破坏性变更，未做 |
| P4 | ✅ | `Reader::open` 只读 16 B（direct 读一个 block），`chunk_buf`/`staging` 延迟分配 |
| P5 | ✅ | `dirty` 标志：无新数据时 `flush` 不再重复 `fdatasync` |
| P6 | ✅ | staging 精确按 block 对齐长度分配（去掉 `next_power_of_two`）；`AlignedBuffer` 尺寸取整修正 |
| A1 | ✅ | `Reader::open_at(path, options, offset)` 支持从 `position()` 检查点续读，包括 EOF 检查点 |
| A2 | ✅ | 借用式/复用缓冲 API（见 P2） |
| A3 | ✅ | `Error`/options `#[non_exhaustive]`、`FusedIterator`、`Error: PartialEq + Eq` |
| A4 | ✅ | 错误细分：`NotAWalworm` / `UnsupportedVersion` / `UnknownRecordType` / `RecordTooLarge` / `Poisoned`；`is_incomplete()`/`is_corruption()` |
| A5 | ✅ | `position()` 文档改为「下一条物理记录」，并说明 batch 语义 |
| A6 | ⚠️ 部分 | header `block_size` 已保留、用于 padding 消歧并暴露 `Reader::file_block_size()`；`statx(STATX_DIOALIGN)` 自动探测未做（需要 unsafe syscall，留作后续） |
| A7 | ✅ | Writer 状态机 + `Drop` 语义文档（buffered 数据可能丢失） |
| A8 | ✅ | README/lib/writer/reader 的不实声明全部修正（持久性、平台、`flush` 措辞、`buffer_capacity`、`aligned.rs` 注释） |
| D1 | ✅ | CI 增加 `cargo test --doc` |
| D2 | ⚠️ 替代 | 未引入 `cargo-fuzz`（该环境离线无法构建 libfuzzer）；改为 `tests/robustness.rs`：全偏移截断、逐字节变异、随机输入、零 header，约 1 秒内跑完 |
| D3 | ✅ | CI 增加 Windows target `cargo check` |
| D4 | ✅ | docs 步骤加 `RUSTDOCFLAGS="-D warnings"` |
| D5 | ✅ | CI 增加 miri job；本地 `cargo +nightly miri test --lib` 通过（O_DIRECT 用例按 `cfg(miri)` 跳过） |
| D6 | ⚠️ 部分 | 已加 `[package.metadata.docs.rs]`；`cargo-deny`/`cargo-audit` 涉及依赖策略决策，未自动加入 |
| D7 | ✅ | 新增错误注入单测（`Faults`）、回归/健壮性测试、`open_at`/限额/错误路径覆盖 |
| D8 | ✅ | 去掉 `lib.rs` 中与 `Cargo.toml [lints]` 重复的 `missing_docs` |
| D9 | ✅ | `rust-version = "1.89"` 实测通过（`cargo +1.89.0 check --all-targets`）；CI 矩阵已固定 1.89.0 |

**修复过程中又发现并修掉一个问题（评审时未列）**：`Reader::open` 读文件头时只调用一次
定位读，把「合法的短读」误判为 `NotAWalworm`。已在 `src/file.rs` 增加
`read_exact_at`（循环重试短读，仅 EOF 返回 `false`）并用于 header 读取。这个问题是
Miri 的短读模拟暴露出来的，真实文件系统上也可能发生（信号中断/特殊 FS/模拟层）。

**补充加固：EINTR 重试。** POSIX 允许 `read`/`write`/`pread`/`pwrite`/`fsync` 在未传输
任何数据时返回 `EINTR`，正确做法是用相同参数重试。`src/file.rs` 的所有 I/O 辅助函数
（`write_all_at`、`read_at`、`sync_data`、`sync_parent_dir`）现在统一通过
`retry_interrupted` 重试 `ErrorKind::Interrupted`，其它错误照常上报；已加单测覆盖
「多次 EINTR 后成功」和「非 EINTR 错误不重试」。

---

## 0. 总体评价

先说优点，这些是真实水平：

- **API 设计克制且自洽**：单写者 + 多读者 + 追加写 + 原子 batch + 显式 EOF 标记，模型简单，`flush()` 作为唯一持久化入口的想法很好。文档（README / rustdoc）质量明显高于一般 0.1 版本。
- **工程质量扎实**：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`（含 `pedantic`）全绿；13 单测 + 14 集成测试 + 2 doctest 全过；集成测试里已经有 model-based fuzz（4 种模式组合）、direct I/O 探测、live tail 并发等，覆盖面超出多数同规模 crate。
- **unsafe 有节制**：只有 `aligned.rs` 一处，带 SAFETY 注释；实测 `cargo +nightly miri test` 通过（3 个 aligned 单测 + 13 个 `--lib` 用例），未发现 UB。
- **读路径状态机经得起 fuzz**：2 万次随机输入、1.6 万次截断、4 千次变异、3 千条记录的 direct 并发竞态，均只出现 `Ok/Incomplete/Corruption/Io/BadHeader`，无 panic、无死循环，`Ok` 数据与模型逐字节一致。

但是——**这是一个 WAL，持久性是它的全部意义**。而当前 `direct_io` 模式下的持久化语义是错的，错误路径也有几处会静默损坏/丢失日志。这些必须排在所有性能优化之前。

**一句话结论**：架构没问题，问题集中在「错误路径 + crash 语义 + direct I/O 的持久性假设」；性能上真正值得做的只有两项（documented 掉 direct-unbuffered 陷阱 + 零拷贝读 API），其余是锦上添花。

---

## 1. 优先级总览

| # | 级别 | 问题 | 位置 | 一句话影响 |
|---|---|---|---|---|
| C1 | **Critical** | `direct_io` 下 `flush()`/`close()` 完全跳过 `fdatasync`，且文档声称「已完成写入即可持久」 | `writer.rs:282-291`, `:303-321`, `:24-26` | 掉电/崩溃可丢失已确认成功的数据，WAL 的核心保证失效 |
| C2 | **Major** | `write_batch` 失败（超限）后不回滚 `buf`，破坏 `buf.len == data_end - write_pos` 不变量 | `writer.rs:236-268`, `:386-389` | 后续记录写到错误偏移、日志从该点起永久不可读（已用真实 4.2 GiB batch 复现） |
| C3 | **Major** | 全零 9 字节 header 一律被当作 Direct I/O padding | `reader.rs:311-322`, `:386-397` | 崩溃后全零 header → 永远 `Incomplete`（永不 `Corruption`），其后的有效记录与 EOF 全部不可达 |
| C4 | **Major** | `close()` 失败后 `closed` 仍为 `false`，EOF 之后仍可写 | `writer.rs:303-321` | 这些记录被 writer 接受但 reader 在 EOF 处停止，静默丢失（故障注入已复现） |
| C5 | **Major** | I/O 错误语义不明确：返回 `Err` 的记录仍可能落盘、`position()` 已计入、`Drop` 在返回 Err 后继续写 | `writer.rs:201-215`, `:405-414` | 调用方无法判断重试是否会造成重复/丢失，`position()` 不能当恢复水位 |
| C6 | **Major** | `create()` 先持久化目录项、后（且从未）持久化文件内容；裸相对路径（README 示例）的 dir fsync 静默失效 | `writer.rs:178-187`, `file.rs:95-101` | 一次崩溃可留下永久不可用的 WAL 路径（无法重开、reader 报 BadHeader） |
| C7 | **Major** | writer 允许 ≤4 GiB 记录，reader 默认上限 1 GiB，且超限报 `Corruption` | `format.rs:52`, `reader.rs:44`, `:326-328` | 自己写的记录自己读不了，还被告知「CRC 校验失败、后面没有有效数据」（已复现 1.06 GiB 记录） |
| C8 | **Major** | 非 Unix 平台无法编译 | `file.rs:60`, `:79` vs `writer.rs:379,386`, `reader.rs:233,269,282` | README:「should build elsewhere」为假；`cargo check --target x86_64-pc-windows-gnu` 6 个错误 |
| C9 | **Major** | batch 逐项 `Vec<Vec<u8>>`，磁盘→内存放大约 7.4× | `reader.rs:421-437`, `:347-352` | **合法文件**可由 reader 触发 OOM abort（1 GiB batch ≈ 6.4 GiB 内存，进程直接 abort） |
| C10 | Minor | 单条大记录内存放大 2–3×；`max_record_size` 默认 1 GiB 过于宽松 | `reader.rs:139-140`, `:333-334`, `:44` | 64 MiB 记录实测峰值 192 MiB，且 pbuf 容量在 reader 生命周期内不回收 |
| C11 | Minor | 空 BATCH 记录（len=0）在返回错误时已推进 cursor | `reader.rs:347-352`, `:421-437` | 违反「出错不推进 cursor」的文档契约（仅构造文件可达） |
| C12 | Minor | `create()` 失败不留清理；Linux 上不支持 O_DIRECT 的 fs 返回 `Io(EINVAL)` 而非 `Unsupported` | `writer.rs:159-187`, `file.rs:31-37` | 留下 0 字节脏文件，后续 `create` 永远 `AlreadyExists` |

性能侧（实测，见 §3）：

| # | 级别 | 优化 | 收益（实测） |
|---|---|---|---|
| P1 | **High** | `direct_io` 不配 `buffered` 是性能陷阱：64 B 记录写放大 **57.2×** | 69.4k rec/s vs direct-buffered **8.12M rec/s（117×）**；文档应警告或自动 buffering |
| P2 | **High** | 零拷贝/复用缓冲读 API（`read_into`、借用式、batch 延迟解析） | 读吞吐 **+7.8% @64 B / +22.7% @64 KiB**；分配 **1.001 → ~0/记录**；顺带解决 C9 |
| P3 | Med | CRC 从「3 次 update」改「2 次」 | CRC 每记录 18.25 ns → 13.28 ns（−27%），小记录吞吐约 +5–10% |
| P4 | Med | `Reader::open` 为 16 字节 header 读/分配 64 KiB（direct 128 KiB） | open 15.9 µs → ~8 µs；direct open 123–144 µs（对 tailing 重开很重要） |
| P5 | Low-Med | `flush()` 无 dirty 标记，无新数据也 `fdatasync` | 每次多余 flush 省 53–152 µs |
| P6 | Low | staging 用 `next_power_of_two` 可浪费 2× 内存 | 1 MiB+4 KiB → 2 MiB 分配 |

---

## 2. 正确性问题详述

### C1 [Critical] `direct_io` 下 `flush()` / `close()` 根本不是持久化屏障

**代码**：`writer.rs:282-291`（`flush`）、`writer.rs:303-321`（`close`）里都是

```rust
self.write_pending()?;
if !self.direct {                 // ← direct_io 时整个 fdatasync 被跳过
    file::sync_data(&self.file)?;
}
```

`writer.rs:24-26`、`lib.rs:20-25`、`README.md:83` 都明确写「Direct I/O 模式下完成的写已视为持久，`flush` 不需要 `fdatasync`」。

**这是错的。** Linux `open(2)` 原文：

> **O_DIRECT** … The O_DIRECT flag on its own makes an effort to transfer data synchronously, but **does not give the guarantees of the O_SYNC flag that data and necessary metadata are transferred**. To guarantee synchronous I/O, O_SYNC must be used in addition to O_DIRECT.

`O_DIRECT` 只绕过 page cache，不绕过 **设备/阵列的易失写缓存**；追加写还要持久化 `st_size`/extent 这类元数据（`fsync(2)`：`st_size` 变更需要 metadata flush）。内核文档 `block/writeback_cache_control` 与 LWN《Ensuring data reaches disk》都明确：O_DIRECT 之后仍需 `fsync`。

**实测证据**（`strace -f -e trace=pwrite64,fdatasync,fsync`）：

- 普通模式：`pwrite64` 若干 + **`fdatasync` × N** + `fsync(dir)` ×1；
- `direct_io`（含 buffered）：`pwrite64` 若干 + **`fdatasync` 0 次**，只有 `sync_parent_dir` 的 dir `fsync`；
- 打开标志：`O_WRONLY|O_CREAT|O_EXCL|O_DIRECT|O_CLOEXEC` —— **没有 `O_DSYNC`**。

**影响**：`flush()` 是本 crate 唯一文档化的持久化边界，`close()` 是「写完了」的边界。两者返回 `Ok` 之后掉电，已确认的记录仍可能丢失；最坏情况下文件大小/extent 回滚，`create()` 之后的全部数据消失，而所有调用都返回成功。这不是可接受的 trade-off——**API 没有提供任何方式获得真正的持久化**。

**修复（推荐 a，最简单且正确）**：

```rust
// writer.rs flush() / close()
self.write_pending()?;
file::sync_data(&self.file)?;      // 不论是否 direct_io 都执行
```

若确实想保留「不做 fsync」的快速路径，请显式建模而不是默认：

```rust
pub enum Durability { Data, Os }   // Data = 持久化；Os = 仅交给 OS
pub fn flush(&mut self) -> Result<()>;             // 永远 Durability::Data
pub fn spill(&mut self) -> Result<()>;             // 仅可见性，不 fsync
```

并同步修正 `README.md:83`、`lib.rs:20-25`、`writer.rs:24-26,271-277` 的措辞：**O_DIRECT 去掉的是 page cache，不是持久化**。同时建议加一条 CI 断言（strace 或 test-only 的 syscall hook）确保 direct 路径也有 `fdatasync`。

---

### C2 [Major] `write_batch` 失败后缓冲区状态被破坏 → 日志静默损坏（已在真实 crate 复现）

**代码**：`writer.rs:236-268`

```rust
let hdr = self.buf.len();
self.buf.resize(hdr + RECORD_HEADER_SIZE, 0);   // 9 字节全零占位 header
...
for item in items {
    if bytes.len() as u64 > u64::from(u32::MAX) { result = Err(RecordTooLarge); break; }
    self.buf.extend_from_slice(...);            // 已追加的内容
}
let payload_len = self.buf.len() - payload_start;
if result.is_ok() && payload_len as u64 > MAX_PAYLOAD_LEN { result = Err(RecordTooLarge); }
if count == 0 { self.buf.truncate(hdr); return result; }   // 只有 count==0 回滚
result?;                                        // ← count>0 的 Err 直接返回，buf 留着垃圾
```

**实测复现**（真实 crate，`/tmp/walworm-review/lead`）：写 1 条 `good-1`，然后 `write_batch(4200 × 1 MiB)`（payload 4.2 GiB > `u32::MAX`）→ 返回 `RecordTooLarge`；再 `write("good-2")` + `close()` 都返回 `Ok`。结果：

```
write_batch error: record exceeds the maximum supported size
file size = 4.2 GB              ← 而按逻辑位置算应该只有几十字节
reader: ["good-1"]  然后永远 Incomplete（good-2 与 EOF 不可达）
```

（把上限调小后的同路径复现可以直接看到 `position()` 与实际文件长度脱节：`position()=29` 而文件 88 字节。）

机理：残留的 9 字节全零 header 正好命中 C3 的「padding 判定」，reader 从此认为「尾部还没写完」。同时 `write_pending` 里 `self.write_pos = self.data_end`（`:386-389`）假设文件尾 == `data_end`，而错误路径后两者已不一致，后续写会落在错误偏移。

**修复**：所有 `Err` 分支都回滚，并且**在追加前**做增量上限检查（避免先吃掉 4 GiB 内存再报错）：

```rust
for item in items {
    let bytes = item.as_ref();
    if bytes.len() as u64 > MAX_PAYLOAD_LEN { self.buf.truncate(hdr); return Err(Error::RecordTooLarge); }
    if (payload_len + 4 + bytes.len()) as u64 > max_payload { self.buf.truncate(hdr); return Err(Error::RecordTooLarge); }
    ...
}
// 统一的错误收口：if let Err(e) = result { self.buf.truncate(hdr); return Err(e); }
```

并加单测：失败 batch 之后 `position()`、文件长度、后续记录可读性都不变（用 test-only 的小上限即可覆盖）。

---

### C3 [Major] 全零 header 被当作 padding：崩溃后永远 `Incomplete`，且永不报 `Corruption`

**代码**：`reader.rs:311-322`（`is_all_zero(header)` → `Parse::Padding`）、`reader.rs:386-397`（每个 `next()` 最多重读一次 → `Incomplete`）。

**关键事实**：合法的记录 header **不可能全零**。空 payload 的 DATA 记录 header 是 `00 00 00 00 35 76 72 45 00`（`crc32c([0;5]) = 0x45727635`），BATCH/EOF 的 type 也非 0。所以「9 字节全零」只可能是：Direct I/O 尾部 padding、或者**崩溃/扇区清零造成的损坏**。

**实测**（两种方式都复现）：

- 构造 `header + 有效记录 + 9 字节零 + 有效记录 + EOF`：reader 只拿到第一条，之后连续 20+ 次 `Incomplete`，`is_closed()==false`，后面那条有效记录和 EOF 永远不可达；把同样位置的 CRC 改坏则正确报 `Corruption`。
- 直接清零 crate 自己写出的文件中的第二个记录 header，同样永久 `Incomplete`。

**影响**：tailing 消费者会永远等待；恢复/扫描场景会静默掩盖损坏，且与 README「never skip past it silently」的承诺矛盾。更糟的是 C2 的残留垃圾正好是全零 header，两者叠加会把「一次失败的 batch」变成「日志从此不可读」。

**修复思路**：`format.rs` 的 header 里已经存了 writer 的 `block_size`，但 `reader.rs:210-211` 直接丢弃了——正好可以用来消歧：

```rust
let header = decode_file_header(&raw).ok_or(Error::BadHeader)?;   // 保留 header.block_size
...
if is_all_zero(header) {
    // 只有 Direct I/O writer 才可能有 padding
    if self.writer_block_size == 0 { return Parse::Corrupt; }
    // 且 padding 只可能出现在最后不足一个 block 的尾部：
    //   从 cursor 到 EOF 必须全是 0，且长度 < block_size；
    //   若 0 之后还有非 0 字节，或 0 串超过 block_size，则是损坏
    ...
}
```

即：**padding 是「零串一直延伸到 EOF 且不超过一个 block」**，其余一律 `Corruption`。这个改动同时把 C2 的残留从「永久 Incomplete」变成「明确 Corruption」。

---

### C4 [Major] `close()` 失败后 EOF 已写入，但 writer 仍可写 → 记录静默丢失

**代码**：`writer.rs:303-321`——先 append EOF 记录并 `write_pending()`，然后 `sync_data`；只有全部成功才 `self.closed = true`（`:319`）。若 `fdatasync` 失败（EIO/ENOSPC），`close()` 返回 `Err` 但 `closed == false`。

**实测**（LD_PRELOAD 注入 `fdatasync` EIO）：

```
close() -> Err(Io(EIO))
is_closed() = false
write("b-after-eof") -> Ok(())      ← writer 接受了
second close() -> Ok(())
reader -> ["a"], None               ← b 被静默丢弃，且看起来是干净的 EOF
```

**修复**：把 `closed: bool` 换成状态机 `Open → Closing → Closed | Failed`。一旦 EOF 记录写入，就拒绝 `write`；`sync` 失败则返回 `Err` 并停在 `Failed/Closing`，重试 `close()` 只重试 sync，不重复 append EOF（现在重试会追加第二个 EOF 记录）。

---

### C5 [Major] I/O 错误语义不明确：返回 `Err` 的记录仍可能落盘，`position()` 已计入

**代码**：`writer.rs:201-215` 先 `data_end += ...` 再 `maybe_spill()`；`write_pending()` 失败时 `buf` 保留（为了重试，这是对的），但 `data_end` 已经算进去了，`Drop`（`:405-414`）还会再试一次。

**实测**（注入第 2 次 `pwrite` EIO）：

```
write("aaaa") -> Err(EIO)
position() = 29                     ← 已包含这条失败的记录
write("bbbb") -> Ok(())
reader -> [aaaa, bbbb]              ← 返回 Err 的那条实际落盘了
```

也就是说：**`Err` 不代表「没写进去」**，而 `position()` 也不代表「已提交水位」。对 WAL 来说这会导致调用方无法安全重试（重复 vs 丢失二选一）。

**修复**：任一 `write/flush/close` 遇到 I/O 错误后进入 poisoned 状态（新增 `Error::Poisoned`），后续写直接拒绝；`Drop` 在出错后不再发起 I/O；文档明确「I/O 错误后必须丢弃 writer，日志有效范围以出错前最后观察到的位置为准」；如需恢复，提供 `Writer` 侧的截断/恢复辅助（见 A1）。

---

### C6 [Major] `create()` 的持久化顺序错误 + 裸相对路径的目录 fsync 静默失效

**代码**：`writer.rs:178-187`

```rust
writer.buf.extend_from_slice(&encode_file_header(block_size_field));
writer.data_end = FILE_HEADER_SIZE as u64;
writer.write_pending()?;            // header 只进了 page cache，从未 fdatasync
file::sync_parent_dir(path);        // 目录项先持久化
```

正确的建新文件顺序是：`create` → 写内容 → `fdatasync(file)` → **最后** `fsync(dir)`。当前顺序反了，且文件内容根本没 sync。

另外 `file.rs:95-101`：

```rust
if let Some(parent) = path.parent() {
    if let Ok(dir) = File::open(parent) { let _ = dir.sync_all(); }
}
```

`Path::new("example.wal").parent()` 是 `Some("")`，`File::open("")` 返回 ENOENT，错误被 `_` 吞掉——**README quick-start 用的正是这种裸文件名**，所以目录从没被 fsync 过。目录 fsync 的所有错误（含 EIO）也都被吞掉。

**影响**：崩溃窗口内可能留下「目录项已持久、文件 0 字节/header 写了一半」的状态；由于 writer 永不重开、也没有恢复 API，这个路径就永久废了（reader → `BadHeader`，writer → `AlreadyExists`）。

**修复**：

```rust
// file.rs
pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}
// writer.rs create(): 写 header -> fdatasync(file) -> sync_parent_dir(path)?
// 任意步骤失败：best-effort unlink + fsync(dir)，再返回 Err
```

并在文档里写清恢复方式：「header 不全/0 字节的 WAL 文件需调用方自行删除」。

---

### C7 [Major] writer/reader 尺寸上限不一致，且用 `Corruption` 表达「你的 reader 配置太小」

**代码**：`format.rs:52` `MAX_PAYLOAD_LEN = u32::MAX`（writer 接受 ≤4 GiB）；`reader.rs:44` 默认 `max_record_size = 1 GiB`；`reader.rs:326-328` 超限直接返回 `Corrupt` → `Error::Corruption`。

**实测**（真实 crate）：写入 1.0625 GiB 记录 → `write`/`flush`/`close` 全部成功；用**默认** `ReaderOptions` 读：

```
default reader: Err(record at offset 16 failed CRC32C verification)   ← 谎报，CRC 是对的
with_max_record_size(2 GiB): Ok(1140850688 bytes)
```

两个问题：(1) 默认配置下 crate 写得出、读不回；(2) 错误信息是「CRC 校验失败」，而文档又说 `Corruption` 意味着「后面没有有效记录了」——实际上后面的记录完全有效，调用方会被误导成「数据损坏、停止消费」。

**修复**：导出一个共享的 `MAX_RECORD_SIZE`（建议 1 GiB），writer 超限返回 `RecordTooLarge`；reader 因 `max_record_size` 判超限时返回 `Error::RecordTooLarge`（或新增 `Error::RecordTooLargeForReader { len, max }`）而不是 `Corruption`；两边默认值对齐并写进文档；补一个「刚好等于上限 / 上限+1」的边界测试。

---

### C8 [Major] 非 Unix 平台无法编译（README 声明相反）

**实测**：`rustup target add x86_64-pc-windows-gnu && cargo check --target x86_64-pc-windows-gnu` → **6 个错误**（`E0425: cannot find function write_all_at / read_at`），因为 `file.rs:60,79` 把 `write_all_at` / `read_at` 关在 `#[cfg(unix)]` 里，而 `writer.rs:379,386`、`reader.rs:233,269,282` 无条件调用；同时 `file.rs:49-57,103-104` 又为 non-unix 写了 stub，说明作者本意是支持的。

**修复**：二选一——(a) 补 `#[cfg(not(unix))]` 实现（Windows 用 `seek_read`/`seek_write`）；(b) 在 `lib.rs` 加 `#![cfg(unix)]` 并把 README 改成「Unix only」，CI 里显式验证 non-unix 会给出清晰报错而不是一堆 E0425。

---

### C9 [Major] batch 逐项 `Vec<Vec<u8>>`：合法文件可以 OOM abort reader

**代码**：`reader.rs:421-437` 为每个 item 分配一个 `Vec<u8>`，`Batch(Vec<Vec<u8>>)`（`:81`）。

**实测数字**：100 万个**空 item** 的 batch 在磁盘上只有 **3.81 MiB**（每项 4 字节长度），reader 里变成 **28.1 MiB** 常驻（7.4×）。按默认 `max_record_size = 1 GiB`，一个 payload 恰好 1 GiB 的 batch = 2.68 亿个空 item → 约 **6.4 GiB** 的 `Vec<Vec<u8>>`；`Vec::push` 分配失败会走 Rust 的 OOM abort，**不是 `Err`**，进程直接死。而且这种文件是 crate 自己的 `write_batch` 就能写出来的。

**修复（也是 P2 的核心）**：batch 改成**惰性解析**——校验 CRC 后只记住 payload 在 `pbuf` 中的区间和当前 item 偏移，`next()` 每次解出一个 item；`advance`/compaction 推迟到 batch 消费完。若暂时不想动，至少加 `try_reserve` + `max_batch_items` 上限 + 文档。

---

### C10 [Minor] 单条大记录内存放大 2–3×，pbuf 容量不回收

64 MiB 记录实测：第一次 `next()` 后常驻 **192.1 MiB**（`pbuf` 因 Vec 翻倍到 128 MiB + 返回的 64 MiB Vec），drop 掉记录并读到 EOF 后仍常驻 **128.1 MiB**。按默认 1 GiB 上限外推约 2–3 GiB/reader，且要等 reader 析构才释放。

修复：reader 在 `pbuf` 完全消费后 `shrink_to_fit()`/换出；配合 P2 的 `read_into`/借用 API 去掉第二次拷贝；把 `max_record_size` 默认值降到更合理量级（如 64 MiB）并文档化内存预期。

---

### C11 [Minor] 空 BATCH 记录在错误返回时已推进 cursor

`parse_batch_payload` 对 len=0 返回 `Some(vec![])`，`parse_once` 先 `advance(total)`，`next_record` 循环继续，于是「无 item 的 batch」被静默吞掉；若下一条又恰好 `Incomplete/Corruption`，返回错误时 cursor 已经动过（实测 16 → 25）。这与 `reader.rs:99-108`、`error.rs:14-20/29-32` 的「出错不推进 cursor」契约冲突，会破坏调用方的 checkpoint 逻辑。仅构造/损坏文件可达（`write_batch` 空 batch 是 no-op）。

修复：把 len==0 的 BATCH 视为 `Corruption`（顺带消掉这个不对称），或批量 yield 前不 advance。

---

### C12 [Minor] `create()` 失败不清理；不支持的 O_DIRECT 报 `Io(EINVAL)` 而非 `Unsupported`

`/dev/shm` 上请求 `direct_io`：open 返回 `EINVAL`，留下 0 字节文件；此后每次 `create` 都是 `AlreadyExists`。文档说这种情况返回 `Error::Unsupported`，实际是 `Io`。

修复：记录「文件是否由本次调用创建」，后续任一步失败则 best-effort unlink + fsync(dir)；把 O_DIRECT 的 `EINVAL` 映射为 `Unsupported`/`InvalidOptions`（提示「文件系统不支持 O_DIRECT 或 block_size 不匹配」）。更好的做法是用 `statx(STATX_DIOALIGN)`（Linux 6.1+）自动探测/校验 block size（见 A6）。

---

## 3. 性能：实测数据与建议

环境：AMD EPYC 9654，Linux 5.15，rustc 1.98，`/tmp` 为 overlayfs（**O_DIRECT 真实可用**，非跳过）。共享机器、负载 25–48，取 7 次中位数；绝对数会有 ±50% 波动，因此「优化前后」用的是同进程交错 A/B。完整日志在 `/tmp/walworm-review/perf/results/`。

### 3.1 写路径

| 模式 | 记录 | items/s | 备注 |
|---|---|---|---|
| unbuffered | 64 B | 0.72 M | 1 次 `pwrite64`/记录；≈ 裸 `File` baseline（0.70 M），framing+CRC 被 syscall 掩盖 |
| buffered (1 MiB) | 64 B | 3.28 M | p50 40 ns（纯内存拷贝） |
| **direct unbuffered** | 64 B | **69.4 k** | **每记录写 4173.8 B = 57.2× 放大** |
| **direct buffered** | 64 B | **8.12 M** | 1.06× 放大 |
| direct buffered | 64 KiB | 30.7 k | 2014 MB/s |
| batch 10×64 B | — | 3.82 M item/s | 0.1 次 `pwrite`/记录 |
| raw baseline | 64 B | 0.70 M | 结论：普通写路径没有优化空间 |

**P1（最重要）**：`direct_io` 本身没问题，**`direct_io` + 非 buffered** 是陷阱——每次小写都会重写整个 4 KiB tail block（57× 写放大），比普通 unbuffered 慢 **10.8×**，比 direct+buffered 慢 **117×**。建议：README/`WriterOptions` 明确「direct_io 只适合配合 buffered，或记录 ≥ block_size」，甚至在 `direct_io && !buffered && block_size > 预期记录大小` 时给出文档级警告。

**P6/P7（小）**：`reserve_staging` 的 `next_power_of_two` 会 2× 过分配（1 MiB+4 KiB → 2 MiB）；`flush()` 没有 dirty 标记，没新数据也会 `fdatasync`（每次 53–152 µs）。

**关于 unbuffered 写路径的 payload 拷贝**：`write()` 会把整个 payload 复制进 `self.buf` 再写。实测「header+payload 分两次 pwrite」的快速路径吞吐反而 **−4.9%/−5.8%/−2.0%**（多一次 syscall），**所以这不是吞吐优化**——它唯一的收益是内存：8 MiB 记录的临时分配从 8.39 MB 变 0。如果要做，用 Linux `pwritev`（`FileExt::write_vectored_at` 目前仍 unstable）保持一次 syscall；否则建议只在「记录 > 某个阈值」时走该路径。**不建议为了这个改动普通写路径。**

### 3.2 读路径

| 模式 | 记录 | items/s | 分配/记录 |
|---|---|---|---|
| 现状 | 64 B | 11.38 M | **1.001 次分配，129.5 B/op** |
| 原型：借用式 | 64 B | 12.42 M（**+7.8%**） | ~0.001 |
| 现状 | 64 KiB | 79.8 k | — |
| 原型：借用式 | 64 KiB | 83.2 k（**+22.7%**） | — |
| 原型：`read_into` 复用 Vec | 64 KiB | 89.9 k（**+12.5%**） | 0.002 |

**P2**：`Iterator<Item = Result<Vec<u8>>>` 强制每条记录一次堆分配（我用计数 allocator 独立验证：10 万条记录 → **100,006 次分配**；5 万 item 的 batch → **50,020 次**）。原型（借用式 `next_record_borrowed` + `next_record_into` + batch 区间惰性解析 + 延迟 compaction）通过原测试套件，读吞吐 +7.8%/+22.7%，分配降到 ~0，并顺带解决 C9 的 batch 内存炸弹。**这是读路径唯一值得做的大改动。**

**P3**：`record_crc` 现在 3 次 `Digest::update`（len / type / payload，18.25 ns @64 B）。改成 2 次（写侧传 5 字节 `len||type` 前缀；读侧先 4 字节 len 再 `type||payload` 连续区）→ 13.28 ns（−27%），小记录整体约 +5–10%。把 CRC 放到记录尾部可以让它变成一次连续 `checksum`（6.84 ns），但那是格式 v2 的破坏性变更。**注意：`checksum_combine` 实测 5130 ns，千万别用。**

**P4**：`Reader::open` 只为校验 16 字节 header 就分配并读 64 KiB（direct 128 KiB）：open 15.9 µs → 约 8 µs；direct open 实测 **123–144 µs**。对「反复重开跟随」的 tailing 场景值得改：非 direct 读 16 B，direct 读一个 block。

**负结果（不要做）**：把非 direct 的 `fill()` 改成「直接读进 `pbuf` 的 spare capacity」实测 **−8.3% @64 B**（`resize` 的 memset 替代了省下的 memcpy），64 KiB 仅 +4.7%。放弃，除非未来用 `read_buf`/未初始化读取。

---

## 4. API / 设计建议

**A1（重要）`Reader::open_at(path, options, offset)`：断点续读/恢复。**
当前只有 `open()`，一个 tailing 消费者重启后只能从头重放。WAL 的典型用法（checkpoint、崩溃恢复）必须有「从某偏移开始读」。配合 C5，还需要一个明确的「已提交水位」概念。注意入参 offset 必须是记录边界（可在文档说明，失败时返回 `Corruption` 或新增 `Error::BadOffset`）。这与 `position()` 一起构成可用的 checkpoint API。

**A2 借用式/复用缓冲的记录 API。** 见 P2。建议 `next_record_into(&mut Vec<u8>) -> Option<Result<()>>`（最简单、不引入生命周期难题）+ 可选的 `next_record_borrowed() -> Option<Result<&[u8]>>`。`Iterator<Item = Result<Vec<u8>>>` 保留为便捷层。

**A3 trait/属性补齐（都是低风险小改动）**：
- `impl FusedIterator for Reader`（`next()` 在 `None` 之后确实一直 `None`）；
- `Error`、`WriterOptions`、`ReaderOptions` 加 `#[non_exhaustive]`（现在加变体/字段就是 breaking change，趁 0.1 定下来）；
- `Error` 手写 `PartialEq`（按 variant + `io::ErrorKind`）并提供 `is_incomplete()`，测试与调用方都会用到。

**A4 错误体系细化。**
- `Corruption` 现在混用了三种语义：真 CRC 失败、长度超过 reader 配置（C7）、未知记录类型。建议拆成 `Corruption { offset }`、`RecordTooLarge { offset, len, max }`、`UnknownRecordType { offset, tag }`。
- `BadHeader` 无法区分「不是 walworm 文件」「版本不支持」「文件太短」。建议 `NotAWalworm` / `UnsupportedVersion { found, supported }`。
- 目前无法区分「可重试的 transient corruption」与「确定的损坏」：建议 `scan()`/`tail_status()` 返回 `CleanEof | TornTail { offset } | Corruption { offset }`。另外 `reader.rs:103-106`（direct 下 transient）与 `error.rs:21-28`（no valid record can follow）的措辞互相矛盾，需要统一。

**A5 `position()` 的文档语义。** 已实测：batch 首 item 出来时 `position()` 已经指向**整条 batch 之后**（31→50），而第二个 item 还没 yield。文档写的是「next record to be yielded」，与事实不符。要么改成「下一条**物理记录**的偏移 / 当前 batch 的结束位置」，要么按 item 暴露偏移。

**A6 利用 header 里的 `block_size`，并用 `statx` 自动探测。**
`format.rs:82-103` 存了 writer 的 block size，`reader.rs:210-211` 解出来就扔了。保留它可以：(a) 修 C3；(b) 给 direct reader 自动配置对齐；(c) 发现 writer/reader block_size 不匹配时给出清晰诊断。更现代的做法是 `statx(STATX_DIOALIGN)` 读取设备真实对齐要求，避免在 16 KiB 逻辑块设备上默认 4096 直接 `EINVAL`。

**A7 Writer 状态机。** `closed: bool` 不足以表达 `Open/Closing/Closed/Failed`（C4/C5）。同时 `Drop` 里吞掉 I/O 错误应写进文档（buffered 数据在 drop 时可能静默丢失）。

**A8 文档一致性（小但影响信任）**：
- `README.md:83`、`lib.rs:20-25`、`writer.rs:24-26`：O_DIRECT 持久性说法错误（C1）；
- `README.md:115-116`：「should build elsewhere」为假（C8）；
- `writer.rs:274-275`：「returns success without any further system call」在 direct+buffered 有 pending 时是假的（会 `pwrite`，实测 spill p50 49 µs）；
- `writer.rs:36-39`：`buffer_capacity` 文档说「exceeds」，代码是 `>=`；且它**不是**内存上界——大于 capacity 的单条记录会被整条缓冲；
- `aligned.rs:19-24`：注释说「size 要向上取整到 align 的倍数，因为 Layout 要求」——`Layout` 没有这个要求，代码也没取整（只是 `size.max(align)` 保证非零）。建议改成 `debug_assert!(size % align == 0)` 或显式 `align_up`，否则未来某天 `len()` 作为 DIO 传输长度可能非 block 对齐。

---

## 5. CI / 工程 / 测试

| # | 问题 | 证据/建议 |
|---|---|---|
| D1 | **doctest 从未在 CI 运行** | `ci.yml:26,28` 用 `cargo test --all-targets`，它**不包含 doctest**（我用「故意失败的 doctest」在临时 crate 验证：`--all-targets` 通过、`--doc` 失败）。而库里有 2 个真 doctest（`lib.rs:38-63`、`reader.rs:112-130`）。加 `cargo test --doc` 或直接 `cargo test`。 |
| D2 | 无 benchmark / fuzz target | 解析器（`parse_once`/`parse_batch_payload`）是纯函数式状态机，非常适合 `cargo-fuzz`（`fuzz/fuzz_targets/parse.rs`），配合 `proptest` 做 round-trip。性能回归也需要 `benches/`（criterion）。 |
| D3 | 无跨平台编译检查 | 因为 C8，建议 CI 加 `cargo check --target x86_64-pc-windows-gnu`（或明确 `#![cfg(unix)]` + `compile_error!` 的清晰提示）。 |
| D4 | docs 步骤不 fail | `ci.yml:44-45` 没有 `RUSTDOCFLAGS="-D warnings"`；`RUSTFLAGS` 不作用于 rustdoc。当前其实干净，加上是免费加固。 |
| D5 | 有 unsafe 却没有 miri job | 实测 miri 通过；加 `cargo +nightly miri test --lib`（加 `-Zmiri-disable-isolation`）可防回归。 |
| D6 | 供应链/发布 | 无 `cargo-deny`/`cargo-audit`；无 `[package.metadata.docs.rs]`；无 `cargo package --list` 的 dry-run（包装清单目前是合理的：`.github/` 已 exclude）。`Cargo.lock` 被 gitignore，对 library 可接受，但 dev-deps 会漂移。 |
| D7 | 测试缺口 | ① 错误注入（像我们用 LD_PRELOAD 那样，或给 I/O 加 test-only trait）覆盖 C2/C4/C5；② 边界：`max_record_size`/`u32::MAX`/0 长度/空 batch；③ `open_at` 恢复；④ crash 语义（至少要测 direct 模式确实发了 `fdatasync`）；⑤ 多进程 reader（README 声称支持）；⑥ 同文件 mixed direct/buffered 的 page cache 竞态（`open(2)` 明确警告不要混用，而默认 reader 就是 buffered）。 |
| D8 | 小项 | `missing_docs` 在 `Cargo.toml:24-25` 和 `lib.rs:92` 重复；可考虑 `clippy::undocumented_unsafe_blocks`。 |
| D9 | MSRV | `rust-version = "1.89"` 是准确的（由 `crc-fast 1.10` 的 MSRV 决定；1.88 被 cargo 直接拒绝，1.95 可构建），CI 矩阵已钉 1.89.0 —— 这项没问题。 |

---

## 6. 建议的落地顺序

**P0（当天，改动都很小，但决定 crate 是否可信）**
1. **C1**：`flush()`/`close()` 无条件 `fdatasync` + 修文档（约 3 行 + 文案）。
2. **C7**：共享 `MAX_RECORD_SIZE`，reader 超限返回 `RecordTooLarge` 而非 `Corruption`。
3. **C8**：README 改成 Unix only（或补 non-unix 实现/`compile_error!`）。

**P1（一周内，错误路径 + crash 语义）**
4. **C2**：`write_batch` 全路径回滚 + 增量上限检查 + 单测。
5. **C4/C5**：Writer 状态机（Open/Closing/Closed/Failed）+ poisoned 语义 + 文档。
6. **C3**：用 header 的 `block_size` 消歧 padding，零串必须延伸到 EOF 且 ≤ 1 block；顺带修 C11。
7. **C6**：`create` 顺序改为 写 header → `fdatasync(file)` → `fsync(dir)`；修裸相对路径；失败时 unlink。
8. **C9**：batch 惰性解析（可与 P2 的零拷贝读 API 一起做，一次改到位）。

**P2（性能，有实测收益）**
9. **P2**：零拷贝读 API（`read_into` 优先，借用式其次）+ `Reader::open` 只读 header + batch 区间解析。
10. **P1 文档**：direct_io 未 buffered 的 57× 写放大警告。
11. **P3/P5/P6**：CRC 2-update、flush dirty 标记、staging 精确大小。

**P3（API/工程）**
12. **A1** `open_at` + 与 `position()` 配套的 checkpoint 语义。
13. **A3/A4** `#[non_exhaustive]`、`FusedIterator`、错误细分、对照文档统一措辞。
14. **D1–D5** CI 补齐（doctest、fuzz、bench、miri、跨平台、`RUSTDOCFLAGS`）。

---

## 7. 验证方法（可复现）

本报告不是纯代码阅读，关键结论都有实测：

```bash
# 基线
cargo test --all-targets && cargo test --doc && cargo clippy --all-targets -- -D warnings

# C1：direct 模式下 flush/close 不产生 fdatasync
strace -f -e trace=pwrite64,fdatasync,fsync \
  /tmp/walworm-target-lead/release/leadprobe flush-syscalls

# C2：真实 crate，4.2 GiB batch 触发 RecordTooLarge 后日志损坏
/tmp/walworm-target-lead/release/leadprobe batch-error

# C3/C7/position 语义
/tmp/walworm-target-lead/release/leadprobe zeroed-header
/tmp/walworm-target-lead/release/probe2 size-mismatch
/tmp/walworm-target-lead/release/leadprobe position-after-batch

# C4/C5：LD_PRELOAD 注入 fdatasync/pwrite EIO
WALWORM_FAIL_FDATASYNC_AT=1 LD_PRELOAD=.../libfault.so CASE=2 .../faultprobe
WALWORM_FAIL_PWRITE_AT=2     LD_PRELOAD=.../libfault.so CASE=1 .../faultprobe

# C8：Windows 目标编译
cargo check --target x86_64-pc-windows-gnu

# 分配计数 / 吞吐 / 零拷贝原型 A/B
/tmp/walworm-target-lead/release/alloc
/tmp/walworm-target-lead/release/bench
# 原型：/tmp/walworm-review/perf/walworm-opt（原测试套件全过）
```

**未能验证 / 需保留的不确定性**
- 真实掉电/崩溃行为：无崩溃一致性测试环境，C1/C6 基于内核文档语义 + strace 证据（未观察到实际丢数据）。
- 4K/16K 原生扇区设备、ext4/xfs/btrfs 上的 O_DIRECT 对齐行为（本机只有 overlayfs + tmpfs）。
- 多 reader/多进程扩展性、冷缓存读吞吐（无 root 清 page cache）。
- 跨平台实现（Windows 只验证了「不能编译」，没有验证修复）。

---

## 8. 附：最小补丁草案（供参考，未应用）

```rust
// ---- C1: writer.rs ----
pub fn flush(&mut self) -> Result<()> {
    if self.closed { return Ok(()); }
    self.write_pending()?;
    file::sync_data(&self.file)?;   // 去掉 if !self.direct
    Ok(())
}
pub fn close(&mut self) -> Result<()> {
    if self.closed { return Ok(()); }
    self.append_eof_record()?;      // 抽出：写 EOF 到 buf
    self.write_pending()?;
    file::sync_data(&self.file)?;   // 去掉 if !self.direct
    self.closed = true;             // 建议改为状态机，见 C4
    Ok(())
}

// ---- C2: writer.rs write_batch 收口 ----
let mut payload_len = 0usize;
for item in items {
    let bytes = item.as_ref();
    let need = 4usize.checked_add(bytes.len()).ok_or(Error::RecordTooLarge)?;
    if (payload_len + need) as u64 > max_batch_payload {
        self.buf.truncate(hdr);     // 关键：先回滚
        return Err(Error::RecordTooLarge);
    }
    self.buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    self.buf.extend_from_slice(bytes);
    payload_len += need;
}
if payload_len == 0 { self.buf.truncate(hdr); return Ok(()); }
```

```rust
// ---- C3: reader.rs 保留 header，消歧 padding ----
let header = decode_file_header(&raw).ok_or(Error::BadHeader)?;
// self.writer_block_size = header.block_size;

fn classify_zero_header(&mut self) -> Parse {
    // 非 Direct I/O 写出的文件不可能有 padding
    if self.writer_block_size == 0 { return Parse::Corrupt; }
    // padding 只可能出现在最后不足一个 block 的尾部：
    // 从 cursor 到物理 EOF 必须全 0，且长度 < block_size
    // （超出 block_size 或 0 之后还有非 0 字节 => 损坏）
    ...
}
```

```rust
// ---- C6: create 顺序 + 裸相对路径 ----
writer.write_pending()?;                 // header
file::sync_data(&writer.file)?;          // 先持久化内容
file::sync_parent_dir(path)?;            // 最后持久化目录项
// 失败时：let _ = fs::remove_file(path); let _ = file::sync_parent_dir(path);
```

---

*报告结束。所有 file:line 均对应当前 HEAD（`9bec009`）。*

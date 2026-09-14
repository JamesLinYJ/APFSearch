# Changelog

## 0.1.1 Preview

### 中文

本次更新减少索引快照的重复分配、查询中的重复计算和输入到结果之间的固定等待。

- 索引记录使用分块写时复制，保留稳定槽位和旧快照；稀疏更新通过排序键定位旧条目，批量复制未变区间。
- 规范化名称、路径和父目录采用共享文本存储；扩展名和卷标识在快照中复用。SQLite 恢复直接构造最终记录，降低中间分配。
- 查询复用求值上下文，优先采用较稀疏的三字符候选，合并重复名称条件；连续槽位批量构造位图。
- 新写入的派生缓存集中存放搜索名称，改善查询访问局部性。缓存格式仍为 1；已有缓存不会因升级被强制重写。
- 使用受限、可复用的 Rayon CPU 池处理适合并行的工作；负载较小或工作池忙碌时直接串行执行。文件扫描并发和持久化策略不因此增加。
- 输入通知改为事件循环内合并，移除固定 16 ms 等待；保留取消、过期回复检查和原生输入法组字行为。结果表增长时不再重复创建新行。
- 离线索引恢复移出 XPC 请求处理线程；索引服务采用 Adaptive 调度。查询偏好读取复用内存状态，无变化的偏好不写入数据库。
- Rust 采用 Edition 2024，补全依赖许可说明。公开版本为 0.1.1，内部构建号及数据格式保持 1。

**测量范围：** 在隔离的百万条合成索引、已签名 XPC 服务和真实 AppKit 控制器中，三组交替测试的常见热查询首屏 P95 从约 23–25 ms 降到 6–9 ms（每类每轮 30 次测量）。不同测试阶段的内存和算法结果详见 [技术记录](docs/INCREMENTAL_INDEX.md)，不能相加作为整体加速倍数。查询测试的服务磁盘读写计数未增加；这不是所有使用场景零写入或 SSD 寿命保证。

**边界：** 仍为预览版。测量不包含物理键盘、输入法延迟或合成器最终显示；未完成真实 Intel Mac、最低系统版本、全盘工作负载、耗电和完整 Everything 行为验收。没有自动迁移或删除旧开发数据。未显示稳定端到端收益的自适应分页实验仅保留在测试代码中。

### English

This update reduces repeated snapshot allocation, query work and fixed input latency.

- Store stable entry slots in copy-on-write blocks; locate sparse sort-order removals by key and copy unchanged runs in bulk.
- Share normalized text storage and snapshot-owned metadata labels; stream SQLite recovery into final record allocations.
- Reuse query evaluation state, prioritize sparse trigram candidates, deduplicate repeated name conditions and build contiguous bitmap runs in batches.
- Place search names together when writing prepared caches, without changing format 1 or forcing existing caches to be rewritten.
- Reuse a bounded Rayon CPU pool with serial fallback; retain bounded filesystem I/O and existing durability settings.
- Coalesce input within the event loop instead of waiting 16 ms, retaining cancellation, stale-reply isolation and native marked-text handling. Avoid recreating newly bound result rows.
- Move offline-index opening off the XPC request handler, use Adaptive service scheduling and avoid unchanged preference writes.
- Adopt Rust Edition 2024 and update dependency notices. Public version is 0.1.1; internal build and data formats remain 1.

In three alternating signed million-record fixture comparisons, common warm-query input-to-AppKit-display-submission P95 changed from approximately 23–25 ms to 6–9 ms, with 30 measured samples per query class per run. Pages, totals and row counts matched, and service query-interval disk counters did not increase. See the [engineering notes](docs/INCREMENTAL_INDEX.md) for separately scoped memory and algorithm measurements; these gains must not be multiplied or treated as whole-machine results.

This remains a preview. Physical keyboard/IME latency, compositor presentation, real Intel hardware, the minimum supported OS, full-volume workloads, energy use and complete Everything parity remain unverified. Old development data is neither migrated nor deleted. Adaptive paging without a consistent end-to-end improvement remains test-only.

# Changelog

## 0.1.5 Preview

### 中文

- 修复大量硬链接下长时间停留在“扫描中”的根因：区分新路径发现与文件对象变化，在同一轮中复用已提交、元数据版本一致的核验结果，避免每发现一个路径就重新读取整组路径。
- 文件内容、身份、链接数或权限状态变化时仍重新核验；未知链接数、未完成核验、取消或事务失败不能留下可复用的成功记录。
- 挂载身份只包含实际影响读取边界的属性；无关卷挂载和挂载列表顺序变化不再打断当前批次。相关边界变化会重新排队受影响工作，保留未提交的事件进度及覆盖信息。
- 保留已有索引、设置、查询语义和派生缓存格式。构建号、数据库及公开协议版本仍为 1。
- GitHub Actions 自动验证、签名、公证并发布三种架构的 DMG/ZIP；应用和 DMG 均须通过公证与交付校验，凭据限定在 release 环境的临时钥匙串中。

### English

- Fix prolonged scanning with large hard-link groups by separating path discovery from object changes. Reuse committed, version-matched verification within the same pass instead of rereading the entire group for each new path.
- Reverify changed content, identity, link count and failed coverage. Unknown counts, incomplete verification, cancellation and failed commits cannot establish reusable success.
- Compare relevant mount boundaries using canonical identity. Unrelated mounts and enumeration-order changes no longer abort a batch; relevant namespace changes requeue the work while preserving pending event progress and coverage.
- Retain existing indexes, settings, search semantics and derived-cache format. Build, database and public protocol versions remain 1.
- Automate validation, signing, notarization and all three DMG/ZIP architecture variants with GitHub Actions. App and DMG checks gate publication; release-environment credentials use a temporary keychain.

**Validation / 验证：** 329 Rust tests and 121 Swift service/content/file checks passed on Apple Silicon, along with strict Clippy and localization checks. Real-file regressions cover 4, 32 and 128 hard links; unchanged discovery requires one initial peer read plus each path's own observation. The signed installed candidate completed recovery and historical replay, entered live watching, and passed creation, content-change, replacement, unlink, rename and removal checks. This is not a controlled whole-machine speedup or physical Intel/macOS 14 validation.

## 0.1.3 Preview

### 中文

- 修复索引设置页迟钝：覆盖路径按可见区域延迟创建视图，保留完整列表及文本选择。
- 服务回复在后台解码，覆盖范围使用固定长度摘要标识；状态更新不再反复拼接全部未覆盖路径。
- 文件图标使用共享、最多两个任务的后台队列及有界缓存；取消离开可见区域的排队请求，丢弃过期结果，不刷新整个表格。
- 简化完整磁盘访问引导：跳转设置后关闭介绍窗口，显示可拖入真实应用的小型辅助面板，返回应用后关闭。解释系统及文件权限仍可能限制覆盖范围。
- 保留八种语言，构建号、协议和数据格式保持 1，不重建或迁移已有索引。

### English

- Lazily create visible coverage rows to keep index settings responsive while retaining the full selectable list.
- Decode service replies off the main thread, identify coverage with a fixed-size digest, and avoid rebuilding full coverage tooltips during status updates.
- Load visible file icons through a shared two-worker queue and bounded cache; cancel obsolete queued work and discard stale results without reloading the table.
- Simplify Full Disk Access guidance: dismiss the introduction when opening Settings, provide a small panel for dragging the actual app, and close it when returning. Explain that system and file permissions can still restrict coverage.
- Preserve all eight languages and build/protocol/data version 1, without rebuilding or migrating existing indexes.

**Validation / 验证：** 284 Rust tests and 84 AppKit checks passed, with service-recovery and localization checks. In a controlled 888-path settings fixture, tab selection through layout/display submission fell from 242–257 ms to 13–14 ms median across reversed-order runs. This is not a whole-app or final-compositor latency measurement. Real Intel hardware and full-volume acceptance remain unverified.

## 0.1.2 Preview

### 中文

- 首次启动新增原生权限引导：解释完整磁盘访问的用途，跳转系统设置后保留可拖放应用的引导窗口，并演示拖入及开启开关。支持减少动态效果，也允许稍后设置。由用户选择索引范围后才开始扫描。
- 自动恢复应用与旧后台服务的协议不匹配：等待旧服务退出后重新注册，合并并发恢复请求，限制重试次数。
- 普通连接失败提供重新连接；仅在后台服务确实需要批准时引导系统设置。不自动重发连接中断的文件操作。
- 引导支持全部八种应用语言。保留独立的格式 1 数据目录，不迁移或删除旧开发数据。

### English

- Add native first-launch guidance for Full Disk Access, with a floating native drag-and-drop companion, a bounded drag/switch demonstration that respects Reduce Motion, an optional skip path, and explicit index-scope selection before scanning.
- Recover protocol mismatches by asynchronously unregistering the stale service, waiting for its exit, and registering the bundled service. Coalesce concurrent recovery requests and bound automatic retries.
- Offer reconnect for connection failures and system settings only when service approval is required. Never replay file operations after transport failures.
- Localize onboarding in all eight application languages. Keep the independent format-1 data directory without migrating or deleting older development data.

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

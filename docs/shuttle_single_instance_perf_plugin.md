---
status: completed
created_at: "2026-08-27"
updated_at: "2026-09-14"
truth_role: shuttle_single_instance_perf_plugin_design
document_role: shuttle_single_instance_latency_optimization
implements: docs/architecture.md §7.5 (未兑现的增量缓存) + §3 阶段③ 延迟压缩
source_revision: d68ab54
implementation_status: implemented_and_benchmarked
implementation_revisions: [36b081ca, 12d96651, f3f7fc2e, 90753d29, 4843b733, bdd24d49]
platform_scope: platform-agnostic (进程编排 + 符号缓存,无 OS 特定假设)
---

# 单个 shuttle 实例运行效率优化方案(插件优先)

> **As-built 状态（2026-09-14）**：本文规划的 ① run-scoped/磁盘符号缓存、② 单进程
> `context-pack`、③ `apply --gate`、④批量 locate、⑤ `--scan-scope` 及端到端延迟基准
> 均已实现。实现落点为 `extensions/shuttle-tools.ts`、`crates/shuttle-cli/src/{main,contextpack}.rs`、
> `crates/shuttle-symbols/src/lib.rs` 与 `benches/e2e_latency.py`。下文“未落地”“今天可上”等
> 表述保留为开工前设计基线，不再代表当前状态。跨任务 warm 进程复用仍是明确非目标。

> 目标:在**不新增第二个 LLM、不做跨任务 warm 进程复用**的前提下,压低单个
> `shuttle run` 实例的尾延迟与 CPU 浪费。优先走 pi 扩展(`shuttle-tools.ts`)层实施,
> 需要 core 子命令的单列出来。
>
> 边界:符合接受决策 §2.4「暂不做跨任务复用 shuttle/pi 进程」——本方案只做
> **run 内 / 仓库内**的缓存与进程合并,不触碰 warm engine reuse。

## 1. 现状浪费点(已由代码复核)

单实例的开销大头不在模型,而在"每个 shuttle 工具调用 = 一次 `shuttle` 二进制冷启动 +
一次全库 ctags 重扫",且 `architecture.md §7.5` 承诺的增量符号缓存至今未落地
(全仓无 `symbols.cache` 读写代码)。

| # | 浪费点 | 证据 | 代价 |
|---|---|---|---|
| W-A | repomap/rename 每次全库重扫 | `main.rs:407 run_repomap` / `:414 run_rename` 均 `SymbolTable::scan(&root)`,无缓存 | 每次 ctags 全扫,仓库越大越贵 |
| W-B | context_pack 单次调用最多 10 次 spawn | `shuttle-tools.ts` `spawnSync` 计数 = 10;每 ident 一次 locate + repomap + git diff | N-1 次进程冷启动 + 重复 ctags |
| W-C | apply 恒定两趟 spawn | `shuttle-tools.ts` apply_edit_plan:probe(dry-run)+ real,各起一次 `shuttle apply` | 第二趟重解析文件 + 重跑模糊阶梯 |
| W-D | locate 逐 ident spawn | `structuralLocate` 对候选 identifier 逐个 spawn `shuttle locate` 直到命中 | 命中前多次进程冷启动 |
| W-E | scan 恒为全库 | `SymbolTable::scan` 不接受范围参数 | focus 场景也全扫无关文件 |

## 2. 优化项(按杠杆排序)

### ① run 内符号表 / repo-map 缓存 —— 最高杠杆

- **纯插件版(先上)**:扩展侧加 run-scoped 缓存,key = `(root, git HEAD oid +
  dirty 文件集 hash)`,把 `shuttle_repomap` / `shuttle_context_pack` / `shuttle_rename` 结果落
  `.shuttle/cache/<runid>.json`,同 run 内命中直接读,跳过 shuttle 子进程与全扫。
- **彻底版(core)**:`shuttle repomap --cache` 落 `.shuttle/symbols.cache`,按 mtime+size
  判失效,只重扫 EditPlan 改过的文件 —— 即补上 §7.5 未兑现的增量缓存。
- 消除:W-A、W-B/W-C 的重复扫描部分。

### ② `shuttle context-pack` 合并子命令 —— 10 次 spawn → 1 次

把 context_pack 的编排从 TS 下沉成 Rust 子命令,同一进程内复用一份
`SymbolTable` 跑完「解依赖 → 取 handoff 文件 → repomap --focus → locate」。
扩展该工具从 10 次 spawn 降到 1 次。消除:W-B。

### ③ `shuttle apply --gate`:probe 与 real 合成一趟

给 `shuttle apply` 加 gate 模式:一个进程内先对所有 op 内存内跑 dry-run 判定,全绿才
落盘,有 miss 直接吐结构化诊断。两趟 spawn 合一趟,省一次进程 + 一次文件重解析。
安全语义不变(仍"全绿才写"),天然 all-or-nothing,与 P0-2 `Rejected`
fail-closed 对齐。消除:W-C。

### ④ locate 批量 ident —— 纯插件可做

`shuttle locate` 支持 `--pattern ident1,ident2,...`(或扩展侧一次把候选拼给一个
locate 进程),一个进程内按 salience 命中即返回,省命中前的多次冷启动。
消除:W-D。

### ⑤ 限定扫描范围 —— 大仓库收益

repomap/context_pack 只关心 focus 邻域,加 `--scan-scope`(git-tracked ∩ 与
focus 有引用边的文件),把 ctags 范围压到相关子集。消除:W-E。

## 3. 插件 vs core 归属

| 项 | 归属 | 依赖 core 改动 | 备注 |
|---|---|---|---|
| ① 缓存(TS 版) | 纯插件 | 否 | 今天可上 |
| ④ locate 批量 | 纯插件 | 否 | 今天可上 |
| ① 缓存(彻底版) | core + 插件 | `shuttle repomap --cache` | 兑现 §7.5 |
| ② context-pack 合并 | core + 插件 | 新子命令 | 杠杆最大 |
| ③ apply --gate | core + 插件 | apply 加 gate 模式 | 与 P0-2 对齐 |
| ⑤ scan-scope | core | scan 加范围参数 | 大仓库收益 |

## 4. 落地顺序

**第一批 —— 零 core 依赖、今天可上(纯 `shuttle-tools.ts`):**
1. ① 缓存 TS 版
2. ④ locate 批量 ident
> 顺带解掉接受决策 §2.4 P1-1:把 `shuttle-tools.ts` 从仓库外 `~/.pi/.../shuttle-tools.ts`
> 纳入仓库并锁摘要,改缓存/批量的同时修掉复现性问题。

**第二批 —— 配小 core 子命令(排在 P0-2 / P1-3 之后):**
3. ③ apply --gate(与 P0-2 Rejected 一起做,语义天然对齐)
4. ② context-pack 合并子命令
5. ① 缓存彻底版(`--cache` + 增量刷新)
6. ⑤ scan-scope

**第三批 —— 量化:**
7. 补 `benches/` 端到端延迟基准,对比每项前后的 wall_ms / spawn 次数 / ctags 扫描字节数,用数据决定 ⑤ 是否值得。

## 5. 边界与非目标

- **不做**跨任务复用 shuttle/pi 进程(warm engine reuse)——守 §2.4。
- **不塞**第二个 LLM;所有优化都是确定性进程编排 + 缓存,规划仍是 pi 里那一个模型。
- 缓存只在 run 内 / 仓库内 `.shuttle/cache`,失效判据保守(oid + dirty hash / mtime+size),
  宁可 miss 重算,不吃脏缓存。
- 安全语义不因合并 spawn 而降级:③ 的 gate 仍是"全绿才写",与 P0-2 fail-closed 一致。

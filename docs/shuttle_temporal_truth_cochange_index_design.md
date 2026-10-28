# G7 · 时间真相层 — 共变耦合弱边 + 增量持久索引（设计 v0.1）

> 状态：**implemented（G7，`f49394a0`）**。co-change 弱提示、内容寻址持久索引、
> 陈旧水位与 `cochange_cli.rs` / `index_cli.rs` 的 hermetic E2E 已落地。这里实现的是
> SHUTTLE 原生 on-disk index，不是  设想的外部 `.scip`
> 索引工位。下文保留为设计与验收基线。承接 
> 轴三、`shuttle-symbols::graph` 的 `EdgeKind`/`GraphConfidence`、T2 的陈旧水位校验
> （`ReverifyDetail.degraded` / `index_base_oid`）、
> 的 land 站增量边界。
> 红线：**§7「绝不瞎猜」——co-change 是天然弱信号：永远 Low、永远无向、永远
> 不参与 add/conflict/drop，只进 review surface 做提示（与 name-match 边同一
> 待遇）；历史不足即不产边（`insufficient-history`），绝不外推。**
> **daemonless：持久索引是可共享的 on-disk cache，绝不是常驻服务。**

---

## 0. 一句话

现在的图是**无时间的单快照**。Git 历史里藏着一层**概率性但诚实**的信号：
历史上**总一起改**的文件之间有一条「经验耦合」边，它**补齐**静态图看不见的
隐式依赖（两个没有 import 关系但语义绑定的文件）。G7 把这层信号引入，且严格
按弱边处置——只提示、不下结论。

---

## 1. 两个子能力

| 子项 | 名称 | 一句话 | 依赖 |
|---|---|---|---|
| **U1** | 共变耦合弱边 | `git log --numstat` 派生 `CoChange(Low, undirected)` 弱边，进 review surface | SymbolGraph |
| **U2** | 增量持久索引 | 内容寻址磁盘 cache，commit 间只重建 delta（B7 大仓延迟根治） | T2 stale 机制 |

**落地顺序：U1 → U2。** U1 是纯增益弱信号、不阻塞任何主线，先做。

---

## 2. U1 · 共变耦合弱边

### 2.1 新 `EdgeKind::CoChange`

```rust
pub enum EdgeKind {
    ImportDirected,   // High，参与裁决
    ContractDirected, // High，参与裁决（G6）
    NameMatch,        // Low，无向，贡献于零
    CoChange,         // 新增：Low，无向，贡献于零 —— 只进 review surface
}
```

**硬约束（与 name-match 边同源，写死在裁决层）**：
- `CoChange` 边**永远 `GraphConfidence::Low`、永远无向**。
- split `correct_split` 的 section 2 已有 `if kind != EdgeKind::ImportDirected {
  continue; }`——`CoChange` 天然被这道闸挡在 add/conflict/drop 之外，**无需改判据**。
- 它只在 `shuttle impact` 的**提示层**出现：改了 A，co-change 提示「历史上 B 常与 A
  同改，建议人看一眼」，但**绝不**把 B 加进机械重验集、绝不成边成环。

### 2.2 派生规则（`shuttle-symbols`，`git log --numstat` 一次性纯函数）

```
co_change_weight(A, B) = Σ_over_commits  decay(commit_size) · 共现(A,B)
```
- `decay(commit_size)`：提交越大权重越低（格式化大提交 / rename 提交产生的假相关
  被压制，承接 §2 坑 #7）。
- 只有权重超阈值才产 `CoChange` 弱边；阈值以下不产（不制造噪声）。

### 2.3 §7 降级

| 情形 | 处置 |
|---|---|
| 历史充足、权重超阈 | `CoChange`(Low, 无向)，只进 review 提示 |
| 格式化 / rename 大提交 | decay 压权，多半降到阈下不产边 |
| shallow clone / squash（历史断层） | 不产 co-change 边，标 `insufficient-history`，不外推（§2 坑 #8） |
| 与 ImportDirected 冲突 | Import 赢，co-change 仅作旁注，绝不改方向 |

---

## 3. U2 · 增量持久索引（B7 大仓延迟根治）

现状：B7（>100 文件）靠 G4 的延迟守护证明「有界时间内可裁决」，但每次冷算全图。
大仓（10k 文件 / 多仓）尺度需要**跨 commit 复用**。

- **内容寻址磁盘 cache**：符号定义 / 引用边按文件内容 hash 存盘；commit 间只对
  **变更文件**重建，未变文件命中 cache。
- **守 daemonless**：这是 **on-disk cache 文件**（可被多次一次性调用共享读），
  **不是常驻进程、不监听端口**——冷启动仍 = 0，只是读盘代替重算。
- **陈旧水位**：复用 T2 的 `index_base_oid` / `degraded` 机制——cache 与 HEAD
  漂移超过安全窗即失效重建，**绝不基于陈旧 cache 出「影响面」**（§2 坑 #9）。

---

## 4. E2E 验收（hermetic，仿 `goal_g4_boundaries.rs`）

`crates/shuttle-cli/tests/cochange_cli.rs` + `index_cli.rs`：
1. **co-change 只提示不成边**：两个无 import 关系但历史同改的文件 →
   `impact` 提示层出现 B，但**不**在机械重验集里、split 里**不**产 add 边。
2. **大提交衰减**：一个 100 文件的格式化提交不制造 100² 条 co-change 边（decay 生效）。
3. **历史断层**：shallow clone → insufficient-history，不产边、不崩。
4. **索引命中与失效**：未变文件命中 cache（第二次调用更快）；HEAD 漂移超窗 →
   cache 失效重建，结果与冷算一致（内容寻址正确性）。
5. **陈旧不出结论**：cache 陈旧且未重建 → `stale-index`，绝不出影响面。

---

## 5. 落地顺序（一口气做到底）

1. `EdgeKind::CoChange` + 裁决层确认被 ImportDirected 闸挡住（`shuttle-symbols::graph`，单测）。
2. `git log --numstat` 派生 + decay 权重（`shuttle-symbols`，一次性纯函数，单测）。
3. `shuttle impact` 提示层接 co-change（不进重验集，`shuttle-cli`）。
4. 内容寻址磁盘 cache + 陈旧水位（复用 T2，`shuttle-symbols`）。
5. hermetic 集成测试 `cochange_cli.rs` / `index_cli.rs` + 全量门禁 + commit（不 push）。

---

## 6. 不做

- 不做 co-change 的因果推断（只报相关、且永远弱、只提示）。
- 不让 co-change 参与任何自动裁决（add/conflict/drop/并发）。
- 不引入常驻索引服务 / 不监听端口（守 daemonless）。
- 不基于陈旧 cache 出结论（陈旧即失效重建）。

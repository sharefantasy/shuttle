# G6 · 语义真相层 — 跨语言契约图 + 类型感知影响面（设计 v0.1）

> 状态：**implemented（G6，`8606f6d4`、`43a4264b`、`9950fd74`、`df637274`）**。
> 跨语言契约图、OpenAPI/GraphQL 方言、`impact --typed` 和 `shuttle dataflow` 已落地，
> 对应 `contract_cli.rs`、`impact_cli.rs`、`dataflow_cli.rs` 覆盖 E2E 与诚实降级。
> 下文保留为设计与验收基线。承接 
> 轴一、T3 有向边（`shuttle-symbols::graph` 的 `EdgeKind::{ImportDirected, NameMatch}`
> / `GraphConfidence::{High, Medium, Low}`）、`shuttle-symbols::imports` 的
> `resolve_{ts,py,go,rust}` 多语言解析、`shuttle-refactor::type_closure` 类型闭包。
> 红线：**§7「绝不瞎猜」——契约对不上 codegen 痕迹 = 保持 name-match 弱边、
> 不跨语言下结论；类型推不出 = 退回 import 有向边(Medium)、不假装懂类型；
> 多方言对不上 = 降级、绝不跨方言瞎认。** 这一层直接根治 A4「跨语言口径不一」。

---

## 0. 一句话

现在的图停在**机械层**（谁 import 谁、谁引用谁的名字）。G6 往上爬一格到
**契约 / 类型层**：把 IDL（proto / thrift / openapi / graphql）当第一等真相源，
让 `Go server ↔ TS client` 之间有一条**可证的有向边**——不是猜两个 `helper`
是不是同一个（A4 的病根），而是「IDL 声明了这个 RPC/message，双侧 codegen 都
落地了它」。

---

## 1. 三个子能力（按依赖排）

| 子项 | 名称 | 一句话 | 依赖 |
|---|---|---|---|
| **S1** | 跨语言契约图 | IDL → service/method/message 节点，双侧 codegen 挂同一契约节点，产出**可证跨语言有向边** | T3 有向边 |
| **S2** | 类型感知影响面 | `shuttle impact --typed`：reverse-closure 只保留**携带类型依赖**的边 | S1 + type_closure |
| **S3** | 数据流 / effect 弱图 | 只报**可证**的值来源路径（taint/provenance），推不动即 None | S2 |

**落地顺序：S1 → S2 → S3。** S1 根治 A4、想象空间最大，先做。

---

## 2. S1 · 跨语言契约图

### 2.1 新 `EdgeKind::ContractDirected`

在既有 `EdgeKind::{ImportDirected, NameMatch}` 旁加第三类：

```rust
pub enum EdgeKind {
    ImportDirected,   // 既有：import 可信有向
    NameMatch,        // 既有：名字巧合，无向弱边
    ContractDirected, // 新增：IDL 契约证明的跨语言有向依赖
}
```

- `ContractDirected` 与 `ImportDirected` **同等可信**（High），可参与
  add/conflict/drop 裁决（split/goal 外环无需改判据，只要多认一类可信有向边）。
- 但它**只在双侧 codegen 痕迹都能锚定契约节点时**才成立；任一侧对不上 → 降级
  为 `NameMatch`（弱、无向、贡献于零），绝不跨语言硬连。

### 2.2 `resolve_idl_*`（`shuttle-symbols::imports`，与 `resolve_go` 等同构）

- `resolve_idl_thrift` / `resolve_idl_proto` / `resolve_idl_openapi` /
  `resolve_idl_graphql`：把 IDL 文件解析成契约节点
  `Contract { kind, namespace, service, method|message, fields[] }`。
- codegen 锚定：在各语言侧找「实现/使用了该契约」的痕迹（Go 的 `*.pb.go` /
  thrift gen、TS 的 client stub、生成注释里的 source-of-truth 标记），把实现文件
  挂到契约节点。**只认可锚定的**——找不到 codegen 痕迹的契约节点悬空，不产边。
- 已有 thrift customLanguage（`shuttle-symbols` 已支持 thrift kinds）是现成起点。

### 2.3 `shuttle impact --contract`

```
shuttle impact --changed api/user.proto --contract
```
回答「改这个 message 的这个字段，波及哪些语言的哪些实现文件」——沿
`ContractDirected` 边做 reverse-closure，跨语言但**每条边都可证**。

### 2.4 §7 降级（A4 的正解）

| 情形 | 处置 |
|---|---|
| IDL 双侧 codegen 痕迹都锚定 | `ContractDirected`(High)，参与裁决 |
| schema 改了但 codegen 没重生成（漂移） | 弱边 + 标 `stale-contract`，不判 High |
| 只对上名字、对不上 codegen（Go `helper()` vs Python `def helper`） | 退回 `NameMatch`，**绝不产跨语言 ADD**（正是 A4 断言） |
| 未知 IDL 方言 | 不解析、不产契约节点，标 `unknown-idl-dialect` |

---

## 3. S2 · 类型感知影响面 `shuttle impact --typed`

现状：reverse-closure 把所有 import 者都算进影响面。真相是——改一个**函数签名**，
真正受影响的是**类型上依赖它**的调用点，不是所有 import 者。

- 在既有 reverse-closure 之上加一层判定：这条边是否**携带类型依赖**（调用点用到
  了被改符号的签名/类型），借 `shuttle-refactor::type_closure` 的跨文件类型 BFS 或
  `shuttle-lsp-bridge` 的按需类型解析。
- `shuttle impact --typed` 输出比 `--directed` 更窄的、类型可证的影响面。

**§7 降级**：类型解析器不可用 / 弱类型语言（Python/JS）推不出 → **退回
`--directed` 的 import 有向边**，标 Medium，**永不假装懂类型**（承接 §2 坑 #6）。

---

## 4. S3 · 数据流 / effect 弱图

- **北极星**：回答「这个值的来源被这次改动动过吗」——轻量 taint/provenance，
  给「安全 / 正确性」类意图用。
- **第一锚点**：在 SymbolGraph 上叠一层**只沿可证赋值/传参链**的前向数据流，
  产 `DataFlow` 边（默认弱，除非语言侧能给强类型保证）。
- **§7 降级**：只报**可证的**数据流路径，推不动的路径直接 **None**，
  **绝不做启发式污点扩散**。

---

## 5. E2E 验收（hermetic，仿 `goal_g4_boundaries.rs`）

`crates/shuttle-cli/tests/contract_cli.rs`：
1. **契约可证跨语言边**：`user.proto` 定义 `GetUser`，Go server + TS client 各有
   codegen 痕迹 → `impact --contract` 命中双侧实现，边为 ContractDirected。
2. **A4 复现根治**：Go `helper()` 与 Python `def helper` 同名无契约 →
   `impact --contract` **不**产跨语言边（沿用 G4 的 A4 断言，此处升级为契约级）。
3. **契约漂移**：schema 改但无 codegen 痕迹 → 弱边 + stale-contract，不判 High。
4. **类型收敛**：`impact --typed` 比 `--directed` 窄，且类型推不出时退回 directed。
5. **未知方言**：喂一个不认识的 IDL → unknown-idl-dialect，不产节点、不崩。

每条 §7 降级路径都有专门断言。

---

## 6. 落地顺序（一口气做到底）

1. `EdgeKind::ContractDirected` + 图裁决多认一类可信有向边（`shuttle-symbols::graph`，单测）。
2. `Contract` 节点类型 + `resolve_idl_{thrift,proto}`（先两方言，`shuttle-symbols::imports`）。
3. codegen 锚定 + `shuttle impact --contract`（`shuttle-cli`）。
4. `resolve_idl_{openapi,graphql}`（补方言）。
5. `shuttle impact --typed`（S2，借 type_closure）。
6. `DataFlow` 弱边 + `shuttle dataflow`（S3）。
7. hermetic 集成测试 `contract_cli.rs` + 全量门禁 + commit（不 push）。

---

## 7. 不做

- 不做跨方言 IDL 的启发式对齐（对不上就降级）。
- 不做全语言类型推断（推不出退回 import 边）。
- 不做深度污点分析 / 符号执行（只沿可证链，推不动 None）。
- 不依赖外部 IDL 编译器常驻服务；解析是一次性纯函数。

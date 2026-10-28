# shuttle 包裹 pi：把 shuttle 提升为 pi 的深度优化前置控制器（v0.1）

> 状态：架构设计 + W1–W7 已落地。
> 关联：本文是对 `architecture.md §9「与宿主 agent 集成」` 的方向反转与升级；
> 冲突插件适配性分析见 §8，对象为 。
> 前提：pi 已本地安装（`~/.npm-global/bin/pi`，v0.84.2），并接到一个
> OpenAI-compatible provider。Key 通过 macOS Keychain 或 Linux 环境变量解析，
> 绝不落明文。

---

## 0. 一句话定位

**反转宿主关系**：不再是「pi 为宿主、shuttle 藏在 `apply_edit_plan` 扩展背后被 spawn」，
而是「**shuttle 为宿主/supervisor，pi 降级为 shuttle 内部一个可替换的 LLM 规划循环**」。
对外只暴露一个入口 `shuttle`，把 pi 当成它的「深度优化后端」。

- 旧拓扑：`用户 → pi（host + agent loop）→ 扩展 apply_edit_plan → spawn shuttle apply`
- 新拓扑：`用户 → shuttle（host + supervisor）→ spawn pi（纯规划）→ pi 调 shuttle 注入的确定性工具`

一句话：**shuttle 从「pi 插件背后的执行器」变成「套在 pi 外面的前置控制器」，pi 成为 shuttle 的可替换大脑后端。**

前提约束（守死）：**shuttle 内部绝不再塞第二个 LLM**。规划永远是 pi 里那一个模型，
守住公平 A/B 的前提；shuttle 只做「确定性工具编排 + 进程编排 + 遥测」。

---

## 1. 为什么反转（价值论证）

| 维度 | 旧（pi 为宿主） | 新（shuttle 为宿主） |
|---|---|---|
| 优化注入点 | 只有 apply 一环能被 shuttle 接管 | 入口 / 配置 / 工具集 / system prompt / 遥测全归 shuttle |
| 落盘路径 | pi 内建 edit/write 与 shuttle apply 并存，易分叉 | 统一走 shuttle，pi 的 edit/write 被禁用 |
| 代码检索 | pi 自带，token 贵、易 diff 对不齐 | 强制走 shuttle 的 repo-map / 符号表 Rust 快路径 |
| 遥测 | 各处解析 pi 输出（脆） | shuttle 一处产出 `{wall_ms, tokens, tool_calls, accept_rate}` |
| eval B 臂 | 手写 `arm_pi_shuttle.sh` 解析 pi | 坍缩成一行 `shuttle run` |
| 边界纪律（禁 land/禁危险工具） | 靠约定 | shuttle 作 supervisor 物理不给危险工具，强制执行 |

核心：**只有把入口收到 shuttle，才能把「所有确定性动作走 shuttle 快路径、模型只在残余模糊处出动」这条哲学从单点(apply)扩到全流程。**

---

## 2. 包裹的三种形态（选 B 为默认）

pi 的 CLI 已给足钩子，**无需改 pi 源码**：`--mode text|json|rpc`、`-e/--extension` 注入扩展、
`--no-builtin-tools / --exclude-tools / --tools` 替换工具集、`--append-system-prompt` 注入上下文、
`--no-session` 控会话、`--offline` 关启动网络。

**形态 A — exec 透传（最轻）**
`shuttle` 解析/改写参数后 `exec pi ...`，进程被替换，零常驻开销。适合交互式 TUI。
缺点：exec 后交出控制权，拿不到结构化遥测。用于 `shuttle` 无参进入交互式。

**形态 B — 子进程 supervisor（推荐默认）**
`shuttle` fork `pi -p --mode json -e <shuttle 扩展>`，全程持有子进程，解析 pi 的 JSON 事件流
（token / 工具调用 / 延迟），做软超时与降级。松耦合、pi 升级不易崩，复用现有
`parse_pi_tokens.py` 思路。**「深度优化版」最合适的骨架。**

**形态 C — RPC 深驱动（最强控制）**
走 `pi --mode rpc`，shuttle 用 JSON-RPC 逐轮驱动 pi，可在中途插入 repo-map、改写工具结果、
多候选重试。控制力最强但强耦合 pi rpc schema。作 `shuttle run --deep` 的可选模式。

**决策**：默认 **B**；C 作 `--deep` 深驱动；A 仅用于无参交互式。

---

## 3. shuttle 作为「pi 深度优化版」优化什么（非套壳）

1. **工具替换（核心）**：启动 pi 时 `--exclude-tools edit,write`（或 `--no-builtin-tools`），
   `-e` 注入 shuttle 扩展，暴露：
   - `apply_edit_plan` → shuttle 4 级模糊 apply 阶梯（已有）
   - `shuttle_locate / shuttle_symbols / shuttle_repomap / shuttle_codemod` → Rust 快路径
   效果：**所有落盘走 shuttle，所有检索走 shuttle 的 repo-map/符号表**，模型少烧 token、少犯 diff 对不齐的错。
2. **配置统一**：一份 `shuttle.toml`（model/provider/thinking/工具白名单/Key 解析）渲染成 pi flags。
   Key：Mac→Keychain，Linux→env，绝不落明文/日志/产物。
3. **system prompt 增强**：`--append-system-prompt` 注入 repo-map 摘要 +
   「编辑一律用 content-anchored search/replace 交 apply_edit_plan」硬约束。
4. **遥测内建**：每次 run 产出 `{wall_ms, tokens, tool_calls, accept_rate}`，正好是 eval B 臂所需。

---

## 4. 命令行形态

```
shuttle run "需求…"              # 默认 supervisor（形态 B），一次性
shuttle -p "需求…"               # 非交互，等价 pi -p 但走 shuttle 优化栈
shuttle run --deep "需求…"       # RPC 深驱动（形态 C）
shuttle                          # 无参 → exec pi 交互式 TUI（形态 A），注入 shuttle 扩展
shuttle -- <raw pi args>         # 透传逃生舱：未知 flag 原样转发，不丢 pi 任何能力
shuttle apply --root <dir>       # 保留：底层执行器仍可独立调用（向后兼容旧拓扑）
```

「透传逃生舱」是「深度优化版」定位的硬要求——用 shuttle **绝不能**让用户失去任何 pi 原生能力。

---

## 5. 配置与密钥

- `shuttle.toml`（项目级，可被 CLI flag 覆盖）：
  ```toml
  [model]
  provider = "model-provider"
  id       = "opensource/deepseek_v4_flash_0731"
  thinking = "off"
  [tools]
  exclude  = ["edit", "write"]          # 交给 shuttle
  inject   = ["apply_edit_plan", "shuttle_locate", "shuttle_symbols", "shuttle_repomap"]
  [key]
  macos   = "keychain:SHUTTLE_KEYCHAIN_SERVICE"
  linux   = "env:SHUTTLE_PROVIDER_KEY"
  ```
- Key 解析：Mac 通过 `SHUTTLE_KEYCHAIN_SERVICE` 指向的 Keychain service；
  Linux 通过 `SHUTTLE_PROVIDER_KEY`。**只读取注入子进程环境，绝不写文件/日志/report**；
  若需落 shell 文件权限 600。

---

## 6. 对 eval harness 的影响（顺带收益）

- B 臂：`arm_pi_shuttle.sh` 从「手写 pi 调用 + 三段解析」**坍缩成一行 `shuttle run`**。
- 公平性不变：A 臂 = native opencode；B 臂 = `shuttle run`（内部 pi + 同一 deepseek-flash、temp 0）。
- 遥测直接从 shuttle 拿，不再各处解析 pi 输出。
- 需同步改 `arm_pi_shuttle.sh` 与 `SCAFFOLDING.md §5.2`（现描述为「pi 为 host」）。

---

## 7. 与旧拓扑的兼容

- `shuttle apply --root` 后端入口**保留**，「pi 扩展 spawn shuttle」老路径**不删**；
  过渡期两种拓扑都能跑，逐步把默认入口切到 `shuttle run`。
- pi 版本锁定（当前 0.84.2），启动探测版本，不匹配→告警并降级到形态 A/透传。

---

## 8. 现有插件适配性分析：以 git 冲突插件为例

对象：。**结论先行：高度适配，且比编辑插件更契合新架构。**
原因是该插件本就把「确定性工具编排」与「模型兜底」显式分层，正好一一落到 shuttle / pi 两侧。

### 8.1 工具面重新归属（原 §3 → 新拓扑）

| 原插件工具（§3） | 性质 | 新架构归属 |
|---|---|---|
| `conflict.list` / `get_hunk` / `classify`（感知组，只读确定性） | 零模型 | **shuttle 原生**（`shuttle conflict list/get-hunk/classify`，Rust + tree-sitter） |
| `conflict.auto3way`（mergiraf 薄封装）/ `take --ours/theirs/union` | 零模型 | **shuttle 原生**（shuttle 直接 spawn mergiraf/git，本就是「编排确定性工具」的角色） |
| `conflict.verify`（分级门）/ `diffstat` | 零模型 | **shuttle 原生**（复用 shuttle 的 build-and-verify + difftastic/imara-diff 编排） |
| `conflict.stage`（仅 git add）/ `emit_receipt` | 零模型 | **shuttle 原生**（边界收敛，shuttle supervisor 掌控） |
| `conflict.propose --text <merged>`（L2 模型提交归并） | **需模型** | **pi loop 唯一暴露的冲突工具**（类比只暴露 `apply_edit_plan`） |

要点：**冲突插件 90% 的工具面是零模型确定性的**，天然下沉为 shuttle 的 Rust 后端；
只有 L2 的 `propose`（残余真语义冲突的模糊胶水）需要 pi 的模型。反转后，
「pi 加载冲突插件」变成「**shuttle 编排冲突工具链，仅在 L2/L3 spawn pi 做仲裁**」。

### 8.2 状态机对齐（原 §9 → 形态 B supervisor）

原文档的状态机本就是「L0/L1/rerere-Tier1 确定性走完，只有 L2/L3 才 [LLM]」。
这与形态 B 完美契合：**shuttle 主导整个状态机，只在 L2/L3 节点 spawn 一个短命 pi 做 propose，
拿回结果继续过 verify 门。** 控制权始终在 shuttle。

```
shuttle conflict resolve <run-id>          # shuttle 作 supervisor 驱动整个状态机
  ├ list/classify/route ────────────── shuttle 确定性
  ├ L0/L1/rerere-Tier1 ─────────────── shuttle 确定性（mergiraf/git/rerere）
  ├ L2 → spawn pi(propose) ─────────── 唯一模型介入点
  │        └ shuttle.verify 门 → 重试预算 → 升强模型 → 挂人
  ├ L3 → 打包 coupled → spawn pi 强模型 or 退 planner
  └ stage + emit_receipt ───────────── shuttle 确定性（禁 land）
```

### 8.3 边界纪律更好执行（原 §8「禁 land」）

新架构下 shuttle 是 supervisor，**pi 子进程物理上拿不到 land/commit/push 工具**
（`--exclude-tools` 只留 propose 类），比旧拓扑靠「写死 skill 约定」更硬。
「解冲突是任务类型不是角色」依旧成立，只是「worker」= 一个装载 conflict 工具集的 **shuttle 实例**
（按需带一个短命 pi），而非「装了插件的 pi worker」。
**[W7 已落地]** `shuttle-conflict` crate 已实现全工具面；`shuttle conflict *` 子命令族（Rust）处理所有确定性分组，仅 L2 语义 hunk 通过 `conflict_propose` 注入 pi 工具调用。

### 8.4 外部零模型工具编排更自然（原 §4/§5/§6/§10）

mergiraf / rerere（+base 复合指纹）/ difftastic / imara-diff / git 内置算法——
这些原来是 pi 插件 shell out 的对象，**新架构下直接是 shuttle 的子进程编排**，
与 shuttle「用 Rust 内核编排确定性工具」的定位完全同频；GPL-3.0 的 mergiraf 作独立进程调用
（不链接进 shuttle），传染问题依旧规避。§6 的 imara-diff 行级预筛闸门、§7 的 base 复合指纹、
§5 的 L0–L3 分级全部原样保留，只是宿主从 pi 换成 shuttle。

### 8.5 需要调整的点（落地清单）

1. ✅ 文档 §0/§8/§9 的「pi 插件 / warm worker 装载插件」表述 → 已更新为「shuttle conflict 子命令族 + L2 spawn pi」（W7）。
2. ✅ 工具命名：`conflict.*` → `shuttle conflict *`（子命令），仅 `propose` 保留为 pi 注入工具名 `conflict_propose`（JSON 定义见 ）。
3. receipt / OCC land 三段式（§8）不变：shuttle 产 receipt，B 线单点 land 照旧。
4. ✅ 新增 crate `shuttle-conflict` 已创建（`crates/shuttle-conflict/`），与 `shuttle-editcore`/`shuttle-symbols` 并列。

### 8.6 结论

冲突插件**能适配，且是新架构的样板案例**：它天生的「确定性主力 + 模型仅兜底残余」分层，
正是 shuttle/pi 反转后的理想分工。迁移成本主要是**表述与命名归属**，核心算法/工具选型/状态机/边界纪律**零改动**。

---

## 9. 风险与取舍

- **耦合 pi schema**：B/C 依赖 pi 的 json/rpc 输出格式。缓解：默认 B 的 JSON 事件解析（容错强），
  锁 pi 版本 + 启动探测，不匹配降级。
- **别长出第二个大脑**：shuttle 永远只有「执行内核 + supervisor」，规划唯一在 pi 的模型，守住公平 A/B。
- **交互式体验**：形态 A exec 替换，TUI 完全 pi 原生，零损耗。
- **双向兼容**：`shuttle apply` 后端保留，旧「pi 扩展 spawn shuttle」不删，过渡期双拓扑并存。

---

## 10. 落地步骤（里程碑）

- **W1**：`shuttle run` 子命令 + 参数层（model/thinking/tools/passthrough）→ 渲染 pi flags。
- **W2**：抽出 supervisor：spawn `pi -p --mode json -e <ext>`，解析事件流 + 软超时 + 遥测。
- **W3**：shuttle 扩展扩容：`apply_edit_plan` + `shuttle_locate/shuttle_symbols/shuttle_repomap`，启动 `--exclude-tools edit,write`。
- **W4**：`shuttle.toml` + Keychain/env Key 解析。
- **W5**：改 `arm_pi_shuttle.sh` → `shuttle run`，同步 `SCAFFOLDING.md §5.2` 架构图。
- **W6（可选）**：`--deep` 的 RPC 深驱动模式。
- **W7（冲突插件迁移）✅**：`shuttle-conflict` crate 已实现（12 unit tests pass）。`shuttle conflict *` 子命令族在 `crates/shuttle-cli/src/conflict.rs`。L2 `propose` 以 `conflict_propose` pi 注入工具暴露（）。算法零改动。

# shuttle — 架构设计（v0.1）

## 1. 目标与定位

构造一个**高效开发 agent 的执行内核**，专为**小而快的模型**优化，职责是**快速执行上游"聪明慢模型"给出的规划**。

- 上游（planner，慢/聪明）：理解意图、拆解任务、产出结构化 `EditPlan`。
- 下游（executor，快/小 = 本项目）：把 `EditPlan` 落到磁盘，追求**最低尾延迟 + 最少 token + 最少模型往返**。
- 以 **pi / aider 这类 agent 为中心**组织：本项目不重造 agent 循环，而是作为它们可挂载的**工具/执行器**（plugin / 外部 CLI）。

## 2. 为什么用 Rust（而非 Go）

| 决定性因素 | 说明 |
|---|---|
| 生态母语 | ast-grep、similar、imara-diff、tree-sitter 绑定全是 Rust crate，直接用，无 FFI |
| 尾延迟 | 无 GC，P99 稳定；小快模型场景每次 apply 都在热路径 |
| 零拷贝 diff | imara-diff 的 token interning 靠借用，省大文件分配 |

Go 的优势（并发编排、开发速度）落在 glue 层，而 glue 层交给 pi/aider，不由本项目承担。

## 3. 一次编辑的 5 阶段与加速点

| 阶段 | 组件 | 加速手段 |
|---|---|---|
| ① 定位 | `shuttle-locate` | ripgrep（文本）/ ast-grep（结构），精准 → 窄上下文 |
| ② 上下文 | planner 侧 | 只喂最小节点，靠 ①的精准定位裁剪 |
| ③ 生成 | planner（模型） | **最小化编辑表示法**：search/replace 块，非整文件、非行号 diff |
| ④ 落盘 | `shuttle-editcore` | 4 级模糊 apply 阶梯，抗漂移、少重试 |
| ⑤ 校验 | 宿主 agent | lint/format/编译，超出本内核 |

延迟大头是 ③；本内核通过"最小表示法 + 一次 apply 成功 + 结构化绕过"三招压缩它。

## 4. Crate 划分

```
crates/
  shuttle-protocol   EditPlan / EditOp / ApplyOutcome —— planner↔executor 线协议
  shuttle-locate     ripgrep / ast-grep 定位封装
  shuttle-symbols    universal-ctags + tree-sitter 符号表 / repo-map（取代 LSP，见 §7、§7.5）
  shuttle-editcore   search/replace 4 级模糊 apply（热路径核心）
  shuttle-codemod    ast-grep 规则驱动的零 token 机械改动
  shuttle-cli        `shuttle` 二进制：宿主 agent 调用入口
```

## 5. 编辑表示法：search/replace 块（非 unified diff）

unified diff 依赖行号，漂移即 apply 失败 → 模型重生成 → 整趟往返。改用**内容锚定块**：

```
    def process(self, data):
        validate(data)
        return data
```

由 `EditOp::SearchReplace` 承载。锚点向内收缩到"唯一命中的最小长度"以省 token。

## 6. 模糊 apply 阶梯（shuttle-editcore）

只有"总 miss"才昂贵（触发模型往返），故阶梯设计成几乎不 miss：

- Tier 0 精确子串
- Tier 1 空白规范化（去行尾、tab↔space），命中后按原文缩进回填
- Tier 2 相似度滑窗（similar 的 ratio ≥ 0.90）
- Tier 3 miss → `AnchorMiss`，回抛模型

`match_tier` 作为遥测回传，用于调阈值。

## 7. 结构化绕过模型（shuttle-codemod + shuttle-symbols）

改动能规则化就不进模型。兜底优先级：

```
符号表精确改（shuttle-symbols） > ast-grep 规则 > comby > tree-sitter query > 模型
```

**为什么不用 LSP rename**：LSP server 冷启动慢（索引整个工程、常几秒到几十秒），
与"小快模型 + 低尾延迟"的定位冲突——一次重命名不该先等一个语言服务器起来。
改用**无守护进程的静态符号表**：

- **一级：universal-ctags** —— `ctags --output-format=json -R`，毫秒级全库扫描，
  产出 `{name, kind, scope, file, line}` 标签流。覆盖 40+ 语言，无需起 server。
  重命名/查定义直接查这张表，命中即用 `shuttle-editcore` 落 search/replace，零模型。
- **二级：tree-sitter** —— ctags 拿不准作用域时（同名局部变量 vs 顶层符号、
  闭包捕获、方法 vs 自由函数），用 tree-sitter 解析该文件的语法树做**作用域消歧**，
  确认某个引用是否真指向目标符号，避免 ctags 纯文本匹配的误改。
- 机械改动（重命名、加参数、批量 API 改法）→ `rules/*.yml`，`sg scan --update-all`，零 token。
- 先 `dry_run` 预览再 apply，把错改挡在磁盘前。

> 组合心智：**ctags 负责"快而广"地找到所有候选，tree-sitter 负责"准"地裁掉误命中**。
> 二者都是进程内一次性调用，无常驻服务，冷启动 = 0。

## 7.5 repo-map：借鉴 aider 的仓库地图

aider 的核心洞察:planner（慢模型）最贵的不是生成 diff，而是**读懂"该改哪儿、周边长啥样"**。
aider 用 tree-sitter 抽取每个文件的**定义/引用标签**，构造符号引用图，用 **PageRank** 排出
"对当前任务最重要的符号"，再按 **token 预算**裁出一张紧凑的仓库地图喂给模型——
既给了全局结构感，又不撑爆上下文。本项目把这套搬进 `shuttle-symbols`:

| 步骤 | 做法 | 复用 |
|---|---|---|
| ① 抽标签 | ctags（快）+ tree-sitter（补作用域）产出 def/ref 标签 | 与 §7 同一份符号表，不重复解析 |
| ② 建图 | 符号为节点，"文件 A 引用了 B 中定义的符号"为边 | —— |
| ③ 排序 | **PageRank**:被引用越多/越被重要文件引用的符号，排名越高 | 可对"当前编辑涉及的文件"加权（personalized PageRank） |
| ④ 裁剪 | 按给定 token 预算，从高到低取符号 + 其定义签名行，拼成 map | 预算由 planner 侧传入 |

与 aider 的差异（贴合本项目定位）:

- **给 planner 用，不是给自己用**:`shuttle repomap --budget N` 输出结构化 map（JSON / 紧凑文本），
  由上游慢模型消费;executor 自身不需要它。
- **增量刷新**:只有被 `EditPlan` 改动过的文件重新抽标签，其余走缓存（`.shuttle/symbols.cache`），
  避免每次全库重算——这是相对 aider 每轮重建的关键提速点。
- **定位复用**:repo-map 的标签图与 §7 的重命名符号表是**同一份数据**，一次 ctags 扫描两用。

## 8. diff 库选型结论

| 库 | 定位 | 何时用 |
|---|---|---|
| similar | 通用 diff + 高亮，人体工学好 | 交互式单文件编辑、模糊定位（默认） |
| imara-diff | 极致性能，token interning | 大文件 / 高吞吐批量（`fuzzy-imara` feature 切换） |
| diffy | 生成/应用 unified patch | 需 patch 存档或类 `patch` 语义时 |

原生 fuzzy 都偏弱，抗漂移的模糊层由 `shuttle-editcore` 自建。

## 9. 与宿主 agent 集成

> **集成拓扑（v0.1）**：首选 **shuttle 作为 supervisor 包裹宿主 agent**（如 pi），
> agent 降级为 shuttle 内部一个可替换的 LLM 规划循环；「agent 扩展 spawn `shuttle apply`」
> 的插件拓扑作为兼容后端保留。

- **pi（主路径）**：入口收敛到 `shuttle run`。shuttle 以子进程 supervisor 形态 spawn
  `pi -p --mode json -e <shuttle 扩展>`，用 `--exclude-tools edit,write` 禁掉 pi 内建落盘，
  改由注入的 `apply_edit_plan` / `shuttle_locate` / `shuttle_symbols` / `shuttle_repomap` 走 shuttle 快路径。
  规划仍是 pi 里那一个模型（shuttle 内不塞第二个 LLM）。
- **aider**：其编辑格式本就是 search/replace，可直接用 `shuttle apply` 替换其 apply 步骤;
  `shuttle repomap` 也可替换 aider 自带的 repo-map（同思路、增量缓存更快）。

### 9.1 运行时诊断与内存边界

repo-map/context-pack 的“一次 ctags 扫描两用”不代表扫描可以无界。递归 root、ctags
完整 stdout buffering、完整 SymbolTable/引用图驻留会共同放大单进程 RSS；supervisor
形态下解析宿主 agent 的 JSON 事件流也必须提供有界背压。

## 10. 里程碑

- M0（骨架）：workspace + 协议 + CLI 桩 ✅（本次，已含 shuttle-symbols 桩）
- M1：shuttle-editcore 三级模糊 apply 落地 + 单测
- M2：shuttle-locate 的 rg/sg JSON 解析
- M3：shuttle-symbols —— ctags JSON 解析 + tree-sitter 作用域消歧 + 符号表重命名
- M4：shuttle-symbols repo-map —— 引用图 + PageRank + token 预算裁剪 + 增量缓存
- M5：shuttle-codemod dry-run/apply + 起步规则集
- M6：pi/aider 集成 adapter + 端到端延迟基准（benches/）

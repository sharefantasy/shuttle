# shuttle 重构引擎：tree-sitter locals 驱动的「无守护进程 code action」方案

> **状态：implemented（设计内三批均已进入开发并交付）**。`shuttle-refactor`、locals
> 求解、局部/跨文件重构、事务 batch、可选 ts-morph、LSP codeAction 投影及相应
> benchmark/测试已落地；主体提交从 `52f0e6a7`、`8766377c` 延伸至 2026-08-29
> 的 batch-3 与安全门提交。下文保留为原始设计和分批验收基线。

> 目标：给 shuttle 补齐 **JetBrains 式一键重构**（提取方法 / 内联 / 提取变量 / 改签名等）的能力，
> 但**不走 LSP `codeAction`、不常驻语言服务器、不做类型推断兜底**——用 tree-sitter 的
> `locals`（作用域 / 定义 / 引用查询）在**函数内作一次性语法+局部语义分析**，把「提取方法」这类
> 重构的**参数推导 / 返回值推导 / 作用域冲突判定**在毫秒级、零构建地算出来。
>
> 本文最初作为不含实现的方案文档编写。定位见 §1，落地顺序见 §9。相关既有设计见
> `shuttle_lsp_capability_plan.md`（能力阶梯与一等/二等/三等分档）、`architecture.md` §7（结构化绕过模型）。
>
> **一句话立场**：LSP 的 code action 慢在「先索引全工程、再靠类型系统算安全变换」；但**绝大多数
> 高频重构（提取方法 / 提取变量 / 内联）本质是「函数内作用域 + 数据流」问题，不需要跨文件类型推断**。
> tree-sitter locals 恰好在「不编译、不索引、纯解析单函数」的前提下给出作用域与名字绑定，
> 因此可以做到**比 LSP code action 快一个量级**，且天然契合 shuttle「一次 run、无守护进程」的定位。

## 1. 背景与定位：为什么在阶梯里新开这一层

`shuttle_lsp_capability_plan.md` 已确立无守护进程能力阶梯：

```
符号表精确改（shuttle-symbols） > ast-grep 规则 > comby > tree-sitter query > 模型
```

其中 `codeAction`（LSP #16）当前被映射到 **`shuttle-codemod` 规则集**，档位是**二等**，注明
「精确（规则内）」。但这里有一个**能力缺口**：

- `shuttle-codemod` 的 ast-grep 规则是 **pattern → pattern** 的**结构模板改写**：它擅长「把所有
  `foo(x)` 改成 `bar(x)`」这类**形状固定、与数据流无关**的机械改动。
- 但 **JetBrains 式重构（提取方法为代表）不是模板改写**：要正确「提取方法」，必须知道
  ①选区里用到了哪些**在选区外定义**的变量（→ 参数）；②选区里定义/修改、又在选区后被用到的
  变量（→ 返回值）；③抽出去后有没有**名字遮蔽 / 作用域冲突**。**这三件事 ast-grep 的
  pattern 语法表达不了**——它没有「作用域」和「名字绑定」概念。

所以现状是：**二等的 `codeAction` 条目，对「规则化 quick-fix」够用，但对「重构类 code action」是空的**。
本方案就是**填这个空**，且**不违背阶梯的无守护进程原则**：

| 阶梯层 | 覆盖的改写类型 | 本方案的位置 |
|---|---|---|
| `shuttle-symbols` 符号表精确改 | 跨文件改名（名字级） | 已有 |
| `shuttle-codemod`（ast-grep 规则） | 形状固定的模板改写（API 批改） | 已有，二等 codeAction 的「规则」部分 |
| **本方案：tree-sitter locals 重构引擎** | **函数内作用域+数据流相关的重构（提取方法/变量/内联）** | **新增，补齐二等 codeAction 的「重构」部分** |
| comby / 模型 | 兜底 | 已有 |

**核心结论先行**：shuttle 要的是 code action 的**能力**（一键重构），不是 LSP 的**实现方式**（索引+类型系统）。
高频重构的正确性只依赖**函数内的局部信息**，tree-sitter locals 正好只提供且快速提供这层信息。

## 2. 为什么 tree-sitter locals 比 LSP code action 快、且够用

### 2.1 tree-sitter locals 是什么

tree-sitter 除了语法树，还支持一组约定的 **query 文件**，本方案用到两类：

- **`locals.scm`**：用 `@local.scope` / `@local.definition.*` / `@local.reference` 三类捕获，
  声明某语言里「什么节点开一个作用域、什么节点是定义、什么是引用」。tree-sitter 有一个内置的
  **locals 求解规则**：一个 `@local.reference` 绑定到「最近的、包住它的作用域链上最近的同名
  `@local.definition`」。**这就是名字绑定（name binding），且完全不需要类型信息**。
- **`tags.scm`**：符号标签（供 repo-map / 大纲用），本方案不依赖它，此处仅提及边界。

关键：locals 求解是**纯结构规则**（作用域嵌套 + 就近同名匹配），**不涉及类型、不涉及跨文件、
不需要编译**。对「提取方法」需要的「这个名字在选区内是不是自由变量（引用了外部定义）」这类问题，
locals 恰好直接回答。

### 2.2 与 LSP code action 的对比

| 维度 | LSP `codeAction`（extract method 等） | shuttle tree-sitter locals 重构引擎 |
|---|---|---|
| 前置条件 | 语言服务器**索引全工程**（秒~几十秒冷启动） | **0**：只解析当前文件的当前函数 |
| 依赖 | 类型系统 / 语义分析（保证「安全变换」） | 仅语法树 + locals 作用域规则 |
| 常驻 | 语言服务器进程常驻 + 内存索引 | 无守护进程，一次调用用完即退 |
| 分析范围 | 可跨文件（也因此更慢、更重） | **函数内局部**（高频重构本就只需这个范围） |
| 单次延迟 | 索引后快，但首次+崩溃恢复贵 | 解析单函数 = 毫秒级 |
| 多语言 | 每语言一个 server | 一份 tree-sitter grammar / 语言，一个引擎 |
| 与 agent 生命周期 | 需管理 server 生命周期 | 无状态，契合「一次 run 一批操作」 |

**为什么「够用」**：JetBrains / LSP 在 extract method 上做的**类型级增强**（比如推导抽出方法的
**返回类型注解**、跨文件调整重载）在 shuttle 的场景里**要么不需要、要么可降级**：

- 动态语言（**python / javascript**）本就**不写返回类型注解**，locals 完全够。
- **typescript** 需要返回类型注解时，**按需**桥接 `ts-morph`（TS Compiler API，非 daemon，
  一次性调用）补类型；不需要注解时 locals 直接出。
- **go / rust** 需要显式返回类型：locals 能定出「要返回哪些变量」，**类型名**从 ctags 定义表
  或就近声明启发式取；取不到则**降级为占位类型 + 标记待确认**（§7 边界），绝不瞎猜。

### 2.3 与 ast-grep 的分工（不是替代，是补位）

- ast-grep（`shuttle-codemod`）：**改写执行层**——真正把新方法插进去、把选区替换成调用，仍走
  search/replace 落盘（`shuttle-editcore`）。它擅长「结构化地生成/替换代码片段」。
- tree-sitter locals（本方案）：**决策/推导层**——算出「参数列表、返回值列表、有没有冲突」
  这些 ast-grep 表达不了的**数据流事实**，再把结果交给改写层去执行。

一句话：**locals 决定「怎么改才对」，ast-grep/editcore 负责「把改动落下去」**。

> **前提订正（重要）**：shuttle 现状**没有 native tree-sitter 通道**——`disambiguate_refs` /
> `shuttle-codemod` / `shuttle-locate` 都是**外挂 `ast-grep`/`sg` 二进制**（ast-grep 内部虽嵌了
> tree-sitter grammar，但 shuttle 用的是它的 `kind` 匹配，拿不到 `locals.scm` 的
> scope/definition/reference 求解）。locals 求解**必须新增 native tree-sitter 依赖**
> （`tree-sitter` crate + 每语言 grammar crate + `locals.scm`）。本方案已确认这笔新依赖
> **成本可接受**，作为 `shuttle-refactor` 的地基一次性引入，不再宣称「复用现成通道」。详见 §6.1。

## 3. 核心算法：提取方法（extract method）的 locals 推导

提取方法是重构里**最能体现「作用域+数据流」本质**的操作，把它讲透，其余重构（提取变量、内联）
都是它的子集或逆操作。

**输入**：文件路径 + 选区（起止行/列，即要抽出的语句块）+ 新方法名。
**输出**：`RefactorPlan`——新方法的签名（参数、返回值）、新方法体、原选区替换成的调用语句，
以及每一项的 confidence。

### 3.1 建立三个变量集合

在**包住选区的最内层函数**上跑 locals，得到该函数内所有 `@local.definition` 和
`@local.reference` 及其作用域。对选区 `S`，定义：

1. **`def_before`**：在 `S` **之前或包含 S 的作用域里**定义、且**其定义点不在 S 内**的变量。
2. **`def_in`**：定义点**在 S 内**的变量（选区内新引入的局部变量）。
3. **`ref_in`**：在 `S` 内出现的 `@local.reference` 所绑定到的定义变量集合。
4. **`ref_after`**：在 `S` **之后**（同函数内）出现的引用所绑定到的定义变量集合。

### 3.2 推导参数（parameters）

> **参数 = 选区内被引用、但定义在选区外的变量**。

```
params = { v in ref_in | v.definition not in S }   # 即 ref_in ∩ def_before
```

对每个参数还要判断**是否被选区修改**（写引用），用于决定传值/传引用/是否需回传（§3.3、§5 各语言差异）。

### 3.3 推导返回值（return values）

> **返回值 = 在选区内定义或被修改、且在选区之后仍被使用的变量**。

```
returns = { v in (def_in ∪ modified_in_S) | v in ref_after }
```

- `returns` 为空 → 新方法无返回值（void / 无 return）。
- `returns` 恰好一个 → 单返回值。
- `returns` 多个 → 依语言：go 用多返回值；python 用 tuple；ts/js 用对象/数组解构；
  rust 用 tuple 或（更地道时）标记建议建结构体（§5）。

### 3.4 作用域冲突与合法性检查（拒绝比错改更重要）

在生成 plan 前，必须过以下 gate，任一不过 → **拒绝或降级**，绝不静默产出可能错误的重构：

| 检查项 | 用 locals/语法树怎么判 | 不过时的处理 |
|---|---|---|
| **名字遮蔽** | 新方法名 / 参数名是否与目标作用域已有 `@local.definition` 冲突 | 自动改名或回报冲突 |
| **控制流出口** | 选区内是否含 `return` / `break` / `continue` / `goto` / 标签跳转，导致「抽出后控制流语义变了」 | 复杂控制流 → 拒绝，回报原因 |
| **选区是否为完整语句块** | 选区边界是否切断了语法节点（半个表达式、跨越 if 的一半） | 边界非完整节点 → 拒绝 |
| **提前 return 依赖** | 选区内 `return` 是否需要传播到调用处 | 简单尾部 return 可处理；中段 return → 拒绝 |
| **别名/可变引用**（rust/python） | locals 无法判断的指针别名、可变对象别名 | 标记「提取 + 待人确认」，不宣称安全 |

**原则**：locals 能判定的（作用域/绑定/名字冲突）给 High confidence；locals 判不了的
（别名、复杂控制流、类型级正确性）**要么降级标注、要么直接拒绝**——shuttle 永远不假装 IDE 级正确。

### 3.5 端到端流程

```
shuttle refactor extract-method --file F --range L1:C1-L2:C2 --name newFn [--write] [--json]

  1. 解析 F → tree-sitter 语法树（shuttle-refactor 新增的 native tree-sitter 通道，见 §6.1）
  2. 定位包住 range 的最内层函数节点 fn_node
  3. 在 fn_node 上跑 locals.scm → 作用域+定义+引用图
  4. 按 §3.1~§3.3 算 params / returns
  5. §3.4 合法性 gate；不过 → 拒绝/降级并回报
  6. 按目标语言模板（§5）生成：新方法签名+体、原选区→调用语句
  7. 组装成 shuttle-protocol::EditPlan（若干 SearchReplace EditOp）
  8. 默认 dry-run 出 diff + confidence；--write 才经 shuttle apply --gate 落盘
```

关键：第 7 步产出的**仍是 shuttle 既有的 `EditPlan`/`EditOp`**，落盘走既有 `shuttle-editcore`
search/replace 4 级模糊 apply（`architecture.md` §6）。**本方案不新增落盘通道，只新增决策层**。

## 4. 其余重构操作（都是提取方法的同构/子集）

| 重构 | 与 extract-method 的关系 | locals 用法 | 档位 |
|---|---|---|---|
| **提取变量 extract-variable** | 提取方法的退化：无控制流、返回单值 | 选表达式 → 在最近作用域上方插 `let/= 赋值` → 替换为变量名；检查新名冲突 | 一等（简单、可靠） |
| **内联变量 inline-variable** | extract-variable 逆操作 | locals 找该变量所有引用 → 全部替换为其定义右值 → 删定义；若变量被重赋值则拒绝 | 一等 |
| **内联方法 inline-method** | extract-method 逆操作 | 找调用点 → 参数实参绑定形参 → 展开方法体；有多返回/副作用则降级 | 二等 |
| **重命名局部变量 rename-local** | 纯 locals 绑定 | locals 求该变量的定义+全部引用（函数内），批量改；与跨文件 `shuttle rename` 互补（那个走符号表） | 一等 |
| **改方法签名 change-signature**（加/删/重排参数） | 参数集变换 + 调用点同步 | locals 定形参；调用点用 ast-grep/`shuttle callers` 找 | 二等（跨文件调用点用启发式，标 confidence） |
| **提取常量 / 字段** | extract-variable 变体（作用域上移到类/模块） | 同 extract-variable，插入点上移；检查模块级名字冲突 | 二等 |

> `rename-local` 与 `shuttle_lsp_capability_plan.md` §9.5 的 `shuttle rename` 分工：
> **函数内局部变量**用本引擎的 locals（更精确、零候选噪声）；**跨文件符号级改名**走符号表
> （ctags + `disambiguate_refs`）。二者按「符号是否逃逸出函数」自动分派。

## 5. 五语言适配矩阵（go / typescript / javascript / python / rust）

本方案**只覆盖 5 种目标语言**（与项目既定范围一致）。各语言在提取方法上的差异集中在
**返回值形态**和**类型注解**两点：

| 语言 | locals 成熟度 | 多返回值形态 | 返回类型注解 | 参数「写回」处理 | 备注 |
|---|---|---|---|---|---|
| **go** | ✅ 成熟 | 原生多返回值 `(a, b)` | **必须显式**：类型从 ctags/就近声明取，取不到→占位+待确认 | 值语义，多返回值回传即可 | `err` 惯例：若选区产 error，建议放最后一个返回 |
| **typescript** | ✅ 成熟 | 对象 `{a, b}` 或元组 `[a, b]` | 可选；需要时**按需 `ts-morph`** 补推导，否则省略/`any`（标注） | 引用类型天然共享，基本类型需回传 | 唯一需要类型级增强的语言，走非 daemon 的 ts-morph |
| **javascript** | ✅ 成熟 | 对象 / 数组解构 | 无（JS 不写类型） | 同 TS | locals 完全够，无需类型工具 |
| **python** | ✅ 成熟 | tuple `return a, b` | 可选 type hint；默认不加（或加占位，标注） | 可变对象别名风险 → §7 降级 | `nonlocal`/`global`/闭包捕获用 locals 的 scope 规则判 |
| **rust** | ✅ 成熟 | tuple `(a, b)`，多值建议提示建 struct | **必须**：类型难从 locals 取，多数需就近声明/ctags，取不到→占位+待确认 | **借用/可变引用/所有权 locals 判不了** → 默认降级标注，不宣称安全 | rust 是边界最紧的语言，见 §7 |

**分派规则**：
- **js / python**（无类型注解或可选）：locals 直接出，High confidence。
- **ts**：不需要返回类型注解时 locals 直接出；需要时 `ts-morph` 一次性补类型（可选 feature）。
- **go / rust**（强制返回类型）：locals 定「返回哪些变量」，类型走「就近声明 → ctags 定义表」
  两级启发式；命中 High，未命中 → **占位类型 + `// TODO: 确认类型` + Medium/Low confidence**。

## 6. 架构落点：新增 `shuttle-refactor` crate，复用既有一切

对齐 `shuttle_lsp_capability_plan.md` §9.11 的 crate 落点风格，本方案**只新增一个 crate**，其余全是
既有原语组合：

```
                     +-------------------------------------------+
   planner(慢模型) ->|  shuttle-cli：shuttle refactor <op> 子命令           |
                     +-------------------------------------------+
   L1 能力面          | extract-method / extract-variable /        |
                     | inline / rename-local / change-signature   |
                     +-------------------------------------------+
   L2 重构决策核心    | shuttle-refactor(新 crate)：locals 求解 +        |
                     |   params/returns 推导 + 合法性 gate         |
                     |   native tree-sitter 解析通道（新增，§6.1） |
                     +-------------------------------------------+
   L3 改写执行(复用)  | shuttle-protocol(EditPlan) -> shuttle-editcore        |
                     | (search/replace 落盘) / 可选 ts-morph 适配   |
                     +-------------------------------------------+
```

| crate | 改动 | 说明 |
|---|---|---|
| **`shuttle-refactor`（新增）** | locals 求解 + `params/returns` 推导 + 合法性 gate + 各语言模板 | 唯一新引擎位，纯函数式、可单测 |
| `shuttle-symbols` | 复用 `defs`（取返回类型的启发式）；**注意其消歧是外挂 ast-grep，非 native tree-sitter** | 不动核心扫描 |
| `shuttle-codemod` | 复用「结构化生成代码片段」的能力，作为改写执行 | 不改 |
| `shuttle-editcore` | 复用 `apply_search_replace` 落盘 | 不改 |
| `shuttle-protocol` | `EditPlan`/`EditOp`/`ApplyOutcome` 复用作改写出口 | 不改 |
| `shuttle-nav`（若已建） | `change-signature` 找调用点复用 `shuttle callers` | 复用 |
| `shuttle-refactor` 的 `ts-morph` 适配 | 可选 feature，仅 TS 需类型注解时按需拉起、用完即走 | 默认不编译 |

### 6.1 新增依赖：native tree-sitter 通道（本方案的唯一新地基）

与 `shuttle-nav`（复用 ctags+ast-grep 外挂进程）不同，`shuttle-refactor` 需要 `locals.scm` 的
**scope/definition/reference 求解**，这是 ast-grep 二进制**给不了**的（它只暴露 `kind` 匹配）。
因此 `shuttle-refactor` 是 shuttle 里**第一个引入 native tree-sitter 的 crate**：

| 依赖 | 用途 | 备注 |
|---|---|---|
| `tree-sitter`（rust crate） | 语法树解析 + `Query`/`QueryCursor` 跑 `locals.scm` | 进程内，无守护，用完即退 |
| `tree-sitter-python` / `tree-sitter-javascript` | 批 1 目标语言 grammar | 批 1 只引这两个 |
| `tree-sitter-go` / `tree-sitter-rust` / `tree-sitter-typescript` | 批 2 目标语言 grammar | 批 2 再引 |
| `locals.scm`（每语言一份，随 crate 内置） | 声明 `@local.scope`/`@local.definition.*`/`@local.reference` | 优先复用官方 grammar 仓库自带的 `queries/locals.scm`，缺失才自写结构子集 |

**成本评估（已确认可接受）**：
- **编译期**：grammar crate 自带 C 源，走 `cc` 编译，首次构建增量数分钟；无运行期守护。
- **二进制体积**：每语言 grammar 约数百 KB~1MB 静态链接进 `shuttle`，5 语言总量可控。
- **与既有阶梯的关系**：native tree-sitter **只服务 `shuttle-refactor` 的 locals 求解**，
  不替换 `shuttle-nav`/`shuttle-codemod` 的 ast-grep 通道（那套在 references/rename/codemod 上已够用）。
  两条通道并存：ast-grep 管「结构匹配/改写执行」，native tree-sitter 管「作用域/数据流决策」。
- **与 `customLanguages`（capability_plan §6.7）的协同**：批 3 若要给 thrift/bash 挂 grammar，
  应与本 crate 引入的 tree-sitter grammar 加载机制**统一规划**，避免两套 grammar 管理。

**统一数据模型**（放 `shuttle-refactor`，与 `shuttle-nav` 的 `NavHit` 风格一致）：

```rust
/// 一次重构的完整计划，self-contained 给 planner 审阅
pub struct RefactorPlan {
    pub op: RefactorKind,          // ExtractMethod | ExtractVariable | Inline | RenameLocal | ChangeSignature
    pub edits: Vec<EditOp>,        // 复用 shuttle-protocol，落盘走 shuttle-editcore
    pub signature: Option<String>, // 提取出的新方法/变量签名，给 planner 直接看
    pub params: Vec<ParamInfo>,    // 名字 + (可选)类型 + 是否被写
    pub returns: Vec<ReturnInfo>,  // 名字 + (可选)类型
    pub confidence: Confidence,    // High | Medium | Low，与 shuttle-nav 同一枚举
    pub warnings: Vec<String>,     // 别名/控制流/类型占位等待确认项
}
```

## 7. 边界与降级规则（保持诚实，与阶梯一致）

沿用 `shuttle_lsp_capability_plan.md` §8 的诚实原则，本引擎的边界：

- **只做函数内**：跨函数/跨文件的语义重构（如「跨模块搬移方法并修所有引用」）**不由 locals 保证**，
  需要时降级为「本引擎出局部改动 + `shuttle rename`/`shuttle callers` 补跨文件引用 + dry-run 人审」。
- **类型注解不瞎猜**：go/rust/ts 需要类型时，两级启发式（就近声明→ctags）取不到就用
  **占位类型 + `TODO` 注释 + 降 confidence**，把不确定性显式交给 planner，绝不填一个看似合理的错类型。
- **别名/可变引用判不了就降级**：rust 的借用/所有权、python 的可变对象别名，locals 无法分析，
  一律**「提取 + 标记待确认」**，confidence ≤ Medium，`warnings` 写明原因。
- **复杂控制流直接拒绝**：选区含中段 `return`/`break`/`continue`/`goto`/标签跳转，或选区边界
  切断语法节点 → **拒绝并回报**，不产出可能改变控制流语义的重构。
- **一切改写先 dry-run**：默认出 diff + `RefactorPlan`，`--write` 才落盘，落盘经 `shuttle apply --gate`；
  任一项 confidence=Low 且未 `--force` → 拒绝落盘。
- **不常驻**：不引任何守护进程（与 `shuttle_single_instance_perf_plugin.md` §5、
  `shuttle_lsp_capability_plan.md` §8 边界一致）；`ts-morph` 适配也是一次性调用、用完即走。
- **非目标**：不追 IDE 级「所有重构都能一键且绝对安全」；shuttle 的承诺是**「高频重构在局部信息充分时
  快且正确，信息不足时诚实降级或拒绝」**。

## 8. 减少 LLM roundtrip 的收益（本方案的动机，承 capability_plan §4）

| 场景 | 现状（无命令，靠 LLM） | 有 `shuttle refactor` 后 | roundtrip 节省 |
|---|---|---|---|
| 「把这几行抽成方法」 | 模型读上下文、手推参数/返回值、手写调用、常漏参数或返回 | `shuttle refactor extract-method` 一次出签名+调用+diff | 多轮→1 |
| 「这个表达式抽个变量」 | 模型找所有出现、手改、可能漏 | `shuttle refactor extract-variable` 一次 | 多轮→1 |
| 「把这个局部变量改名」 | 模型 grep + 逐处判断是否同一变量 | `shuttle refactor rename-local`（locals 精确绑定，零噪声） | 多轮→1 |
| 「这个重构安全吗」 | 模型逐处推断副作用/控制流 | dry-run + `RefactorPlan.warnings` 显式列风险 | 收敛到 1 次审阅 |

**设计原则**（与 capability_plan §4 一致）：输出 self-contained + 带 confidence，让 planner
一次拿到「完整重构计划 + 可信度 + 待确认项」，而不是拿到半成品回来追问。

## 9. 落地顺序（增量、每批可验证）

- **批 1（最高性价比，无类型依赖）**：`shuttle-refactor` 骨架 + locals 求解 + `RefactorPlan` 模型
  + **`extract-variable` / `inline-variable` / `rename-local`**（这三个不需要类型注解、最可靠）。
  语言先覆盖 **python / javascript**（无类型注解，locals 直接够）。
- **批 2（提取方法核心）**：`extract-method` 的 params/returns 推导 + §3.4 合法性 gate，
  语言扩到 **go / rust / typescript**；go/rust 的返回类型走「就近声明→ctags」启发式 + 占位降级。
- **批 3（类型增强 + 跨文件）**：TS 的 `ts-morph` 可选适配（补返回类型注解）；
  `change-signature`（调用点用 `shuttle callers`）；`inline-method`。

每批：`cargo build && cargo test` 验证 → `shuttle-refactor` 单测（作用域求解 / params-returns 推导 /
合法性 gate / 各语言模板 / confidence 断言）→ 只提交本批相关文件（不带无关工作区改动）；
端到端延迟进 `benches/`（对齐 `architecture.md` §10 M6 与 capability_plan §9.12）。

## 10. 与既有文档的关系

- **`shuttle_lsp_capability_plan.md`**：本方案是其**二等 `codeAction` 条目中「重构类」部分的具体展开**。
  capability_plan 把 codeAction 映射到 `shuttle-codemod`（模板改写），本方案补上 `shuttle-codemod` 表达不了的
  「作用域+数据流相关重构」，二者合起来才完整覆盖 LSP codeAction 的能力语义。
- **`architecture.md` §7**：本方案落在「结构化绕过模型」阶梯里——重构能被 locals 确定性算出的部分
  不进模型，只有 locals 判不了的（复杂控制流/别名）才降级回抛。改写落盘完全复用 §6 的
  search/replace 模糊 apply。
- **`shuttle_single_instance_perf_plugin.md`**：严格遵守其无守护进程/无 warm engine 边界。

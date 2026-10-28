# shuttle 无守护进程「类 LSP 能力」方案

> **状态：implemented（核心能力 + 可选真实 LSP transport 均已落地）**。
> `shuttle-nav` 的 references/symbols/defs/rename/impls/callers/hover、磁盘缓存、confidence
> 模型及默认关闭的 `shuttle-lsp-bridge` 均已落地；起始提交为 `992efcbf`、`ab9614c7`、
> `bb7c51ea`。`real-lsp` feature 下的 tier-3 transport（`initialize -> didOpen ->
> typeDefinition/typeHierarchy -> shutdown -> exit`）已从 fail-closed scaffold 升级为
> 真实的 spawn-query-kill（用完即走）：纯 transport 的 `converse()` 在默认构建里可
> hermetic 测试，feature-gated 的 `real::resolve` 才 spawn 子进程、带 deadline 看门狗
> 驱动一趟对话后 kill+reap。默认构建仍完全 LSP-free，`shuttle typedef` 会诚实回报「需要
> `--features real-lsp` 重建」。设计详见 。
> 下文是原始能力设计和分批基线。

> 目标：在 shuttle 现有 `ctags + tree-sitter + ast-grep` 阶梯上，系统性补齐 LSP 级的代码导航/改写能力，
> 但**比 LSP 快、无常驻进程、贴合小快模型的低尾延迟定位**，并尽可能把原本要靠 LLM 多轮对话
> 才能完成的「找定义/找引用/改名/重构」压成一次进程内命令，减少 planner↔executor 的 roundtrip。
>
> 本文最初作为不含实现的方案文档编写。落地顺序见 §7。相关既有设计见 `architecture.md` §7 / §7.5、
> `shuttle_single_instance_perf_plugin.md`。
>
> **设计取向（重要）**：我们要的是 LSP 的**能力**，不是它的**接口/协议**。LSP 为「人在 IDE
> 长驻交互」设计，很多场景（JSON-RPC 往返、document sync、position encoding 折算、server
> 生命周期）对「一次 run 批量确定性编辑」的高速场景是纯开销。因此 shuttle **不实现 LSP 协议、不跟随
> `textDocument/*` 接口形态**，只借用其能力语义，用最贴合本场景的命令行/进程内形态重新表达，
> 一切以「更快、更少 roundtrip」为准。§2 的对照表仅用 LSP method 作能力锚点，不代表接口约束。
## 1. 背景与定位

shuttle 的既有共识（`architecture.md` §7）已经明确**不用 LSP rename**：LSP server 冷启动要索引整个
工程，常几秒到几十秒，与「小快模型 + 低尾延迟」冲突。shuttle 转而用**无守护进程的静态符号表**：

```
符号表精确改（shuttle-symbols） > ast-grep 规则 > comby > tree-sitter query > 模型
```

代码里已经落地的地基（本方案直接复用，不重造）：

| 既有能力 | 位置 | 说明 |
|---|---|---|
| ctags 全库扫描 | `shuttle-symbols::SymbolTable::scan` | `--output-format=json --fields=+n+K+s -R`，产出 `{name, kind, scope, file, line}` |
| ctags 显式文件扫描 | `scan_files` | `-f - <files>`，支持增量/范围扫描 |
| 作用域消歧 | `disambiguate_refs` / `genuine_identifier_lines` | ast-grep 解析语法树，裁掉 ctags 的文本级误命中 |
| 符号级重命名 | `rename` + `run_rename` | 查定义表 → 生成 `RenameEdit` → 消歧 → 落 search/replace |
| 文本引用探测 | `extract_references` | repo-map 建图用，本方案复用为 find-references 的候选层 |
| 结构定位 | `shuttle-locate::struct_locate` / `text_locate` | ast-grep(`sg`) / ripgrep(`rg`) |
| 规则改写 | `shuttle-codemod` | `sg scan --update-all`，零 token |
| repo-map | `repo_map_from` | ctags 标签图 + PageRank + token 预算 |
| 运行内缓存 / 范围扫描 | `scan_cached` / `scan_scoped` | ①/⑤ 优化，mtime+size 失效 / focus 邻域收敛 |

**结论先行**：LSP 的能力可分三层——① 纯静态查表就能给的（定义、大纲、层级），
② ctags 找候选 + tree-sitter 裁误命中就能给「够用精度」的（引用、改名），
③ 需要真正语义/类型推断才安全的（跨文件类型级重构、补全、类型提示）。
shuttle 应把 ①② 做成一等命令（多数编辑任务落在这里，能砍掉最多 roundtrip），
③ 明确标注为「启发式 / 需人确认」或按需桥接真 LSP，不假装 ctags 能给语义保证。
## 2. LSP 全能力清单与 shuttle 可达性逐项对比

按 LSP 协议 `textDocument/*` 与 `workspace/*` 请求逐条列。「shuttle 路径」列给出用什么工具达成；
「精度」列区分 **精确**（AST/查表可判定）/ **启发式**（文本或跨文件近似）/ **需语义**（要类型推断）。

> **注意**：shuttle 不跟随 LSP method 的接口形态，§2 的对照表仅用 LSP method 作能力锚点，
> 不代表接口约束。shuttle 用最贴合本场景的命令行/进程内形态重新表达。

| # | LSP 能力 | LSP method | shuttle 实现路径 | 精度 | 速度 vs LSP | 建议档位 |
|---|---|---|---|---|---|---|
| 1 | 跳转定义 | `definition` | ctags 定义表直接查 | 精确* | 冷启动0，快 100× | **一等** 已有基础 |
| 2 | 跳转声明 | `declaration` | ctags（`kind` 区分 proto/def） | 精确* | 同上 | 一等 |
| 3 | 文档符号/大纲 | `documentSymbol` | ctags 单文件 `scan_files` | 精确 | 快 | 一等 |
| 4 | 工作区符号搜索 | `workspace/symbol` | ctags 全表 + 名字/kind 过滤 | 精确 | 快 | 一等 |
| 5 | 查找引用 | `references` | ctags 定义锚点 + `extract_references` 收候选 + ast-grep 裁误命中 | 启发式→接近精确 | 快 | **一等**（新增 `shuttle references`） |
| 6 | 重命名 | `rename` | 已有 `run_rename` + `disambiguate_refs` | 启发式→接近精确 | 快 | **一等**（强制 dry-run gate） |
| 7 | 高亮同名出现 | `documentHighlight` | 单文件 ast-grep identifier 匹配 | 精确（文件内） | 快 | 二等 |
| 8 | 类型定义 | `typeDefinition` | ctags 只能给「同名类型定义」，无类型推断 | 需语义 | — | 三等（桥接/降级） |
| 9 | 实现跳转 | `implementation` | ctags 找 trait/interface 名 + ast-grep 找 `impl`/`implements` 结构 | 启发式 | 快 | 二等 |
| 10 | 悬停信息 | `hover` | ctags 定义签名行 + 就近 doc 注释抽取 | 启发式 | 快 | 二等 |
| 11 | 调用层级 | `callHierarchy` | ctags 定义 + ast-grep 匹配 `调用点` 模式建边 | 启发式 | 中 | 二等 |
| 12 | 类型层级 | `typeHierarchy` | ast-grep 抽 `extends/impl` 边 | 启发式 | 中 | 三等 |
| 13 | 代码补全 | `completion` | 需类型/上下文推断 | 需语义 | — | 三等（不做/桥接） |
| 14 | 签名帮助 | `signatureHelp` | ctags 拿到定义签名行可近似 | 启发式 | 快 | 三等 |
| 15 | 诊断（错误/告警） | `publishDiagnostics` | 语言编译器/linter 更合适（`tsc`/`cargo check`/`ruff`），非 shuttle 本职 | 需语义 | — | 桥接外部工具 |
| 16 | 代码动作/quick-fix | `codeAction` | 映射到 `shuttle-codemod` 规则集 | 精确（规则内） | 快 | 二等（已有基础） |
| 17 | 格式化 | `formatting` | 外部 formatter（`rustfmt`/`prettier`/`ruff`） | — | — | 桥接 |
| 18 | 折叠范围 | `foldingRange` | tree-sitter 结构 | 精确 | 快 | 低优先 |
| 19 | 语义高亮 | `semanticTokens` | tree-sitter | 精确 | 快 | 低优先（agent 用不上） |
| 20 | 内联提示 | `inlayHint` | 需类型推断 | 需语义 | — | 不做 |
| 21 | 链接 | `documentLink` | 正则/tree-sitter | 精确 | 快 | 不做 |
| 22 | 选区范围 | `selectionRange` | tree-sitter | 精确 | 快 | 不做 |

> \* **「精确\*」的边界**：ctags 给的是「同名符号在哪里定义」，对同名重载/跨语言同名符号不做消歧。
> 单一顶层定义时等价精确；存在同名歧义时降为「候选集 + tree-sitter 裁剪」，即第 5/6 项的路径。

**分层小结**：

- **一等（多数编辑任务落这里，砍 roundtrip 收益最大）**：定义、声明、大纲、工作区符号、查找引用、重命名。
- **二等（有价值但精度启发式，给 confidence 标注）**：高亮、实现跳转、悬停、调用层级、code action。
- **三等（需真语义，明确降级或按需桥接真 LSP）**：类型定义、类型层级、补全、诊断、格式化。
## 3. 为什么这套「比 LSP 快、更适合高速场景」

| 维度 | LSP server | shuttle 静态阶梯 |
|---|---|---|
| 冷启动 | 索引整个工程，秒~几十秒 | **0**：ctags 一次进程内调用即出结果 |
| 常驻开销 | 常驻进程 + 内存索引 | 无守护进程，用完即退 |
| 单次查询延迟 | 索引后快，但首次贵 | 全库 ctags 毫秒~秒级；⑤ `scan_scoped` 收敛到 focus 邻域后更快 |
| 与 agent 生命周期 | 需管理 server 生命周期、崩溃恢复 | 无状态，天然契合「一次 run 一批操作」 |
| 增量 | 靠 server 内部增量 | ①`scan_cached`（mtime+size 失效）+ ⑤`scan_scoped`（git-tracked ∩ 引用邻域） |
| 多语言 | 每语言一个 server | ctags 覆盖 40+ 语言，一个二进制；ast-grep 覆盖主流语言 |

关键：shuttle 面对的是**「一次 run 里做一批确定性编辑」**，不是「人在 IDE 里长时间交互」。
LSP 为后者优化（长驻换低单次延迟），shuttle 为前者优化（零冷启动换整体吞吐）。⑤ 的 `scan_scoped`
已经证明：把扫描收敛到 focus 邻域，能把一个会超时的全库扫描变成 ~6.6s 可用操作。

## 4. 减少 LLM roundtrip 的收益分析（本方案的核心动机）

planner（慢模型）最贵的是「多轮试探——读文件、猜引用、改一处、发现漏改、再改」。每一轮都是一次
LLM roundtrip。把 LSP 能力做成进程内命令，等于把这些试探折叠成**一次确定性调用**：

| 场景 | 现状（无命令，靠 LLM） | 有 shuttle 命令后 | roundtrip 节省 |
|---|---|---|---|
| 「这个符号在哪定义」 | 模型 grep 猜 + 读多个文件确认 | `shuttle locate`/定义查表 一次 | N→0（executor 直答） |
| 「改名 foo→bar」 | 模型逐文件搜、逐处改、漏改再补 | `shuttle rename --dry-run` 出全量 diff | 多轮→1 |
| 「谁调用了这个函数」 | 模型 grep + 逐个读判断真假引用 | `shuttle references --symbol` 带 confidence | 多轮→1 |
| 「给我改动点周边结构」 | 模型来回读文件建立心智 | `shuttle repomap --focus`（已有） | 多轮→1 |
| 「这个改动安全吗」 | 模型逐处推断 | dry-run diff + gate，模型只审 diff | 收敛到 1 次审阅 |

设计原则：**命令输出要 self-contained 且带 confidence**，让 planner 一次拿到「候选 + 可信度」，
而不是拿到半成品再回来问。低可信度项显式标注，把「是否信任」的决定权交给上游，而不是隐藏不确定性。
## 5. 新增/增强命令建议（接口草案）

复用既有 `scan_scoped`/`extract_references`/`disambiguate_refs`,新增均为进程内一次性调用:

1. **`shuttle references --symbol <name> [--root R] [--scan-scope] [--json]`**（一等，投入最小）
   - 定义锚点来自 ctags 表 → `extract_references` 在（可 `--scan-scope` 收敛的）文件集里收候选
   - `disambiguate_refs` 用 ast-grep 裁掉注释/字符串/字段误命中
   - 输出 `[{path, line, kind, confidence}]`：定义 scope 内命中=high，跨文件裸名命中=medium

2. **`shuttle rename` 增强**（一等）
   - 已有 `run_rename`；补 **默认 dry-run 出 diff**，`--write` 才落盘
   - 落盘复用 `shuttle apply --gate` 单进程 probe+real 通道（③），保持 all-or-nothing

3. **`shuttle symbols --file <f>` / `shuttle symbols --query <q>`**（一等）
   - documentSymbol / workspace/symbol：直接投影 `scan_files` / 全表，`--json` 输出大纲

4. **`shuttle impls --type <name>`**（二等）
   - ast-grep 模式匹配 `impl <name>` / `implements <name>` / trait 实现

5. **`shuttle callers --symbol <name>`**（二等）
   - ast-grep 匹配调用点模式建边，输出调用者列表（启发式，带 confidence）

6. **`shuttle hover --symbol <name>`**（二等）
   - ctags 定义签名行 + 就近 doc 注释；给模型「不用读整个文件就懂签名」

> 三等能力（补全/类型定义/诊断）不新增 ctags 路径命令；如确需，走「按需拉起真 LSP，用完即走」的
> 独立适配器，不混进静态阶梯，避免把「已知不精确」包装成「看起来精确」。
## 6. 外部工具选型

| 工具 | 角色 | 已在用 | 补充理由 |
|---|---|---|---|
| universal-ctags | 快而广找候选/定义 | ✅ | 40+ 语言，冷启动 0 |
| tree-sitter / ast-grep(`sg`) | 准而裁误命中、结构匹配 | ✅ | 已用于 `disambiguate_refs`、`shuttle-codemod`、`shuttle-locate` |
| ripgrep(`rg`) | 文本定位 | ✅ | `shuttle-locate::text_locate` |
| comby | 结构化改写兜底 | 阶梯已列 | 仅在无 grammar 且未挂 customLanguages 时的降级路径 |
| tree-sitter-cli + gcc | 编译自定义 grammar 为 `.so` | 构建期 | 供 ast-grep `customLanguages` 挂载 thrift/自写 grammar |
| taplo | toml 主后端（fmt/lint/schema/键路径读写） | 建议装 | Rust 实现冷启动低；ctags toml parser 默认禁用、ast-grep 无 toml grammar，taplo 是唯一可达后端 |
| 语言编译器/linter | 诊断 | 桥接 | 见 §6.6 各语言清单 |
| 真 LSP（rust-analyzer/tsserver） | 三等能力按需桥接 | 可选 | 仅在需要类型级精度时，按需拉起用完即走 |

**是否自定义工具**：优先复用 ctags+ast-grep 组合。thrift 和 bash 是仅有的两个 ast-grep
内置无 grammar 的开发语言，但 **ast-grep 支持 `customLanguages`——把任意 tree-sitter grammar
编译成动态库、在 `sgconfig.yml` 注册后即可像内置语言一样用 `-l <lang>` 做 pattern/rule/改写/outline**，
因此这两种语言可以从「文本级」直接升到「AST 级」，且**仍走 ast-grep 一个引擎、复用
`disambiguate_refs`/`shuttle-codemod`/outline 同一条 pipeline，不引第二套 query 工具、不引守护进程**。
详见 §6.7。相比之下 comby/正则只作为「没挂 grammar 时」的降级路径，不是首选。

**taplo 的具体作用（补 TOML 空白）**：`taplo fmt` 格式化、`taplo lint` 语法/结构校验、
JSON Schema 校验（`Cargo.toml`/`pyproject.toml` 等已知 schema）、语法感知的键路径读写
（`taplo get`/`taplo set`，正是配置文件「定位键、改值」的主诉求）。因 ctags 的 TOML parser
默认禁用、ast-grep 无 TOML grammar，taplo 是 shuttle 处理 TOML 的唯一可达后端。

## 6.5 语言支持矩阵（必须覆盖的核心语言）

必须支持的开发语言：**go / rust / thrift / typescript / javascript / python / bash**；
配置语言：**yaml / json / toml / ini**。下表列每种语言在各能力层上「靠哪个后端、能到什么精度」。

> 后端可达性均在本机实测（universal-ctags 6.2.1 / ast-grep 0.45.1）：
> ctags 覆盖全部 11 种（TOML parser 存在但默认 disabled，需 `--languages=+TOML`）；
> ast-grep 内置覆盖 go/rust/ts/tsx/js/python/yaml/json，内置**不含 bash/toml/thrift/ini**——
> 但 bash/thrift 可经 `customLanguages` 挂 grammar 补齐到 AST 级（见 §6.7）。

| 语言 | ctags 定义/大纲 | ast-grep 结构/消歧 | 引用+改名(一等) | 结构改写(codemod) | 诊断/格式化(桥接) | 备注 |
|---|---|---|---|---|---|---|
| **go** | ✅ Go parser | ✅ go | ✅ 全链路 | ✅ ast-grep rules | `gofmt`/`goimports`/`staticcheck`/`golangci-lint` | 全链路可达 |
| **rust** | ✅ Rust parser | ✅ rust | ✅ 全链路 | ✅ ast-grep rules | `rustfmt`/`cargo check`/`clippy` | 全链路可达 |
| **thrift** | ✅ Thrift parser | ⚠️→✅ 挂 customLanguages 后 | ⚠️→✅ 挂 grammar 后 | ⚠️→✅ ast-grep(挂 grammar) | `thrift -gen` 语法校验 | 建议**自写 grammar**（语法封闭、无 scanner）；未挂前 ctags+rg 启发式 |
| **typescript** | ✅ TypeScript parser | ✅ typescript | ✅ 全链路 | ✅ ast-grep rules | `prettier`/`tsc --noEmit` | 全链路可达 |
| **javascript** | ✅ JavaScript parser | ✅ javascript | ✅ 全链路 | ✅ ast-grep rules | `prettier`/`biome` | 全链路可达 |
| **python** | ✅ Python parser | ✅ python | ✅ 全链路 | ✅ ast-grep rules | `ruff format`+`ruff check` | 全链路可达 |
| **bash** | ✅ Sh parser | ⚠️→✅ 挂 customLanguages 后 | ⚠️→✅ 挂 grammar 后 | ⚠️→✅ ast-grep(挂 grammar) | `shfmt`/`shellcheck` | **复用 `tree-sitter-bash`**（依赖 external scanner，不自写）；未挂前 ctags+rg 启发式 |
| **yaml** | ✅ Yaml parser | ✅ yaml | 键路径级（非符号） | ✅ ast-grep | `yamllint`/`yq` | 配置多用「键定位/改值」 |
| **json** | ✅ JSON parser | ✅ json | 键路径级 | ✅ ast-grep | `jq`（校验+改写） | 结构化最完整 |
| **toml** | ⚠️ 默认禁用 | ❌ 无 grammar | 键路径级（需启用 ctags） | ⚠️ `taplo`/正则 | `taplo fmt`/`taplo lint` | 建议用 `taplo` 作主后端 |
| **ini** | ✅ Iniconf parser | ❌ 无 grammar | 键路径级（ctags） | 正则 | — | 结构简单，正则+ctags 足够 |

**归纳**：7 种开发语言里 go/rust/ts/js/python 五种是「ctags+ast-grep 全链路」；
**thrift 和 bash 内置无 ast-grep grammar，但可经 `customLanguages` 挂 grammar 升到 AST 级**（§6.7）——
bash 复用成熟的 `tree-sitter-bash`，thrift 建议自写（语法封闭、无 external scanner）；未挂前退到 ctags+rg 启发式。
配置语言里 json/yaml 最完整；**toml 用 `taplo` 作主后端**（ctags 默认不解析且 ast-grep 无 grammar）；
ini 语法正则可判定，ctags+正则足够，不值得上 AST。
## 6.6 各语言外部依赖清单（formatter / linter / 校验器）

shuttle 核心能力（ctags/ast-grep/rg/comby）不依赖以下工具；下表是把诊断/格式化档位拉满时需要的东西。
标注本机现状（mac 实测）供参考；缺失工具时 shuttle 自动降级，不影响核心导航/改写能力。

| 语言 | formatter | linter/静态检查 | 类型/编译校验 | 本机现状 |
|---|---|---|---|---|
| go | `gofmt`（内置）、`goimports` | `golangci-lint`、`staticcheck` | `go build`/`go vet` | ✅ 全部已装 |
| rust | `rustfmt` | `clippy`（`clippy-driver`） | `cargo check` | ✅ 全部已装 |
| thrift | 无标准 fmt | — | `thrift -gen <lang>` 语法校验 | ✅ thrift 已装 |
| typescript | `prettier`（或 `biome`/`dprint`） | `eslint`/`biome` | `tsc --noEmit` | ✅ prettier；⚠️ tsc/biome 未装 |
| javascript | `prettier`（或 `biome`） | `eslint`/`biome` | — | ✅ prettier 已装 |
| python | `ruff format`（已含 black 兼容） | `ruff check`（已含 flake8 覆盖） | `pyright`（快，首选）/`mypy` | ✅ ruff；⚠️ pyright/mypy 未装 |
| bash | `shfmt` | `shellcheck` | `bash -n` 语法校验 | ✅ shfmt/shellcheck 已装 |
| yaml | `yamlfmt`/`prettier` | `yamllint` | `yq` schema 校验 | ✅ yq；⚠️ yamlfmt/yamllint 未装 |
| json | `jq`/`prettier` | — | `jq -e`/schema | ✅ jq/prettier 已装 |
| toml | `taplo fmt` | `taplo lint` | `taplo` schema | ⚠️ taplo 未装（**建议优先装**） |
| ini | — | — | — | — |

**优先补齐**（当前缺口，按收益排序）：
1. `taplo` — toml 唯一可用的格式化/校验后端，不装则 toml 只有正则降级
2. `tsc` / `biome` — TS 类型校验；biome 可同时替代 eslint+prettier
3. `pyright` — python 类型检查（TS/Node 实现，增量快，契合 shuttle 低尾延迟；`mypy` 作生态兼容兜底）
4. `yamllint` — yaml 校验

**部署原则**：shuttle 启动时探测 PATH，缺某工具时该语言诊断/格式化档位自动降级（而非报错），
核心导航/改写能力（ctags+ast-grep+rg）始终不受影响。
## 6.7 ast-grep `customLanguages`：给 thrift/bash 挂 AST（不引第二引擎）

ast-grep 不限于内置语言。把 tree-sitter grammar 编译成动态库（`.so`/`.dylib`/`.dll`），
在 `sgconfig.yml` 的 `customLanguages` 下注册，即可像内置语言一样用 `-l <lang>` 匹配/改写：

```yaml
# sgconfig.yml
customLanguages:
  thrift:
    libraryPath:                 # 支持按 target-triple 分平台给不同 .so
      aarch64-apple-darwin: parsers/thrift-mac.dylib
      x86_64-unknown-linux-gnu: parsers/thrift-linux.so
    extensions: [thrift]
    outlineRules: outline/thrift.yml   # 有它 `shuttle symbols`/大纲对该语言也生效
    expandoChar: _               # $VAR 在该 grammar 非法时用它替换 metavariable
```

- **`libraryPath`** 可为按平台的 target-triple map，便于预编译 `.so` 分发；
- **`outlineRules`** 让 `ast-grep outline` 对自定义语言也能出符号大纲，一并补上 documentSymbol/workspace symbol；
- **`expandoChar`** 处理 `$VAR` 在某些 grammar 里不是合法语法的情况。

**收益**：thrift/bash 的引用/改名从「⚠️ 文本启发式」升到「✅ AST 级」，与 go/rust 同档；
仍是进程内一次性调用、无守护进程，与 §8 边界一致。
**构建期成本**：需 `tree-sitter-cli` + gcc 把 grammar 编成 `.so` 并按平台预编译分发（一次性，运行期零负担）。

### grammar 来源：复用 vs 自写

| 语言 | 策略 | 理由 |
|---|---|---|
| **bash** | **复用 `tree-sitter-bash`** | 依赖 C external scanner（heredoc/展开/词法拆分/引号），自写极易出 bug；社区实现成熟（neovim/GitHub 在用） |
| **thrift** | **自写可行（值得）** | Thrift IDL 语法封闭（namespace/typedef/const/enum/struct/union/exception/service/field），**无表达式优先级、无需 external scanner**，只需覆盖结构子集即可喂 disambiguate/codemod；社区 `tree-sitter-thrift` 可参考/兜底 |
| **toml** | **不写，用 `taplo`** | 配置文件诉求是键路径定位/改值/schema 校验，taplo 直接覆盖，比写 grammar 划算 |
| **ini** | **不写，ctags+正则** | 语法（`[section]`+`key=value`）正则可判定，ctags Iniconf parser 已可用，上 AST 属过度工程 |

**自写 grammar 的判据**（三条同时满足才自写）：① 社区无成熟 grammar；② 不需要 external scanner；
③ shuttle 只需「结构子集」而非编译器级完整语法。thrift 三条基本满足 → 自写；ini 因正则已够而不写；
bash 因强依赖 scanner + 社区已成熟 → 坚决复用不自写。关键点：**shuttle 只需认出「定义/引用/字段/结构边」
来喂 `disambiguate_refs` 和 codemod，面向结构子集写比面向编译器写容易一个量级**。
## 7. 落地顺序（增量、每批可验证）

- **批 1（最高性价比，复用现成原语）**：`shuttle references` + `shuttle rename` dry-run gate 增强 + `shuttle symbols`。
  这批砍掉最多 roundtrip，且几乎不造新引擎；lang_for_path 同时补齐 bash/sh/thrift/yaml/toml/ini。
- **批 2（启发式增强，带 confidence）**：`shuttle impls` / `shuttle callers` / `shuttle hover`。
- **批 3（桥接层，可选）**：诊断桥接编译器；toml 接 taplo；
  **thrift/bash 经 ast-grep `customLanguages` 挂 grammar**（bash 复用 `tree-sitter-bash`、thrift 自写）升 AST 级；
  三等语义能力按需桥接真 LSP 适配器。

每批：`cargo build && cargo test` 验证 → 只提交本批相关文件（不带无关工作区改动）。

## 8. 边界与非目标（保持诚实）

- **不追 IDE 级正确性**：一等能力在单一定义时精确，同名歧义时降为「候选+裁剪」，永远标 confidence。
- **不常驻**：不引入任何守护进程/warm engine（与 `shuttle_single_instance_perf_plugin.md` §5 边界一致）。
- **重构（refactor）不用 ctags 硬上**：跨文件类型级重构需 AST 变换 + 语义保持验证，
  应走 tree-sitter query 定制或真 LSP，且**默认 dry-run + gate**，不假装安全。
- **诊断/格式化/补全不自造**：交给编译器/formatter/（按需）真 LSP，见 §6.6 各语言外部依赖清单。
- **thrift / bash / toml 精度诚实标注**：三者内置无 ast-grep grammar（见 §6.5）。**未挂 grammar 时**
  thrift/bash 退到 ctags+rg 启发式，输出必须标 confidence；**挂 `customLanguages` 后升到 AST 级**
  （§6.7，bash 复用 tree-sitter-bash、thrift 自写）；toml 用 `taplo`。自写 grammar 只覆盖结构子集，
  非编译器级完整语法，仍保留 dry-run + confidence，不假装 IDE 级正确。
- **外部依赖优雅降级**：缺 formatter/linter 时该语言诊断档位自动降级，核心导航/改写不受影响。
- **一切改写先 dry-run**：把「文本级不保证」变成流程上的一道 gate，而非隐藏。


## 9. shuttle 特化版 LSP —— 详细设计

本节把 §2/§5 的能力清单落成可实现的详细设计。**核心立场重申**:不实现 LSP 协议、不常驻、
不做 `textDocument/*` 增量同步;把"人在 IDE 长驻交互"重构成"一次 run 内的批量确定性命令"。
所有设计锚定已核对的真实接口:`shuttle-symbols`(`SymbolTable::{scan,scan_cached,scan_scoped,defs}`、
`rename`、`disambiguate_refs`、`genuine_identifier_lines`、`extract_references`、`repo_map_from`、
`lang_for_path`)、`shuttle-locate`(`text_locate`/`struct_locate`)、`shuttle-codemod`(`dry_run`/`apply`)、
`shuttle-editcore`(`apply_search_replace`)、`shuttle-protocol`(`EditPlan`/`EditOp`/`ApplyOutcome`/`ScopedPathResolver`)。

### 9.1 总体架构:三层,无 server

```
                     ┌─────────────────────────────────────────┐
   planner(慢模型) ──▶│  shuttle-cli  (一次 run,一批子命令,进程内)     │
                     ├─────────────────────────────────────────┤
   L1 能力面(命令)   │ references / rename / symbols / defs /    │
                     │ impls / callers / hover                   │
                     ├─────────────────────────────────────────┤
   L2 索引/查询核心   │ shuttle-nav (新 crate):Index + Query + 消歧    │
                     │   复用 shuttle-symbols 的 scan_cached/defs/... │
                     ├─────────────────────────────────────────┤
   L3 后端适配        │ ctags | ast-grep(内置+customLanguages) |  │
                     │ rg | taplo | (按需)真 LSP 适配器          │
                     └─────────────────────────────────────────┘
```

- **L1 能力面**:每个能力是一个 `shuttle <verb>` 子命令,输入 self-contained、输出带 `confidence`,
  一次调用给"候选 + 可信度",不让 planner 回来追问。
- **L2 `shuttle-nav`(新增 crate)**:唯一新增引擎位。它**不重造索引**,而是把 `shuttle-symbols` 的
  `SymbolTable` 当作定义层、`disambiguate_refs`/`genuine_identifier_lines` 当作消歧层,
  组合出 references/impls/callers/hover 的查询逻辑。放独立 crate 是为了让 `shuttle-cli` 薄、
  让查询逻辑可单测。
- **L3 后端**:全部无状态、用完即退;唯一"有生命周期"的是可选的真 LSP 适配器(§9.9),
  且严格隔离在三等能力,不混进 L2。

### 9.2 统一数据模型(所有能力共享)

在 `shuttle-nav` 定义一组贯穿所有能力的类型,`shuttle-protocol` 已有的 `EditPlan`/`EditOp` 复用为改写出口:

```rust
/// 一次能力查询的统一命中项。references/impls/callers/defs 都产出它。
pub struct NavHit {
    pub path: String,
    pub line: usize,
    pub col: Option<usize>,      // ast-grep 命中给列;ctags/rg 行级可为 None
    pub kind: NavKind,           // Definition | Reference | Impl | Caller | Declaration
    pub symbol: String,
    pub snippet: Option<String>, // 该行文本,给 planner 免读文件
    pub confidence: Confidence,  // High | Medium | Low
    pub source: Backend,         // Ctags | AstGrep | Rg | Taplo,用于可解释性
}

pub enum Confidence { High, Medium, Low }
```

**confidence 判定规则(全局统一,写进 §9 而非各命令各自发明)**:
- **High** = AST 可判定:ast-grep 在定义所属 scope 内命中、或经 `disambiguate_refs` 保留的命中。
- **Medium** = 跨文件裸名命中且通过 `genuine_identifier_lines`(排除注释/字符串/字段名)。
- **Low** = 仅 ctags/rg 文本命中,未过 AST 消歧(无 grammar 语言如未挂 customLanguages 的场景)。

输出统一 `--json` 走 serde,人读走紧凑表格。planner 侧只需按 `confidence` 决定是否直接采信。

### 9.3 索引层:run 内一次,增量复用

复用既有三级优化(§7.5 已落地),`shuttle-nav` 只做编排,不新写缓存:

| 场景 | 用哪个 | 行为 |
|---|---|---|
| 首次全库 | `SymbolTable::scan(root)` | ctags 一次进程内调用 |
| run 内二次查询 | `SymbolTable::scan_cached(root, cache)` | mtime+size 失效,命中即免扫 |
| 有 focus(改动点已知) | `SymbolTable::scan_scoped(root, focus)` | 收敛到 git-tracked ∩ 引用邻域 |

**关键设计**:一次 `shuttle run` 内,索引对象在进程内单例持有(不落盘 server),
多个子命令(先 `references` 再 `rename`)共享同一 `SymbolTable`,把"扫一次库"摊薄到多个能力上。
这是 shuttle 相对 LSP 的结构性优势:LSP 每个 server 各自索引,shuttle 一次 run 一张表喂所有能力。

> **跨 run 的增量索引(探针)**:本节的索引是 `shuttle run` 进程内单例、run 结束即释放。若要把索引复用拉长到跨 worktree / 跨任务的流水线尺度,另见  —— 把 SCIP 索引器当作 external 预热/合回两端的工位,喂给 splitter 的 DAG(读写集)与 touch_hint,而非进 shuttle 热路径。

### 9.4 能力 1:`shuttle references`(一等,新增,投入最小)

**目标**:替代"planner grep 猜 + 逐文件读确认真假引用"的多轮试探。

**命令**:`shuttle references --symbol <name> [--root R] [--kind fn|type|..] [--scan-scope] [--json]`

**算法(纯组合既有原语,无新引擎)**:
1. `table.defs(name)` 取定义锚点(可能多个 → 多定义时在输出里分组标注歧义)。
2. 候选收集:对候选文件集(默认全库;`--scan-scope` 时 `scan_scoped` 收敛)逐文件读入,
   用 `extract_references(content, defined)` 收裸名引用行。
3. 消歧:对每个候选文件调 `genuine_identifier_lines(path, name)`,裁掉注释/字符串/字段误命中;
   对有 ast-grep grammar 的语言(含挂了 customLanguages 的 thrift/bash),进一步用
   `disambiguate_refs` 提到 AST 级(High)。
4. confidence 标注:定义 scope 内=High,跨文件裸名过消歧=Medium,仅文本命中=Low。
5. 输出 `Vec<NavHit>`,按 confidence 降序、同级按 path/line。

**为什么快**:全程 ctags + 文件读 + ast-grep 单次,冷启动 0;`--scan-scope` 把会超时的全库扫描
收敛到 focus 邻域(§3 实测 ~6.6s 级)。

### 9.5 能力 2:`shuttle rename`(一等,增强现有 `rename`/`run_rename`)

**现状**:已有 `rename(table, from, to) -> Vec<RenameEdit>` + `disambiguate_refs`。
**增强点**:把它从"直接改"变成"**默认 dry-run 出全量 diff,`--write` 才落盘**"。

**命令**:`shuttle rename --from <a> --to <b> [--scan-scope] [--write] [--json]`

**流程**:
1. `rename(&table, from, to)` 生成 `Vec<RenameEdit>{path,line,old,new}`。
2. `disambiguate_refs(candidates)` 裁误命中(复用,不重写)。
3. 转换为 `EditPlan`(`shuttle-protocol`):每个 `RenameEdit` → 一条 search/replace `EditOp`。
4. **dry-run(默认)**:走 `shuttle-codemod::dry_run` 风格产出 diff + 每处 confidence,不落盘。
5. **`--write`**:经 `shuttle apply --gate` 单进程 probe+real 通道(§③ 已落地)落盘,保持 all-or-nothing;
   底层是 `shuttle-editcore::apply_search_replace`。
6. **gate**:任一命中 confidence=Low 且未加 `--force` → 拒绝落盘,回报"需人确认的低可信项"。

**roundtrip 收益**:planner 一次拿到全量 diff + 风险标注,把"逐文件搜改、漏改再补"压成 1 次审阅。

### 9.6 能力 3:`shuttle symbols` / `shuttle defs`(一等)

- `shuttle symbols --file <f>`(documentSymbol):`scan_files`(显式文件 ctags)投影单文件大纲。
- `shuttle symbols --query <q>`(workspace/symbol):全表 `scan_cached` + 名字/kind 过滤。
- `shuttle defs --symbol <name>`(definition/declaration):`table.defs(name)`,`kind` 区分 proto/def。
- 全部 `--json` 输出 `Symbol{name,kind,scope,path,line,signature}`(已有结构,直接 serde)。

这三个是"纯静态查表"档,精确、快,是 planner 定位的第一入口(替代 grep 猜)。

### 9.7 能力 4/5/6:`shuttle impls` / `shuttle callers` / `shuttle hover`(二等,带 confidence)

这三个是 ast-grep 结构匹配 + ctags 锚点的组合,精度启发式,**输出强制标 confidence**:

| 命令 | LSP 锚点 | 实现 | confidence 上限 |
|---|---|---|---|
| `shuttle impls --type <T>` | implementation | `struct_locate` 匹配 `impl <T>`/`implements <T>`/trait 实现 | High(有 grammar)/ Medium |
| `shuttle callers --symbol <f>` | callHierarchy(incoming) | ast-grep 匹配调用点模式 `<f>($$$)` 建边 | Medium(调用点模式近似) |
| `shuttle hover --symbol <name>` | hover | `defs` 拿签名行 + 就近 doc 注释抽取 | High(签名精确)/ Medium(注释启发) |

`shuttle callers` 明确只做 incoming(谁调用我);outgoing 用 repo-map 已够,不重复造。

### 9.8 thrift/bash 的 AST 化接入(承 §6.7)

`shuttle-nav` 在选后端时,语言分派统一走**扩展后的 `lang_for_path`**——批 1 要给它补
`sh/bash → bash`、`thrift → thrift`、`yaml/toml/ini`。分派逻辑:

```
lang_for_path(path):
  内置 ast-grep 语言(go/rust/ts/js/python/yaml/json) → 直接用内置 grammar
  thrift/bash 且 sgconfig.yml 注册了 customLanguages   → 用 custom grammar(AST 级, High)
  否则(无 grammar / 未注册)                            → 降级 ctags+rg(Medium/Low),标 confidence
```

**关键**:接入 customLanguages **不改 `shuttle-nav` 的能力代码**,只改后端探测——
探测 `sgconfig.yml` 是否声明了对应 `customLanguages` 且 `.so` 存在,存在则该语言的 references/rename
自动升到 High。这保证"挂 grammar"是纯配置动作,不触碰查询逻辑。

### 9.9 三等能力:真 LSP 的按需桥接(隔离适配器)

类型定义 / 补全 / 类型层级等"需真语义"的能力,**不进 L2**,走独立 `shuttle-lsp-bridge` 适配器:
- 仅在 planner 显式请求且任务确需类型级精度时拉起对应 real LSP(rust-analyzer/tsserver/pyright)。
- **用完即走**:一次查询拉起 → 查 → 杀,不常驻(与 §8"不常驻"边界一致)。
- 输出仍归一为 `NavHit`,`source=Lsp`、`confidence=High`,但显式标注"经外部 LSP,非 shuttle 静态阶梯"。
- 这层是可选编译 feature,默认不启用,保证核心二进制不依赖任何 LSP server。

### 9.10 与 planner 的契约(减 roundtrip 的落点)

1. **命令 self-contained**:每次输出含 `snippet`,planner 不必再读文件确认。
2. **confidence 显式**:High 可直接采信,Medium/Low 交由 planner 决定是否复核,不隐藏不确定性。
3. **dry-run 优先**:所有改写默认出 diff,planner 只审 diff,把"改—发现漏改—再改"收敛到 1 次。
4. **一次 run 多能力共享索引**:planner 在一个 run 里连续 `references`→`rename`,索引只建一次。
5. **JSON 稳定 schema**:`NavHit`/`Symbol`/`EditPlan` 均 serde 固定字段,planner 侧可稳定解析。

### 9.11 crate 落点与新增边界

| crate | 改动 | 说明 |
|---|---|---|
| `shuttle-symbols` | 扩 `lang_for_path`(+bash/sh/thrift/yaml/toml/ini);`defs`/`extract_references` 复用 | 不动核心扫描 |
| **`shuttle-nav`(新增)** | references/impls/callers/hover 查询逻辑 + `NavHit`/`Confidence` 模型 | 唯一新引擎位,可单测 |
| `shuttle-cli` | 新增 `references`/`symbols`/`defs`/`impls`/`callers`/`hover` 子命令;`rename` 加 dry-run/`--write` | 薄封装,调 `shuttle-nav` |
| `shuttle-codemod`/`shuttle-editcore` | 复用 `dry_run`/`apply_search_replace` 作 rename 落盘 | 不改 |
| `shuttle-protocol` | `EditPlan`/`EditOp`/`ApplyOutcome` 复用作改写出口 | 不改 |
| `shuttle-lsp-bridge`(可选 feature) | 三等能力真 LSP 适配器 | 默认不编译 |

**新增边界**:只加一个必选 crate(`shuttle-nav`)+一个可选 feature crate(`shuttle-lsp-bridge`);
其余全是既有原语的组合,兑现 §1"复用地基、不重造"。

### 9.12 验证与落地映射(对齐 §7 批次)

- **批 1**:`shuttle-nav` 骨架 + `NavHit` 模型 + `references`/`symbols`/`defs` + `rename` dry-run gate
  + `lang_for_path` 扩语言。验证:`cargo build && cargo test`,加 `shuttle-nav` 单测(定义/引用/消歧/confidence)。
- **批 2**:`shuttle impls`/`shuttle callers`/`shuttle hover`,补 confidence 断言测试。
- **批 3**:thrift/bash 的 customLanguages 接入(bash 复用 tree-sitter-bash、thrift 自写 grammar)+
  `shuttle-lsp-bridge` 可选适配器 + 诊断桥接。
- 每批只提交本批相关文件,不带无关工作区改动;端到端延迟进 `benches/`(§8 已有基准位)。

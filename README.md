# shuttle

面向编码 agent 的**延迟优先执行内核（execution kernel）**。它为**小而快的模型**优化：
这类模型的职责是*执行*上游"聪明但慢"的规划器下发的计划——用尽可能少的 token、尽可能少的
模型往返、尽可能低的长尾延迟。

本项目不重新发明 agent 循环；它作为宿主 agent 的编辑 / 导航 / 重构执行器接入。

命名取自纺织隐喻：上游规划器排定纹样，shuttle 是梭，把每一根纬线快速、精确地
穿进织物（代码库）。它是织机里的部件，不驱动织机。

## 为什么用 Rust

整个编辑栈（ast-grep、similar、imara-diff、tree-sitter）全是原生 Rust——无 FFI、
热路径上无 GC 抖动、零拷贝 diff。所有分析都是**无守护进程（daemonless）**的：
每次查询都是一次性纯调用，冷启动约为 0。

## 目录结构

```
crates/
  shuttle-protocol    plan/op/outcome wire 契约、路由、编排
  shuttle-locate      ripgrep / ast-grep 定位封装
  shuttle-symbols     ctags 符号表、repo-map、符号级引用图
  shuttle-nav         引用 / 定义 / 调用者 / 实现 / 重命名 / 变更影响
  shuttle-refactor    AST 精确重构（提取/内联/移动/改签名）
  shuttle-editcore    4 级模糊 search/replace 应用（热路径）
  shuttle-codemod     ast-grep 规则驱动的零 token 机械编辑
  shuttle-conflict    git 合并冲突解决引擎
  shuttle-goal        持久化本地 Goal 日志、CAS/fencing、产物、条件
  shuttle-lsp-bridge  默认关闭的真实 LSP 适配器（tier-3，feature 门控）
  shuttle-cli         `shuttle` 二进制——宿主 agent 入口
rules/           ast-grep codemod 规则
docs/            architecture.md、能力规划
examples/        plan.sample.json
```

## 快速开始

```bash
cargo build --release
./target/release/shuttle apply --plan examples/plan.sample.json --dry-run
```

可选外部工具在 `PATH` 上存在时会升级分析阶梯：
[universal-ctags](https://github.com/universal-ctags/ctags)（符号索引）和
[ast-grep](https://ast-grep.github.io/)（结构化匹配）。二者缺失时，
`shuttle` 退化为文本级结果而不是失败。

## 核心思想

1. **最小编辑表示** —— 内容锚定的 search/replace 块，绝不用行号 diff。
   见 。
2. **模糊应用阶梯** —— 精确 → 空白归一化 → 相似度 → 完全未命中。
   只有完全未命中才昂贵。§6。
3. **绕过模型** —— 可规则化的编辑走 ast-grep codemod，零 token 成本。§7。
4. **变更影响分析** —— `shuttle impact --changed <files>` 沿原生符号引用图
   报告反向依赖闭包（一次变更波及哪些符号 / 文件 / 测试），用于测试选择与任务调度。

完整设计见 [docs/architecture.md]。

## 导航与重构

```bash
shuttle references --symbol parse_query        # 全部引用，带置信度
shuttle callers    --symbol run                # 谁调用了它
shuttle impact     --changed src/parser.ts     # 反向依赖波及面
shuttle refactor extract-method ...            # AST 精确，tree-sitter 支撑
```

重构引擎是 AST 精确的（tree-sitter），覆盖方法提取/内联、变量与常量提取/内联、
重命名、改签名、移动函数、参数操作等——每项都输出带置信度与告警的机器可读结果。

## 作为执行单元（execution cell）运行

`shuttle` 也可以作为受笼约束的执行单元运行。`shuttle cell-run --binding <file>`
依据 `cell_binding/v1` 信封立起一个 fail-closed 的写边界，并在其中驱动一个规划器
——不需要外部编排器。可选的上游编排器可以通过 launch bundle 驱动同一条代码路径；
该集成是可插拔的，位于 `UpstreamOrchestrator` 接缝之后，因此原生构建对编排器零依赖。

## 持久化本地 Goal

`shuttle goal` 提供独立的、单主机的 Delivery Goal 控制面。它在平台状态目录中
存放只追加的 SQLite 日志与内容寻址产物；不要求外部编排器、不要求 Python 门面、不要求
常驻服务。Goal 引用会永久绑定所选后端与存储实例。

创建一个严格的验收契约：

```json
{"schema_version":"goal_acceptance/v1","required_gates":["tests","scope"]}
```

然后创建并检视 Goal：

```bash
shuttle goal start "交付这个变更" \
  --acceptance acceptance.json \
  --repository-identity repo:example/project \
  --driver-id local-shell --json

shuttle goal status goal:local:default:<goal-id> --json
shuttle goal explain goal:local:default:<goal-id> --json
shuttle goal doctor --json
```

`shuttle goal step --bundle action.json` 接受严格的
`shuttle_goal_action_bundle/v1`，在拉起既有受笼 `cell-run` 之前先提交动作意图，
将有界/脱敏后的输出存为内容寻址产物，并且绝不把进程退出本身当作 Goal 完成。
完成要求在同一冻结基准上，每个必备验收门都有 `shuttle goal evaluate` 证据。
每次变更都使用修订号 CAS；控制器操作还要求当前的 fencing epoch。

对于有界的多动作续跑，`shuttle goal run --plan run-plan.json`
接受无授权语义的 `shuttle_goal_run_plan/v1`。受信驱动器在每个动作前重读 Goal 与
附件修订号，并在动作数、token、活跃时长、provider 失败或无进展预算上停下。
run plan 必须恰好以一个独立校验器收尾：它是一个一次性只读 cell，只有在
Rust 门控的 `allowed_commands` 调用成功后才被接受。获准命令使用精确 argv 匹配、
无 shell 运行于一次性工作区快照、收到清洗过的环境，且不能写回源工作区。

## 状态

已超出初始骨架：应用引擎、符号/导航栈、AST 精确重构、codemod、冲突解决、
变更影响分析，以及持久化本地 Delivery Goal（含其有界续跑能力），均已实现并测试。
发布历史见 [CHANGELOG.md](CHANGELOG.md)。

二进制归档与运行时容器通过共享的
[GitHub、Gitea 与 Gitee 发布工作流] 发布。
运行时镜像为多阶段构建，不含 Rust/Cargo 工具链。

## 许可

MIT —— 见 [LICENSE](LICENSE)。

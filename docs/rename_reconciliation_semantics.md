# rename 对账语义(rename-weak-type-safety 收口后)

2026-10-28,story `rename-weak-type-safety`(design.md 在 story 目录)。本文是 rename
子系统的行为契约,供 CLI `--help` 之外的完整口径。

## 计划从哪来(证据层级)

`plan_rename = ctags 定义标签(经 kind 过滤 + AST 消歧) ⊕ references 管线对账`。

- 定义站点:ctags 标签中 kind 为代码类者(`Symbol::is_code_symbol`,排除 JSON
  `string`/`number`/`key`、INI/TOML `section` 等数据标签),再经
  `disambiguate_refs`(tree-sitter 按语言的 `identifier_kinds` 表,排除注释/字符串/
  字段名假匹配)。
- 引用站点:与 `shuttle references` 同一条管线(def 锚点 + `rg` 文本候选 +
  AST identifier 行交叉),逐点带 High/Medium/Low。**默认不进计划**,构成
  `excluded` 缺口并阻断 `--write`;`--include-refs` 显式收编。

## 对账合同(SC-1/SC-2)

dry-run 首行 `planned N / known M sites`;N<M 时逐条列 excluded
(`path:line [conf] snippet`)。`--write` 在缺口非空且未加 `--include-refs` 时
**拒绝**(非零退出,零落盘),错误信息给出精确重跑命令。静默半改名不再可能。

## flag 矩阵

| 组合 | 行为 |
|---|---|
| (默认) | dry-run:对账行 + def 站点 diff + excluded 列表 |
| `--write` | 缺口非空 → 拒绝;缺口为零 → 直接写(def 全有或全无) |
| `--include-refs` | dry-run:计划 = def + 全部引用站点(逐点置信度) |
| `--write --include-refs` | 含 Low(文本档)站点时仍要求 `--force`(§9.5 门) |
| `--write --include-refs --force` | 完整机械改名,一条命令 |
| `--force`(无 `--write`) | **仍是 dry-run**;`--force` 不隐含 `--write` |

## 扫描范围与索引(SC-3)

rename 走**现场 ctags 扫描**(`SymbolTable::scan`),范围 = `--root` 下的
git-tracked 文件;**不读** `.shuttle/symbols.index`(`scan_cached` 是读路径
references/repomap 的事;写路径要当前真相)。`--root` 为仓库严格子目录时
dry-run 打印 scope 警告行:root 外的文件从未被扫描,那里的站点不在本计划
(也不在 `references` 同 root 的已知集)内。

## L1 边界(诚实但不万能)

`--include-refs` 是文本+AST 证据档:跨文件 Medium 站点只证明"这里有一个同名
identifier",不证明它解析到目标符号(同名异符号可能过收)。防线 = 逐点置信度
+ dry-run 预览 + all-or-nothing apply。语义级保证(scope/import/别名/动态分发)
留给 L2 `rename --semantic`(real-lsp,独立 story;shuttle-lsp-bridge
`conversation.rs` 的 one-shot 骨架已备,WorkspaceEdit → EditPlan 同一写通道)。

## 已知残留

- `shuttle rename | head` 在下游提前关管时因 Rust 默认忽略 SIGPIPE 而 panic
  (`println!` broken pipe)——所有子命令共病,非 rename 独有,待统一修。
- 未注册自定义语法(thrift/toml/ini)无 AST 档:其引用不参与对账(与修复前
  行为一致),站点覆盖依赖文本层。

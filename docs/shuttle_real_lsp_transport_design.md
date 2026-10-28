# shuttle · 可选真实 LSP transport（tier-3）— 设计与落地 (N5)

> 状态：**implemented（N5）**。`shuttle-lsp-bridge` 的 tier-3 能力（goto
> type-definition / type-hierarchy）已从 fail-closed scaffold 升级为真实的
> **spawn-query-kill（用完即走）** transport：纯 transport 的
> `conversation::converse()` 在**默认构建**里即可 hermetic 测试；feature-gated
> 的 `real::resolve` 才 spawn 子进程、带 deadline 看门狗驱动一趟对话后 kill+reap。
> 默认构建仍完全 **LSP-free**，`shuttle typedef` 会诚实回报「需要 `--features
> real-lsp` 重建」。承接 （能力总纲）与
> （G9·D1 反向的 shuttle-as-LSP-server）。
>
> **红线**：#11——shuttle 绝不拥有物理路径/凭据/land 权，只做推理与证明；
> §7/§20「绝不瞎猜」——未知即 None/空集，绝不编造一个位置；
> daemonless——按需 spawn、一趟对话即 kill，绝不常驻、绝不监听端口（§8）。

---

## 0. 一句话

shuttle 的静态阶梯（ctags + tree-sitter + ast-grep + 符号表）能覆盖
references/defs/rename/impls/callers/hover，但**类型层**能力（「这个表达式的
*类型* 定义在哪」「这个 trait 的 super/subtypes」）需要一次真实的语义解析——
这正是 language server 存在的理由。N5 把这条 tier-3 能力**隔离**进
`shuttle-lsp-bridge`，behind OFF-by-default `real-lsp` feature：默认构建不 link 任何
language server、冷启动 = 0；只有显式开启并显式请求 `shuttle typedef` 时，才 spawn
一次 rust-analyzer / tsserver / pyright，问一个问题，然后杀掉。

---

## 1. 为什么隔离在 feature flag 后

shuttle 的既有共识（`architecture.md` §7 / `shuttle_lsp_capability_plan.md` §1）：**不要
LSP 的接口/生命周期，只要它的能力**。LSP server 冷启动要索引整个工程、常几秒到
几十秒，与「小快模型 + 低尾延迟」冲突。所以：

- **tier-1/2（静态阶梯）** 永远在默认构建里——进程内、零 spawn、零 daemon。
- **tier-3（类型层）** 是唯一必须外接 language server 的能力，因此：
  - 放进独立 crate `shuttle-lsp-bridge`，作为**隔离边界**；
  - behind `real-lsp` feature，`default = []`——默认构建**不编译** adapter、
    核心 `shuttle` 二进制**不依赖**任何 language server；
  - 生命周期严格 **用完即走**：spawn → 一趟对话 → kill，绝不常驻、绝不监听端口。

这样「有 tier-3 能力可用」是一个**可选升级**，而不是给所有用户强加一个
language-server 依赖与冷启动税。

---

## 2. 分层：纯 transport vs. feature-gated spawn

N5 的核心是把「说 LSP 协议」和「起进程」这两件事**彻底分开**：

| 层 | 位置 | 构建 | 纯度 | 职责 |
|---|---|---|---|---|
| framing | `framing.rs` | 默认 | 纯 | `Content-Length: N\r\n\r\n` + JSON body 的读写；client 与 server 共用一份，永不漂移 |
| conversation | `conversation.rs` | 默认 | 纯（不 spawn、不碰管道） | 在任意 `BufRead`+`Write` 上驱动 `initialize -> initialized -> didOpen -> <op> -> shutdown -> exit` 一趟握手，按 id 关联响应，把 `Location`/`LocationLink` 映射成 `NavHit` |
| real adapter | `lib.rs::real`（`#[cfg(feature = "real-lsp")]`） | 仅 `--features real-lsp` | 不纯（spawn/kill） | 唯一 spawn 子进程的地方：起 server、把它的 stdio 管道递给 `converse`、带 deadline 看门狗、kill+reap |
| CLI shell | `shuttle-cli/src/main.rs::run_typedef` | 默认（能描述 tier，不能执行） | 不纯（canonicalize/print） | 语言/服务器路由、`--root` 绝对化、组 `LspRequest`、诚实降级出口 |

**关键收益**：`converse()` 是纯函数，住在默认构建里，用一个内存里的
`Cursor<Vec<u8>>`（脚本化的 framed 响应）+ 一个 `Vec` 写 sink 就能 hermetic 测试
——不需要任何真实进程。spawn 出来的路径与被测路径调用的是**同一个** `converse`，
只是宿主线程和 kill 触发不同，所以两者只可能在**存活性**上不同，绝不会在**行为**
上漂移。

---

## 3. spawn-query-kill（用完即走）与 deadline 看门狗

`real::resolve` 的一趟生命周期：

1. **PATH presence check**：`<server> --version`，不在 PATH 则
   `BridgeError::ServerMissing`——诚实回报，绝不 spawn 一个不存在的东西。
2. **读目标文件文本**一次（喂给 server 的 `didOpen`）。文件读不出是硬错误
   `BridgeError::Query`，绝不用一个编造的答案糊弄。
3. **spawn** 子进程，`stdin/stdout` piped、`stderr` 置空（多话的 server 不会污染
   我们的 framed channel）。
4. **worker 线程 OWNS 管道**，`Child` handle 留在主线程——只有主线程能 kill 它。
   worker 里跑的就是那个纯 `converse`。这是「有界生命周期」的关键：`converse`
   做**阻塞读、没有内部 deadline**，若内联等待，一个卡住/正在索引的 server 会把
   我们**永远**挂住。
5. **`await_with_deadline` 看门狗仲裁**（从 `resolve` 抽出的纯逻辑，可 hermetic
   测试）：
   - `SHUTTLE_LSP_TIMEOUT_MS == 0`：禁用 deadline，阻塞直到 worker 出结果（仅调试用）。
   - 否则 `recv_timeout(deadline)`：
     - `Ok(res)` → 原样返回 worker 的结果；
     - `Timeout` → 触发注入的 kill 闭包（杀 child，其 stdout 关闭 → worker 的阻塞
       读拿到 EOF → worker 结束），返回诚实的
       `"no answer within {N}ms (killed; set SHUTTLE_LSP_TIMEOUT_MS to adjust)"`——
       **绝不**为一个没回答的 server 编造 hit（§7）；
     - `Disconnected` → 诚实回报 `"worker vanished"`，不吞掉。
   - 默认 deadline = 30_000ms，`SHUTTLE_LSP_TIMEOUT_MS` 可调。
6. **kill + wait + join**：无论走哪条分支，`child.kill(); child.wait();
   worker.join()` 保证调用返回后没有任何进程或线程残留。

看门狗的 kill 触发用**闭包注入**，所以「server 没回答 → kill → 诚实报错」这个决策
可以用一个内存 channel 在测试里精确演练，永不需要真实进程（见 §5）。

---

## 4. §7「绝不瞎猜」的降级矩阵

| 情形 | conversation / bridge 行为 | `shuttle typedef` 出口 |
|---|---|---|
| 空 / null 结果 | 零 hit（`Ok(vec![])`） | 诚实「no result…shuttle never fabricates a position」，exit 0 |
| JSON-RPC error 对象 | `BridgeError::Query(server 消息)` | 把 server 消息透出，anyhow error |
| location 的 uri 非 `file://` | 丢弃该 location，不回报 | 不出现在 hits 里 |
| `real-lsp` 未编译 | `BridgeError::Disabled` | 诚实「compiled LSP-free…rebuild with --features real-lsp」，exit 0 |
| server 不在 PATH | `BridgeError::ServerMissing` | 诚实「not found on PATH…install it」，exit 0 |
| 未知文件扩展名 | —（CLI 层拦截） | 诚实「no language could be determined」，exit 0 |
| 该语言无 tier-3 server | —（CLI 层拦截） | 诚实「language `X` has no tier-3 external server」，exit 0 |
| 超时 | `await_with_deadline` kill + `Query` | anyhow error，进程已被杀 |

每条**回来的** hit 都已被 bridge 打上「via external LSP」标签、带
`Backend::Lsp` + `Confidence::High`，planner 永不会把它和 shuttle 的静态阶梯混淆。

---

## 5. 测试策略（hermetic，无真实进程）

N5 的测试**全部** hermetic，不依赖任何已安装的 language server：

- **`conversation.rs`（10+ 测试）**：用脚本化 `Cursor` 响应 + `Vec` 写 sink 驱动
  纯 `converse`：单 Location→单 NavHit、LocationLink 形态、空结果零 hit（不编造）、
  JSON-RPC error→Query、well-formed 关联请求序列、type-hierarchy 用 prepare 方法、
  等待中的 server 请求被 ack 后跳过、op 响应前 EOF→Query、非 file:// uri 丢弃、
  `uri_to_rel_path` 剥 root 并拒绝非 file。
- **`framing.rs`（3 测试）**：round-trip、忽略 Content-Type、空 reader EOF。
- **`lib.rs::real::watchdog_tests`（5 测试，`#[cfg(feature = "real-lsp")]`）**：用
  内存 `mpsc` channel 演练 `await_with_deadline`——
  1. 按时回答 → 原样透传、**绝不**触发 kill；
  2. 超时沉默 → kill 恰好一次 + 诚实「no answer within Nms (killed)」；
  3. deadline=0 → 阻塞直到回答、不 kill；
  4. worker 消失（sender drop）→ 诚实「worker vanished」，非超时 kill；
  5. deadline=0 下 worker 消失 → 同样诚实回报。

因为 spawn 路径与测试路径共用同一个 `converse` 和同一个 `await_with_deadline`，
「被测的」与「真跑的」不会行为漂移；真实 rust-analyzer 的 live E2E 仅作人工冒烟
（对 u32 primitive 返回空结果、8s/90s deadline 均不挂、exit 0），不进 CI。

---

## 6. `shuttle typedef` CLI 契约

```
shuttle typedef --path <FILE> --line <N> --col <N> [--symbol <S>] \
           [--type-hierarchy] [--root <DIR>] [--json]
```

- **语言路由**纯看扩展名（`shuttle_symbols::lang_for_path`），不看内容——未知扩展名
  降级为「no tier-3 server」而非猜测。
- **`--root` 绝对化**：`std::fs::canonicalize` + cwd-join 兜底，保证 bridge 发出
  合法的 `file://<absolute>` document URI（真实 server 拒绝相对 file URL）。
  canonicalize 是不纯步骤，故留在 CLI shell，transport 保持纯。
- **`--json`** 出口固定 keys：`op` / `path` / `language` / `server` /
  `available`（= `real_lsp_enabled()`）/ `hits` / `note`。
- **默认（非 json）** 出口：每个 hit 打印 `path:line` + snippet；无结果 / 降级走
  stderr 诚实说明，exit 0。

---

## 7. 与其它设计的关系

-  — 能力总纲。N5 是它 §9.9 tier-3「real LSP on
  demand」的落地；banner 已交叉引用本文。
-  — G9·D1 是**反向**：shuttle 自己作为 LSP
  *server*（stdio、按需退）把 impact/callers confidence 喂进编辑器 hover。N5 是
  shuttle 作为 LSP *client* 外接真实 server 拿类型层答案。两者共用 `framing.rs`。
- 红线 #11：tier-3 只**读**语义、只**回报**位置与类型，绝不拥有物理路径/land 权、
  绝不 push；一切写盘/落地仍由上层（external / agent）决定。

---

## 8. 未来（非 N5 范围）

- 外部 SCIP 索引 spike（`shuttle_scip_incremental_spike.md`）作为 tier-3 的**离线**
  替代：预生成 SCIP、查询时零 spawn。
- 看门狗遥测：把「超时 kill」「server missing」计数投影进 telemetry，量化 tier-3
  的可用率与尾延迟。
- 多问题 batching：一次 spawn 内问多个位置（仍用完即走），摊薄冷启动。

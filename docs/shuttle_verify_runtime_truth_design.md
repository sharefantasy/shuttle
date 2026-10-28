# G5 · 执行真相层 — `shuttle verify` land 时刻真验证（设计 v0.1）

> 状态：**implemented（G5，`4e05a1ed`）**。`shuttle verify`、`VerifyRunner` /
> `CellVerifyRunner`、`VerifyTelemetry` 与 `crates/shuttle-cli/tests/verify_cli.rs` 已落地；
> green、build-blocked、test-blocked、needs-run、partial、stale-index 等降级路径均有
> 机器校验。下文保留为设计与验收基线。承接 
> 轴二、 的 T2（`reverify_impact`
> 增量重验集）与 `cell-run` 权限笼。
> 红线：**#11 shuttle 不持 land 权、不碰凭据**（verify 的「跑」在 cell-run 笼内、由
> 上层授权）；**§7「绝不瞎猜」——跑不起来 / 环境缺失 = `needs-run`，绝不把
> 「没跑」报成「通过」；图标 UNKNOWN 的边 → verify 只能给 `partial`，绝不冒充
> full green。**

---

## 0. 一句话

把 T2 已经算出的**最小重验集**从「一份纸面清单」升级为「**真的编译 + 真的跑，
再把 green/blocked 作为证据回填 confidence**」的执行闭环。静态图声称「改动只影响
这 N 个编译单元」，`shuttle verify` 在隔离笼里**证明**这 N 个的测试全绿、且改动没有
越出这 N 个之外。

---

## 1. 缺口与价值

现状：`shuttle impact --base <oid> --head <oid>` 出 `ReverifyDetail { reverify:
Vec<ReverifyUnit>, reverify_size, degraded, .. }`，但**没有任何东西真的去跑它**。
confidence 全部来自静态推断，没有运行时反证。

价值三连：
1. **把静态断言变成可证事实**：从「应该只影响 X」→「已验证 X 全绿且无外溢」。
2. **回填 confidence**：跑绿的边升 High，跑红/跑不动的边据实降级——让后续
   split/goal 的裁决建立在**运行时证据**之上，而非纯结构猜测。
3. **解锁 CI 交付**（G9 前身）：PR 上 `shuttle verify --base origin/main --head HEAD`
   **只编译 + 只跑受影响测试**，省钱、可证、无需全量。

---

## 2. 命令面

```
shuttle verify --base <oid> --head <oid> [--run] [--telemetry] [--dry-run]
```

- 默认（不加 `--run`）：**只算不跑**，退化为 `shuttle impact --base/--head` 的别名，
  输出重验集清单（向后兼容，零行为变化）。
- `--run`：进入执行真相模式，对重验集里的**测试单元**在 `cell-run` 笼里编译 + 跑。
- `--dry-run`：走执行状态机但注入零副作用 runner（仿 goal 的 `--dry-run` 缝），
  用于 hermetic 测试断言编排而非真跑。
- `--telemetry`：向 **stderr** 打一行 `VerifyTelemetry` JSON，stdout 恒为
  `ToolOutcome` 机器信封（house 约定，对齐 `shuttle run` / `shuttle goal`）。

---

## 3. 架构落点

### 3.1 纯执行状态机 `drive_verify`（`shuttle-nav::verify`，仿 `drive_goal`）

四道注入缝，与 `orchestrate.rs` / `goal.rs` 同范式（状态机 + 注入 runner）：

```
select   : 复用 T2 reverify_impact → ReverifyDetail（重验集 + degraded 水位）
partition: 把 reverify 单元切成「测试单元」与「非测试单元」（复用 test_file 启发式）
run      : 对每个测试单元在 cell-run 笼里 build + test（注入 VerifyRunner trait）
fold     : 把每单元 pass/fail/skip 折叠成 VerifyDetail，再降维到 ToolOutcome
```

`VerifyRunner` trait（可注入，测试用 dry-run 实现）：

```rust
trait VerifyRunner {
    fn build(&self, unit: &ReverifyUnit, cage: &CellBinding) -> BuildResult;
    fn test (&self, unit: &ReverifyUnit, cage: &CellBinding) -> TestResult;
}
```

真实实现：`CellVerifyRunner` —— 经既有 `run_cell` / `run_cell_from_external`
（`shuttle-cli::cell_run`）在权限笼里 spawn 语言原生 build/test 命令（由 cell binding
的 `allowed_commands` 授权，shuttle 不自己决定跑什么，只跑笼子允许的）。

### 3.2 两级信封（区分「编不过」与「测试红」）

- `build-blocked`：改动本身编译失败 → 携带失败编译单元 + stderr 摘要。
- `test-blocked`：编译通过但测试红 → 携带失败测试 + 首个失败断言。
- `needs-run`：环境缺失 / 命令不在 cell binding allowlist / 笼外副作用 → **不判绿**。
- `partial`：重验集里含图标 UNKNOWN 的边（如 A5 动态导入）→ 跑绿的部分给绿，
  UNKNOWN 部分显式标 `partial`，**绝不把 partial 冒充 full green**。

### 3.3 协议层 `VerifyDetail`（`shuttle-protocol::tool_outcome`，新 TypedDetail 变体）

```rust
pub struct VerifyDetail {
    pub base_oid: String,
    pub head_oid: String,
    pub reverify_size: usize,       // 来自 T2，纸面应验数
    pub ran: usize,                 // 真跑了几个
    pub passed: usize,
    pub failed: Vec<FailedUnit>,    // {unit, kind: build|test, evidence}
    pub skipped: Vec<SkippedUnit>,  // {unit, reason: no-test|not-allowed|env-missing}
    pub partial: bool,              // 重验集含 UNKNOWN 边
    pub degraded: bool,             // 透传 ReverifyDetail.degraded（索引陈旧水位）
}
```

降维规则（`verify_outcome`）：
- 全 build+test 通过、无 skipped-because-blocked、`!partial` → **Green**。
- 有 UNKNOWN 边但已跑部分全绿 → **Green_low**（`partial=true`，summary 标不可知面）。
- 任一 build 失败 → **Blocked `build-blocked`**。
- 任一 test 失败 → **Blocked `test-blocked`**。
- 有单元因 env/allowlist 无法跑 → **Blocked `needs-run`**（绝不因「没跑」判绿）。
- `degraded`（索引陈旧）→ 复用 T2 语义 **Blocked `stale-index`**。

---

## 4. §7 降级矩阵（每条必须有专门测试断言）

| 情形 | 处置 | 绝不做 |
|---|---|---|
| 测试命令不在 cell binding allowlist | `needs-run` | 不自行 spawn 任意命令 |
| 依赖网络/时钟/全局态（笼外副作用） | `needs-run` | 不把不 hermetic 的绿判成绿 |
| 编译失败 | `build-blocked` + 证据 | 不把编不过说成测试通过 |
| 重验集含 UNKNOWN 边（A5） | `partial` / Green_low | 不冒充 full green |
| 索引陈旧（T2 水位） | `stale-index` | 不基于陈旧图跑出「绿」 |
| 空 diff | honest Green（复用 T2 短路） | —— |

---

## 5. CI 交付形态（G9 前身，本里程碑顺带兑现）

```
shuttle verify --base origin/main --head HEAD --run
```
- PR 上只编译 + 只跑受影响测试；退出码恒 0（机器结果是契约），CI wrapper 读
  `ToolOutcome.state` 决定 pass/fail。
- 省钱可证：全量测试 O(all) → 受影响测试 O(blast-radius)。
- 与 GitHub Actions / 内部 CI 的对接留给 G9；G5 只保证 CLI 出口稳定可脚本化。

---

## 6. 落地顺序（一口气做到底）

1. `VerifyDetail` + `verify_outcome` 降维（`shuttle-protocol`，含单测）。
2. `VerifyRunner` trait + `drive_verify` 状态机（`shuttle-nav::verify`，注入 dry-run 单测）。
3. `CellVerifyRunner`（经 `run_cell` 笼内 build/test）。
4. CLI `shuttle verify --base/--head --run/--dry-run/--telemetry`（`shuttle-cli`）。
5. `VerifyTelemetry`（`shuttle-protocol::telemetry`，纯投影：ran/passed/failed/partial rate）。
6. hermetic 集成测试 `crates/shuttle-cli/tests/verify_cli.rs`（仿 `goal_cli.rs`）：
   绿灯全跑通、build-blocked、test-blocked、needs-run(allowlist 拒)、partial(UNKNOWN 边)、
   stale-index、空 diff 短路、telemetry 开关两态。
7. 全量门禁 + commit（不 push）。

---

## 7. 不做

- 不在 shuttle 内决定跑什么命令 —— 只跑 cell binding allowlist 授权的。
- 不做全量测试编排（那是 CI 的事）；verify 只管**受影响子集**。
- 不缓存测试结果跨 commit（留给 G7 增量索引）；G5 每次真跑。
- 不监听端口、不常驻。

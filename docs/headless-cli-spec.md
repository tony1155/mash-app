# mash-cli 最小接口规范（Headless Machine Interface Spec）

版本：v1（2026-10-24）。目标消费者：OrchestratoRRR `MashAdapter`（见 [tony1155/OrchestratoRRR 分支 feat/fgo-mash-workflow](https://github.com/tony1155/OrchestratoRRR/tree/feat/fgo-mash-workflow)）；验收指引见该分支 `docs/fgo-acceptance-guide.md`。

设计原则：

1. **四个机器接口**：`validate` / `run` / `cancel` / `status`（外加辅助 `devices`、`projects`）。
2. **stdout 只输出结构化数据**（NDJSON，一行一事件 + 末行 result）；人类可读日志全走 stderr。
3. **结果双通道**：进程 exit code 给粗粒度判断，`--result-file` JSON 给细粒度裁决。
4. **不驱动 GUI**：headless 进程与 GUI 共享 `app_data_dir` 配置，但互不启动。
5. 语义与 Mash 内部 API 一一对应：`run`→`start_automation`，`cancel`→`stop_automation(_after_current)`，`status`→`get_automation_status`。

---

## 1. 命令总览

```
mash-cli validate  [--project <id>] [--json]
mash-cli projects  [--json]
mash-cli devices   [--json]
mash-cli run       --project <id> [--device <serial>] [--server cn|jp]
                   [--repeat infinite|count:<N>] [--ap-items gold:2,silver:*]
                   [--result-file <path>] [--events ndjson|none]
                   [--watchdog <seconds>] [--json]
mash-cli cancel    [--after-current] [--wait <seconds>]
mash-cli status    [--json]
```

通用环境变量：

| 变量 | 作用 | 默认 |
|---|---|---|
| `MASH_RESOURCE_DIR` | release 构建下替代 `resource_dir()`（Mash 安装目录） | 不设 → dev 构建用源码树回退，release 报 validate 错误 |
| `MASH_APP_DATA_DIR` | 覆盖 app data 目录（隔离测试用；等价 `--app-data-dir`） | `%APPDATA%\com.xiaotongx.mash` |

## 2. `validate` —— 前置条件自检（不碰设备、不启动 sidecar）

检查项（全部写入报告）：

| 检查 | 判定 |
|---|---|
| `appDataDir` | 目录可建/可写 |
| `settings` | adb/server/recognition/app_ui 各 JSON 存在或可默认 |
| `project` | 指定（或 active）项目存在、battle_scenes 可读 |
| `cvRuntime` | 已安装且版本 == manifest 要求；exe/模型目录存在 |
| `resources` | 模板目录 / cv.json / scrcpy-server.jar / 内置 CV 代码可解析 |
| `servantAssets` | `assets/catalogs/servants.json` 有效（sidecar 启动硬依赖） |
| `adb` | adb 二进制可解析（bundle 或 PATH） |

- `--json`：输出 `{ "ok": bool, "checks": [{ "id": "cvRuntime", "ok": true, "detail": "…" }], "appDataDir": "…", "resourceDir": "…" }`。
- exit code：全部通过 0；任一失败 1。
- 用途：OrchestratoRRR `VALIDATE_CONFIG` 阶段；也用于安装排障。

## 3. `run` —— 执行一次自动化并等待终态

流程：构建 mock AppHandle（identifier 同 GUI）→ 复刻 GUI 的 managed state → 按 `projectRunConfig.ts` 语义把 Project 组装成 `RunConfig` → `start_automation_with_config` → 监听事件流 → 等待 `RunnerState` 进入终态（Finished / Error / 被停止后的 Idle）→ 写 result → 退出。

参数：

| 参数 | 语义 |
|---|---|
| `--project <id>` | 必填。projects.json 中的项目 id（`projects` 命令可列） |
| `--device <serial>` | 覆盖本次设备（内存态覆盖 `selected_adb_serial`，不写回 adb_settings.json）。缺省用持久化选择 |
| `--server cn\|jp` | 覆盖本次服务器（内存态，不写回）。缺省用 server_settings.json |
| `--repeat infinite` | `repeatMission=true, maxMissionRuns=null`（无限周回） |
| `--repeat count:<N>` | `repeatMission=false, maxMissionRuns=N` |
| 缺省 `--repeat` | 用项目保存的默认（等价 GUI 直接点开始：`repeatMode=count` → 项目内保存的次数） |
| `--ap-items gold:2,silver:*` | 覆盖体力恢复物品与限额（`*`=不限）。缺省用项目默认 |
| `--result-file <path>` | 终态 JSON 落盘路径（原子写）。缺省 `<appDataDir>/cli/runs/<ts>.json` 并更新 `last-run.json` |
| `--events ndjson\|none` | stdout 是否输出事件流（默认 ndjson） |
| `--watchdog <seconds>` | 硬超时：到点先触发 graceful cancel，宽限 120s 后强退并标 `timeout`。缺省不启用（Orchestrator 自管超时时的逃生门） |
| `--json` | 启动横幅也走 JSON（便于管道首行解析） |

事件流（stdout，每行一个 JSON）：

```jsonc
{"type":"run-started","runId":"…","ts":"…","project":{"id":"…","name":"…"},"device":"127.0.0.1:16384","server":"CN","repeat":{"mode":"count","count":3}}
{"type":"status","ts":"…","state":"Running","screen":"Battle","message":"点击攻击按钮","level":"info"}     // ← automation-status 全量转发
{"type":"progress","ts":"…","completedRuns":1,"maxRuns":3,"apRecoveryUsage":{"rainbow":0,"gold":0,"silver":0,"bronze":0,"copper":0}}
{"type":"result", …见下…}
```

`run-result.json`（与 `result` 事件同构）：

```jsonc
{
  "schemaVersion": 1,
  "runId": "6f1c…",
  "startedAt": "2026-10-24T01:00:00Z",
  "endedAt": "2026-10-24T01:21:33Z",
  "outcome": "finished",            // finished | error | cancelled | startup-failed | timeout
  "runnerState": "Finished",        // Finished | Error{message} | Idle | Starting
  "message": null,                  // error/cancel 时的人类可读原因
  "project": {"id":"p1","name":"周回A","advancedMode":false},
  "server": "CN",
  "device": "127.0.0.1:16384",
  "repeat": {"mode":"count","count":3},
  "completedRuns": 3,
  "apRecoveryUsage": {"rainbow":0,"gold":1,"silver":0,"bronze":0,"copper":0},
  "eventCounts": {"status":412,"warn":9,"error":0},
  "exitCode": 0
}
```

Exit codes（Orchestrator 判据）：

| code | 含义 |
|---|---|
| 0 | `Finished`——按配置完成（轮数达标/触发停止条件/停止后当前局收尾） |
| 1 | `Error` / `startup-failed`——启动或运行失败，`message` 带原因 |
| 2 | `cancelled`——外部 cancel，未完成但干净退出 |
| 3 | `timeout`——watchdog 强制结束（设备可能仍在战斗中，Orchestrator 应安排恢复动作） |

## 4. `cancel` —— 优雅取消正在运行的 run

- 机制：`run` 启动时写 `<appDataDir>/cli/current.json`（pid、runId、project、startedAt）并每 300ms 轮询 `cli/cancel.request`。
- `mash-cli cancel`：写 `cancel.request` = `{"mode":"immediate"}`，等目标进程退出（`--wait <secs>`，默认 30）。
- `mash-cli cancel --after-current`：`{"mode":"after-current"}` → 对应 `stop_automation_after_current`（当前局打完再停，终态仍算 `finished`，completedRuns 为实际值）。
- Ctrl+C（SIGINT/CTRL_C_EVENT）等价 immediate cancel。
- 无运行中进程时 exit 1 并说明。

## 5. `status` —— 查询当前/最近一次 run

- 读 `cli/current.json` 并对记录的 pid 探活（活着 → `{"running":true,"current":{…}}`）+ `cli/last-run.json`。
- `{"running":false,"stale":false,"current":null,"lastRun":{…run-result…}}`。
- `stale=true` 表示发现记录但 pid 已不存在（硬杀/崩溃残留）：`current` 为 null，记录已自动清理。
- Orchestrator 主要在异常恢复时用它做对账（例如 Orchestrator 崩溃重启后确认 Mash 是否还在跑）。

## 6. 辅助命令

- `projects [--json]`：列 `{id,name,advancedMode,slots 摘要,repeat 默认}`。
- `devices [--json]`：`adb devices -l` 解析为 `[{serial,status,description}]`（OrchestratoRRR `WAIT_MUMU_ADB_READY` 之后可复核）。

## 7. 与 OrchestratoRRR 的映射（详见 03 文档）

| Orchestrator 概念 | mash-cli |
|---|---|
| `AutomationRuntime::validate` | `validate --project <id>` |
| `AutomationRuntime::run` | `run --project <id> --device <serial> --result-file <path> --events ndjson`（长驻进程；Orchestrator v1 以退出码 + result-file 为裁决依据，stdout 事件流供人工排查与未来的实时监控，v1 不做流式解析） |
| `AutomationRuntime::cancel` | `cancel [--after-current]` |
| `AutomationRuntime::status` | `status` / run 进程存活探测 + result-file |

## 8. 非目标（v1 不做）

- 不做多 run 并发（单设备单 run，pidfile 互斥）。
- 不提供强化/友情点/强化任务等其他 runner 的 CLI 包装（内部 API 形状相同，需要时按同一模式扩展）。
- 不做远程/HTTP 模式——Orchestrator 与 Mash 同机，进程级接口足够。

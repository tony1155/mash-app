//! Headless CLI entry point for external orchestrators.
//!
//! `mash-cli` exposes the automation runtime through a machine-facing,
//! process-level interface (validate / projects / devices / run / cancel /
//! status) so an orchestrator such as OrchestratoRRR can drive FGO battles
//! without touching the GUI. It boots the *real* Tauri app runtime with an
//! empty window list — the same `mash_lib` automation code, settings
//! persistence, and CV sidecar lifecycle as the desktop app run.
//!
//! Contract highlights (see docs/02-mash-cli-spec.md in the integration
//! repo for the full spec):
//! - stdout carries NDJSON events only; human-readable logs go to stderr.
//! - `run` maps terminal runner states to exit codes:
//!   0 finished, 1 error/startup-failed, 2 cancelled, 3 watchdog timeout.
//! - Env overrides: `MASH_APP_DATA_DIR`, `MASH_RESOURCE_DIR`.

use crate::adb;
use crate::commands::automation::start_automation_with_config;
use crate::commands::debug;
use crate::commands::emulator;
use crate::commands::projects as projects_commands;
use crate::commands::runtime as runtime_commands;
use crate::commands::settings;
use crate::device_operation::DeviceOperationCoordinator;
use crate::models::Project;
use crate::paths;
use crate::platform::MuMuLookupState;
use crate::recognition_diagnostics::{DiagnosticLogs, DiagnosticStore};
use crate::runner::{ApRecoveryItem, ApRecoveryLimits, RunConfig, RunnerHandle, RunnerState};
use crate::server::Server;
use crate::storage::write_json_atomic;
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};
use tauri::{Listener, Manager};

/// Must match `tauri.conf.json` so the CLI shares the GUI's app data dir.
const MASH_IDENTIFIER: &str = "com.xiaotongx.mash";
const MASH_PRODUCT_NAME: &str = "Mash";

const USAGE: &str = "mash-cli — headless automation interface

用法:
  mash-cli validate  [--project <id>] [--json]
  mash-cli projects  [--json]
  mash-cli devices   [--json]
  mash-cli run       --project <id> [--device <serial>] [--server cn|jp]
                     [--repeat infinite|count:<N>] [--ap-items gold:2,silver:*]
                     [--result-file <path>] [--events ndjson|none]
                     [--watchdog <seconds>] [--json]
  mash-cli cancel    [--after-current] [--wait <seconds>]
  mash-cli status    [--json]

通用:
  --app-data-dir <path>   覆盖 app data 目录（等价 MASH_APP_DATA_DIR）
  --json                  结构化输出

环境变量:
  MASH_APP_DATA_DIR       覆盖 app data 目录（默认 %APPDATA%\\com.xiaotongx.mash）
  MASH_RESOURCE_DIR       覆盖资源根目录（Mash 安装目录，含 resources/）
";

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CANCELLED: i32 = 2;
const EXIT_TIMEOUT: i32 = 3;
const EXIT_USAGE: i32 = 64;

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

pub fn cli_main() -> i32 {
    confine_to_kill_on_close_job();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return EXIT_USAGE;
    };
    let parsed = match Args::parse(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("参数错误: {message}\n");
            eprint!("{USAGE}");
            return EXIT_USAGE;
        }
    };
    // --app-data-dir must take effect before any path resolution happens.
    if let Some(dir) = parsed.value("app-data-dir") {
        std::env::set_var("MASH_APP_DATA_DIR", dir);
    }

    match command.as_str() {
        "validate" => with_headless_app(|app| command_validate(&app, &parsed)),
        "projects" => with_headless_app(|app| command_projects(&app, &parsed)),
        "devices" => with_headless_app(|app| command_devices(&app, &parsed)),
        "run" => with_headless_app(|app| command_run(&app, &parsed)),
        "cancel" => command_cancel(&parsed),
        "status" => command_status(&parsed),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            EXIT_OK
        }
        other => {
            eprintln!("未知命令: {other}\n");
            eprint!("{USAGE}");
            EXIT_USAGE
        }
    }
}

fn with_headless_app(f: impl FnOnce(tauri::App<tauri::Wry>) -> i32) -> i32 {
    match build_headless_app() {
        Ok(app) => f(app),
        Err(message) => {
            eprintln!("初始化 headless 运行时失败: {message}");
            EXIT_ERROR
        }
    }
}

// ---------------------------------------------------------------------------
// Headless app bootstrap (mirrors the state setup in lib.rs run())
// ---------------------------------------------------------------------------

fn build_headless_app() -> Result<tauri::App<tauri::Wry>, String> {
    let mut context: tauri::Context<tauri::Wry> =
        tauri::test::mock_context(tauri::test::noop_assets());
    {
        let config = context.config_mut();
        config.identifier = MASH_IDENTIFIER.into();
        config.product_name = Some(MASH_PRODUCT_NAME.into());
        config.version = Some(env!("CARGO_PKG_VERSION").into());
        // The mock context has an empty window list: the app builds and its
        // handle behaves like the GUI's, but no window is ever created.
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .build(context)
        .map_err(|error| format!("{error}"))?;

    let handle = app.handle();
    let adb_device_settings = settings::load_adb_device_settings(handle)?;
    let emulator_settings = emulator::load_emulator_settings(handle)?;
    let server = settings::load_server_setting(handle)?;
    let recognition_settings = settings::load_recognition_settings(handle)?;
    let debug_settings = settings::load_debug_settings(handle)?;
    let app_ui_settings = projects_commands::read_app_ui_settings_from_path(
        &paths::app_ui_settings_path(handle),
    )?;

    app.manage(Mutex::new(adb_device_settings));
    app.manage(emulator::EmulatorSelectionState::new(emulator_settings));
    app.manage(Mutex::new(server));
    app.manage(MuMuLookupState::default());
    app.manage(Mutex::new(recognition_settings));
    app.manage(Mutex::new(debug_settings));
    app.manage(Mutex::new(app_ui_settings));
    app.manage(Mutex::new(RunnerHandle::new_idle()));
    app.manage(DeviceOperationCoordinator::default());
    app.manage(DiagnosticLogs::default());
    app.manage(DiagnosticStore::default());
    app.manage(debug::DebugSidecar::new());
    Ok(app)
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct Args {
    flags: Vec<String>,
    values: Vec<(String, String)>,
}

impl Args {
    fn parse(tokens: &[String]) -> Result<Self, String> {
        let mut flags = Vec::new();
        let mut values = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            let Some(name) = token.strip_prefix("--") else {
                return Err(format!("无法识别的参数「{token}」"));
            };
            let takes_value = !matches!(
                name,
                "json" | "after-current" | "help" | "strict" | "prune"
            );
            if takes_value {
                let Some(value) = tokens.get(i + 1) else {
                    return Err(format!("--{name} 需要一个值"));
                };
                if value.starts_with("--") {
                    return Err(format!("--{name} 需要一个值，但得到「{value}」"));
                }
                values.push((name.to_string(), value.clone()));
                i += 2;
            } else {
                flags.push(name.to_string());
                i += 1;
            }
        }
        Ok(Self { flags, values })
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn require_value(&self, name: &str) -> Result<String, String> {
        self.value(name)
            .map(str::to_string)
            .ok_or_else(|| format!("缺少必填参数 --{name}"))
    }
}

// ---------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------

fn command_projects(app: &tauri::App<tauri::Wry>, args: &Args) -> i32 {
    let handle = app.handle();
    let projects = match projects_commands::read_projects(handle) {
        Ok(projects) => projects,
        Err(message) => return report_simple_error(&format!("读取项目失败: {message}"), args),
    };
    let active_project_id = app
        .state::<Mutex<paths::AppUiSettings>>()
        .lock()
        .unwrap()
        .active_project_id
        .clone();
    let mut entries: Vec<Value> = projects
        .iter()
        .map(|project| {
            json!({
                "id": project.id,
                "name": project.name,
                "advancedMode": project.advanced_mode,
                "active": active_project_id.as_deref() == Some(project.id.as_str()),
                "repeatMode": project.repeat_mode,
                "repeatCount": project.repeat_count,
                "slots": project.slots.iter().map(|slot| json!({
                    "id": slot.id,
                    "type": slot.kind,
                    "servantId": slot.servant_id,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        let active = |value: &Value| value["active"].as_bool() == Some(true);
        active(b).cmp(&active(a))
    });
    print_json(&json!({ "projects": entries }), args);
    EXIT_OK
}

// ---------------------------------------------------------------------------
// devices
// ---------------------------------------------------------------------------

fn command_devices(app: &tauri::App<tauri::Wry>, args: &Args) -> i32 {
    let handle = app.handle();
    let adb_path = adb::resolve_adb_path(handle);
    let output = match adb::adb_command(&adb_path).args(["devices", "-l"]).output() {
        Ok(output) => output,
        Err(error) => {
            return report_simple_error(
                &format!("无法运行 adb（{}）: {error}", adb_path.display()),
                args,
            )
        }
    };
    if !output.status.success() {
        return report_simple_error("adb devices 执行失败", args);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let devices: Vec<Value> = adb::Adb::parse_devices(&stdout)
        .into_iter()
        .map(|device| {
            json!({
                "serial": device.serial,
                "status": device.status,
                "description": device.description,
            })
        })
        .collect();
    print_json(&json!({ "devices": devices }), args);
    EXIT_OK
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

fn command_validate(app: &tauri::App<tauri::Wry>, args: &Args) -> i32 {
    let handle = app.handle();
    let mut checks: Vec<(&str, bool, String)> = Vec::new();

    let data_dir = paths::app_data_dir(handle);
    let writable = std::fs::File::create(data_dir.join(".cli-write-probe")).is_ok();
    let _ = std::fs::remove_file(data_dir.join(".cli-write-probe"));
    checks.push((
        "appDataDir",
        writable,
        data_dir.to_string_lossy().into_owned(),
    ));

    let settings_ok = settings::load_adb_device_settings(handle).is_ok()
        && settings::load_server_setting(handle).is_ok()
        && settings::load_recognition_settings(handle).is_ok()
        && settings::load_debug_settings(handle).is_ok();
    checks.push((
        "settings",
        settings_ok,
        if settings_ok { "各设置文件可读" } else { "设置文件损坏" }.into(),
    ));

    let server = settings::load_server_setting(handle).unwrap_or_default();
    let requested_project = args.value("project").map(str::to_string);
    let active_project_id = app
        .state::<Mutex<paths::AppUiSettings>>()
        .lock()
        .unwrap()
        .active_project_id
        .clone();
    let project_id = requested_project.or(active_project_id);
    let mut project_ok = true;
    let mut project_detail = String::from("未指定项目，跳过检查");
    if let Some(project_id) = &project_id {
        match projects_commands::read_projects(handle) {
            Ok(projects) => match projects.iter().find(|p| &p.id == project_id) {
                Some(project) => {
                    let scenes_ok = if project.advanced_mode {
                        projects_commands::load_advanced_battle_scenes(
                            handle.clone(),
                            project.id.clone(),
                        )
                        .is_ok()
                    } else {
                        projects_commands::load_battle_scenes(handle.clone(), project.id.clone())
                            .is_ok()
                    };
                    project_ok = scenes_ok;
                    project_detail = if scenes_ok {
                        format!("项目「{}」战斗配置可读", project.name)
                    } else {
                        format!("项目「{}」战斗配置读取失败", project.name)
                    };
                }
                None => {
                    project_ok = false;
                    project_detail = format!("找不到项目 {project_id}");
                }
            },
            Err(message) => {
                project_ok = false;
                project_detail = format!("读取项目失败: {message}");
            }
        }
    }
    checks.push(("project", project_ok, project_detail));

    let runtime_status = runtime_commands::runtime_status_for_app(handle);
    let cv_ok = runtime_status.as_ref().map(|s| s.installed).unwrap_or(false);
    let cv_detail = match &runtime_status {
        Ok(status) => format!(
            "需要 {}，已安装 {}",
            status.required_runtime_version,
            status.installed_runtime_version.as_deref().unwrap_or("无")
        ),
        Err(message) => message.clone(),
    };
    checks.push(("cvRuntime", cv_ok, cv_detail));

    let resource_dir = crate::resource_base_dir(handle);
    let jar_ok = runtime_commands::resolve_scrcpy_jar(handle)
        .map(|jar| jar.is_file())
        .unwrap_or(false);
    checks.push((
        "scrcpyJar",
        jar_ok,
        match runtime_commands::resolve_scrcpy_jar(handle) {
            Some(jar) => jar.to_string_lossy().into_owned(),
            None => "无法解析（release 需设置 MASH_RESOURCE_DIR）".into(),
        },
    ));

    let template_dirs = runtime_commands::resolve_template_dirs(handle, server);
    let templates_ok = !template_dirs.is_empty()
        && template_dirs.iter().all(|spec| spec.dir.is_dir());
    checks.push((
        "templates",
        templates_ok,
        format!(
            "{} 个模板目录（server={server}）",
            template_dirs
                .iter()
                .map(|spec| spec.dir.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ));

    let cv_configs = runtime_commands::resolve_cv_config_paths(handle, server);
    let configs_ok = !cv_configs.is_empty() && cv_configs.iter().all(|path| path.is_file());
    checks.push((
        "cvConfig",
        configs_ok,
        format!("{} 个 cv.json", cv_configs.len()),
    ));

    let sidecar_code_ok = runtime_commands::resolve_sidecar_code_dir(handle)
        .map(|dir| dir.join("mash_cv").is_dir())
        .unwrap_or(false);
    checks.push((
        "sidecarCode",
        sidecar_code_ok,
        match runtime_commands::resolve_sidecar_code_dir(handle) {
            Some(dir) => dir.to_string_lossy().into_owned(),
            None => "未找到内置 CV 代码".into(),
        },
    ));

    let servants_ok = runtime_commands::resolve_servants_json_path(handle).is_some();
    checks.push((
        "servantAssets",
        servants_ok,
        if servants_ok {
            "从者元数据有效".into()
        } else {
            "缺失或无效（需在 GUI 下载资源包）".into()
        },
    ));

    let adb_path = adb::resolve_adb_path(handle);
    let adb_ok = if adb_path.is_file() {
        true
    } else {
        // Bare executable name: probe PATH.
        adb::adb_command(&adb_path).arg("version").output().is_ok()
    };
    checks.push((
        "adb",
        adb_ok,
        adb_path.to_string_lossy().into_owned(),
    ));

    let all_ok = checks.iter().all(|(_, ok, _)| *ok);
    if args.flag("json") {
        let checks_json: Vec<Value> = checks
            .iter()
            .map(|(id, ok, detail)| {
                json!({ "id": id, "ok": ok, "detail": detail })
            })
            .collect();
        print_json(
            &json!({
                "ok": all_ok,
                "appDataDir": data_dir.to_string_lossy(),
                "resourceDir": resource_dir
                    .map(|dir| dir.to_string_lossy().into_owned()),
                "checks": checks_json,
            }),
            args,
        );
    } else {
        for (id, ok, detail) in &checks {
            println!("{} {id}: {detail}", if *ok { "✓" } else { "✗" });
        }
        println!("{}", if all_ok { "validate: OK" } else { "validate: FAILED" });
    }
    if all_ok {
        EXIT_OK
    } else {
        EXIT_ERROR
    }
}

fn report_simple_error(message: &str, args: &Args) -> i32 {
    if args.flag("json") {
        println!("{}", json!({ "ok": false, "error": message }));
    } else {
        eprintln!("{message}");
    }
    EXIT_ERROR
}

fn print_json(value: &Value, _args: &Args) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunOutcome {
    Finished,
    Error,
    Cancelled,
    StartupFailed,
    Timeout,
}

impl RunOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::StartupFailed => "startup-failed",
            Self::Timeout => "timeout",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Finished => EXIT_OK,
            Self::Error | Self::StartupFailed => EXIT_ERROR,
            Self::Cancelled => EXIT_CANCELLED,
            Self::Timeout => EXIT_TIMEOUT,
        }
    }
}

/// Map the runner's final state to the CLI outcome.
///
/// `watchdog_fired` marks a stop that the watchdog itself initiated (audit
/// round 2, P1-1N): the engine honouring the cancel request lands in `Idle`,
/// which is a *timeout* semantically — reporting it as `cancelled` makes the
/// orchestrator skip managed MuMu cleanup and report the run as cancelled
/// instead of timed out. `Finished` stays `Finished`: a quota that genuinely
/// completed before the cancel took effect is a completion, not a timeout.
fn final_outcome(final_state: &RunnerState, watchdog_fired: bool) -> RunOutcome {
    match final_state {
        RunnerState::Finished => RunOutcome::Finished,
        RunnerState::Error { .. } => RunOutcome::Error,
        RunnerState::Idle => {
            if watchdog_fired {
                RunOutcome::Timeout
            } else {
                RunOutcome::Cancelled
            }
        }
        RunnerState::Starting | RunnerState::Running => RunOutcome::Error,
    }
}

struct RunOverrides {
    repeat_mission: bool,
    max_mission_runs: Option<u32>,
    ap_recovery_items: Vec<ApRecoveryItem>,
    ap_recovery_limits: ApRecoveryLimits,
}

impl Default for RunOverrides {
    fn default() -> Self {
        Self {
            repeat_mission: false,
            max_mission_runs: None,
            ap_recovery_items: Vec::new(),
            ap_recovery_limits: ApRecoveryLimits::default(),
        }
    }
}

const AP_RECOVERY_ORDER: [ApRecoveryItem; 5] = [
    ApRecoveryItem::Gold,
    ApRecoveryItem::Silver,
    ApRecoveryItem::Bronze,
    ApRecoveryItem::Copper,
    ApRecoveryItem::Rainbow,
];

fn ap_item_from_str(value: &str) -> Option<ApRecoveryItem> {
    match value.trim().to_ascii_lowercase().as_str() {
        "gold" => Some(ApRecoveryItem::Gold),
        "silver" => Some(ApRecoveryItem::Silver),
        "bronze" => Some(ApRecoveryItem::Bronze),
        "copper" => Some(ApRecoveryItem::Copper),
        "rainbow" => Some(ApRecoveryItem::Rainbow),
        _ => None,
    }
}

fn ap_item_key(item: ApRecoveryItem) -> &'static str {
    match item {
        ApRecoveryItem::Gold => "gold",
        ApRecoveryItem::Silver => "silver",
        ApRecoveryItem::Bronze => "bronze",
        ApRecoveryItem::Copper => "copper",
        ApRecoveryItem::Rainbow => "rainbow",
    }
}

/// Port of the frontend `buildProjectRunConfig` (projectRunConfig.ts).
/// Building the payload as JSON and deserializing it through serde keeps the
/// `#[serde(default)]` semantics identical to the GUI's IPC path.
fn build_project_run_config(project: &Project, overrides: &RunOverrides) -> Result<RunConfig, String> {
    let slots = &project.slots;
    let support_slot_index = slots
        .iter()
        .position(|slot| slot.kind == "support")
        .map(|index| index as u32);
    let support_slot = support_slot_index.and_then(|index| slots.get(index as usize));

    let servant_selections: Vec<Value> = slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            if slot.kind == "servant" && slot.servant_id.is_some() {
                Some(json!({
                    "memberId": slot.id,
                    "slotIndex": index,
                    "servantId": slot.servant_id,
                }))
            } else {
                None
            }
        })
        .collect();

    let support_ce_ids: Vec<u32> = match support_slot {
        Some(slot) if !slot.craft_essence_ids.is_empty() => {
            slot.craft_essence_ids.iter().copied().take(10).collect()
        }
        Some(slot) if slot.craft_essence_id.is_some() => vec![slot.craft_essence_id.unwrap()],
        _ => Vec::new(),
    };

    let project_limits = |item: ApRecoveryItem| -> Value {
        match project.ap_recovery_limits.get(item) {
            Some(value) if value > 0 => json!(value.min(u32::MAX)),
            _ => Value::Null,
        }
    };

    let stop_on_five_star = project
        .recognition_settings
        .as_ref()
        .and_then(|settings| settings.stop_on_five_star_ce_drop)
        == Some(true);
    let five_star_target = project
        .recognition_settings
        .as_ref()
        .and_then(|settings| settings.five_star_ce_drop_target_count)
        .filter(|value| *value > 0)
        .unwrap_or(1);

    let payload = json!({
        "projectId": project.id,
        "mysticCodeId": project.mystic_code_id,
        "partyOrder": Value::Null,
        "supportClassFilter": Value::Null,
        "supportServantName": Value::Null,
        "supportServantId": project.support_servant_id,
        "supportServantVariantKey": project.support_servant_variant_key,
        "supportSlotIndex": support_slot_index,
        "supportMemberId": support_slot.map(|slot| slot.id.clone()),
        "supportCraftEssenceId": support_slot.and_then(|slot| slot.craft_essence_id),
        "supportCraftEssenceIds": support_ce_ids,
        "supportCraftEssenceMlbRequired": support_slot
            .map(|slot| slot.craft_essence_mlb_required)
            .unwrap_or(true),
        "supportGrandMode": project.support_grand_mode,
        "supportGrandCraftEssenceIds": project.support_grand_craft_essence_ids,
        "supportGrandCraftEssenceIdLists": project.support_grand_craft_essence_id_lists,
        "supportGrandCraftEssenceMlbRequired": project.support_grand_craft_essence_mlb_required,
        "supportGrandBondCeMode": project.support_grand_bond_ce_mode,
        "grandServants": project.grand_servants,
        "grandClass": project.grand_class,
        "grandCardStrategy": project.grand_card_strategy,
        "supportServantLevelMin": project.support_servant_level_min,
        "supportNoblePhantasmLevelMin": project.support_noble_phantasm_level_min,
        "supportStarMapScoreMin": project.support_star_map_score_min,
        "supportGrandStarMapScoreMin": project.support_grand_star_map_score_min,
        "supportSkillLevelMins": project.support_skill_level_mins,
        "supportAppendSkillLevelMins": project.support_append_skill_level_mins,
        "preferHigherCriticalChance": project.prefer_higher_critical_chance,
        "servantSelections": servant_selections,
        "maxSupportScrolls": 3,
        "repeatMission": overrides.repeat_mission,
        "maxMissionRuns": overrides.max_mission_runs,
        "apRecoveryItems": overrides.ap_recovery_items,
        "apRecoveryLimits": json!({
            "rainbow": project_limits(ApRecoveryItem::Rainbow),
            "gold": project_limits(ApRecoveryItem::Gold),
            "silver": project_limits(ApRecoveryItem::Silver),
            "bronze": project_limits(ApRecoveryItem::Bronze),
            "copper": project_limits(ApRecoveryItem::Copper),
        }),
        "stopOnFiveStarCeDrop": stop_on_five_star,
        "fiveStarCeDropTargetCount": if stop_on_five_star { five_star_target } else { 1 },
    });
    serde_json::from_value(payload).map_err(|error| format!("构造 RunConfig 失败: {error}"))
}

fn project_default_overrides(project: &Project) -> RunOverrides {
    let (repeat_mission, max_mission_runs) = match project.repeat_mode {
        Some(crate::models::ProjectRepeatMode::Infinite) => (true, None),
        Some(crate::models::ProjectRepeatMode::Count) => (false, project.repeat_count),
        Some(crate::models::ProjectRepeatMode::Single) | None => (project.repeat_mission, None),
    };
    RunOverrides {
        repeat_mission,
        max_mission_runs,
        ap_recovery_items: AP_RECOVERY_ORDER
            .into_iter()
            .filter(|item| project.ap_recovery_items.contains(item))
            .collect(),
        ap_recovery_limits: project.ap_recovery_limits,
    }
}

fn parse_run_args(args: &Args, project: &Project) -> Result<RunOverrides, String> {
    let mut overrides = project_default_overrides(project);
    if let Some(repeat) = args.value("repeat") {
        if repeat == "infinite" {
            overrides.repeat_mission = true;
            overrides.max_mission_runs = None;
        } else if let Some(count) = repeat.strip_prefix("count:") {
            let count: u32 = count
                .parse()
                .map_err(|_| format!("--repeat count 的次数无效:「{count}」"))?;
            if count == 0 {
                return Err("--repeat count 必须 >= 1".into());
            }
            overrides.repeat_mission = false;
            overrides.max_mission_runs = Some(count);
        } else {
            return Err(format!(
                "--repeat 只接受 infinite 或 count:<N>，得到「{repeat}」"
            ));
        }
    }
    if let Some(spec) = args.value("ap-items") {
        let mut items = Vec::new();
        let mut limits = ApRecoveryLimits::default();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (name, limit) = entry.split_once(':').unwrap_or((entry, "*"));
            let item = ap_item_from_str(name)
                .ok_or_else(|| format!("未知体力恢复物品「{name}」"))?;
            let limit = if limit == "*" {
                None
            } else {
                let value: u32 = limit
                    .parse()
                    .map_err(|_| format!("「{name}」的限额无效:「{limit}」"))?;
                (value > 0).then_some(value)
            };
            match item {
                ApRecoveryItem::Rainbow => limits.rainbow = limit,
                ApRecoveryItem::Gold => limits.gold = limit,
                ApRecoveryItem::Silver => limits.silver = limit,
                ApRecoveryItem::Bronze => limits.bronze = limit,
                ApRecoveryItem::Copper => limits.copper = limit,
            }
            items.push(item);
        }
        // Keep the canonical priority order the runner expects.
        items = AP_RECOVERY_ORDER
            .into_iter()
            .filter(|candidate| items.contains(candidate))
            .collect();
        overrides.ap_recovery_items = items;
        overrides.ap_recovery_limits = limits;
    }
    Ok(overrides)
}

struct CliRunFiles {
    root: PathBuf,
}

impl CliRunFiles {
    fn new(data_dir: &std::path::Path) -> Self {
        Self {
            root: data_dir.join("cli"),
        }
    }

    fn current(&self) -> PathBuf {
        self.root.join("current.json")
    }

    fn cancel_request(&self) -> PathBuf {
        self.root.join("cancel.request")
    }

    fn last_run(&self) -> PathBuf {
        self.root.join("last-run.json")
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }
}

struct CurrentRunGuard {
    path: PathBuf,
}

impl Drop for CurrentRunGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Best-effort liveness probe for the PID recorded in `current.json`.
///
/// Windows uses `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` +
/// `GetExitCodeProcess`; other platforms fall back to `ps -p`. A PID we
/// cannot open is treated as dead — falsely flagging a stale record only
/// costs a warning, while missing a dead one blocks every future run
/// (audit P0-1).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let probed = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            probed != 0 && exit_code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

/// Read `current.json` and decide whether it represents a live run.
/// `None` means the file is missing or unreadable/unparseable.
fn read_current_run(files: &CliRunFiles) -> Option<(Value, bool)> {
    let record = read_json(&files.current())?;
    let pid = record["pid"].as_u64().unwrap_or(0) as u32;
    Some((record, pid_alive(pid)))
}

/// Remove a stale run record (and any pending cancel request) after the
/// recorded PID turned out to be dead.
fn clear_stale_run(files: &CliRunFiles) {
    let _ = std::fs::remove_file(files.current());
    let _ = std::fs::remove_file(files.cancel_request());
}

/// Windows: confine this process to a kill-on-close Job Object so that any
/// exit path — including the watchdog's `std::process::exit`, which does not
/// run destructors — tears down spawned children such as the mash-cv python
/// sidecar instead of leaking them (audit P1-3). The job handle is
/// deliberately kept open for the process lifetime; the OS closes it on exit,
/// which triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
#[cfg(target_os = "windows")]
fn confine_to_kill_on_close_job() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured != 0 && AssignProcessToJobObject(job, GetCurrentProcess()) != 0 {
            // Intentionally leak the handle: closing it here would trigger
            // the kill-on-close semantics immediately.
            return;
        }
        CloseHandle(job);
    }
}

#[cfg(not(target_os = "windows"))]
fn confine_to_kill_on_close_job() {}

fn emit_stdout(line: &str, enabled: bool) {
    if enabled {
        println!("{line}");
    }
}

fn command_run(app: &tauri::App<tauri::Wry>, args: &Args) -> i32 {
    let handle = app.handle();
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = SystemTime::now();
    let events_enabled = args.value("events") != Some("none");

    let finish = |outcome: RunOutcome,
                  message: Option<String>,
                  project: Option<&Project>,
                  device: Option<String>,
                  server: Server,
                  overrides: &RunOverrides,
                  progress: Option<&Value>,
                  event_counts: (u64, u64, u64)|
     -> (Value, i32) {
        let completed_runs = progress
            .and_then(|value| value["completedRuns"].as_u64())
            .unwrap_or(0);
        let ap_usage = progress
            .map(|value| value["apRecoveryUsage"].clone())
            .unwrap_or_else(|| json!({
                "rainbow": 0, "gold": 0, "silver": 0, "bronze": 0, "copper": 0
            }));
        let result = json!({
            "schemaVersion": 1,
            "runId": run_id,
            "startedAt": iso8601(started_at),
            "endedAt": iso8601(SystemTime::now()),
            "outcome": outcome.as_str(),
            "message": message,
            "project": project.map(|p| json!({
                "id": p.id,
                "name": p.name,
                "advancedMode": p.advanced_mode,
            })),
            "server": server.to_string(),
            "device": device,
            "repeat": {
                "mode": if overrides.repeat_mission { "infinite" }
                        else if overrides.max_mission_runs.is_some() { "count" }
                        else { "single" },
                "count": overrides.max_mission_runs,
            },
            "completedRuns": completed_runs,
            "apRecoveryUsage": ap_usage,
            "eventCounts": {
                "status": event_counts.0,
                "warn": event_counts.1,
                "error": event_counts.2,
            },
            "exitCode": outcome.exit_code(),
        });
        (result, outcome.exit_code())
    };

    // ---- resolve project & options --------------------------------------
    let project_id = match args.require_value("project") {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };
    let projects = match projects_commands::read_projects(handle) {
        Ok(projects) => projects,
        Err(message) => {
            eprintln!("读取项目失败: {message}");
            return EXIT_ERROR;
        }
    };
    let Some(project) = projects.iter().find(|p| p.id == project_id) else {
        eprintln!("找不到项目「{project_id}」（用 mash-cli projects 列出可用项目）");
        return EXIT_ERROR;
    };
    let overrides = match parse_run_args(args, project) {
        Ok(overrides) => overrides,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };

    // Apply per-run in-memory overrides (never written back to disk).
    let device_override = args.value("device").map(str::to_string);
    if let Some(serial) = &device_override {
        app.state::<Mutex<settings::AdbDeviceSettings>>()
            .lock()
            .unwrap()
            .selected_adb_serial = Some(serial.clone());
    }
    let server = match args.value("server") {
        Some(value) => match value.to_ascii_uppercase().parse::<Server>() {
            Ok(server) => {
                *app.state::<Mutex<Server>>().lock().unwrap() = server;
                server
            }
            Err(message) => {
                eprintln!("{message}");
                return EXIT_USAGE;
            }
        },
        None => *app.state::<Mutex<Server>>().lock().unwrap(),
    };

    let config = match build_project_run_config(project, &overrides) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_ERROR;
        }
    };

    // ---- run ledger ------------------------------------------------------
    let data_dir = paths::app_data_dir(handle);
    let files = CliRunFiles::new(&data_dir);
    if let Err(message) = std::fs::create_dir_all(files.runs_dir()) {
        eprintln!("创建 CLI 运行目录失败: {message}");
        return EXIT_ERROR;
    }
    if files.current().exists() {
        // Audit P0-1: a hard-killed or crashed run leaves current.json
        // behind; only refuse when the recorded PID is still alive.
        match read_current_run(&files) {
            Some((_, true)) => {
                eprintln!("已有正在运行的 mash-cli run（{}）", files.current().display());
                return EXIT_ERROR;
            }
            Some((record, false)) => {
                eprintln!(
                    "[mash-cli] 检测到陈旧 run 记录（pid {}，进程已退出），已清理后继续",
                    record["pid"].as_u64().unwrap_or(0)
                );
                clear_stale_run(&files);
            }
            None => {
                eprintln!("run 记录损坏（{}），已清理后继续", files.current().display());
                clear_stale_run(&files);
            }
        }
    }
    let device_used = device_override.clone().or_else(|| {
        app.state::<Mutex<settings::AdbDeviceSettings>>()
            .lock()
            .unwrap()
            .selected_adb_serial
            .clone()
    });
    let _current_guard = CurrentRunGuard {
        path: files.current(),
    };
    if let Err(message) = crate::storage::write_json_atomic(
        &files.current(),
        &json!({
            "pid": std::process::id(),
            "runId": run_id,
            "projectId": project.id,
            "server": server.to_string(),
            "device": device_used,
            "startedAt": iso8601(started_at),
        }),
        "CLI run 记录",
    ) {
        eprintln!("{message}");
        return EXIT_ERROR;
    }

    // ---- event pump -------------------------------------------------------
    let (event_tx, event_rx) = mpsc::channel::<Value>();
    {
        let tx = event_tx.clone();
        app.listen("automation-status", move |event| {
            if let Ok(mut value) = serde_json::from_str::<Value>(event.payload()) {
                if let Some(object) = value.as_object_mut() {
                    object.insert("type".into(), json!("status"));
                    object.insert("ts".into(), json!(iso8601(SystemTime::now())));
                }
                let _ = tx.send(value);
            }
        });
    }
    {
        let tx = event_tx.clone();
        app.listen("battle-run-progress", move |event| {
            if let Ok(mut value) = serde_json::from_str::<Value>(event.payload()) {
                if let Some(object) = value.as_object_mut() {
                    object.insert("type".into(), json!("progress"));
                    object.insert("ts".into(), json!(iso8601(SystemTime::now())));
                }
                let _ = tx.send(value);
            }
        });
    }

    let ctrlc_requested = Arc::new(AtomicBool::new(false));
    {
        let flag = ctrlc_requested.clone();
        let _ = ctrlc::set_handler(move || {
            flag.store(true, Ordering::Relaxed);
        });
    }

    emit_stdout(
        &json!({
            "type": "run-started",
            "runId": run_id,
            "ts": iso8601(started_at),
            "project": { "id": project.id, "name": project.name, "advancedMode": project.advanced_mode },
            "server": server.to_string(),
            "device": device_used,
            "repeat": {
                "mode": if overrides.repeat_mission { "infinite" }
                        else if overrides.max_mission_runs.is_some() { "count" }
                        else { "single" },
                "count": overrides.max_mission_runs,
            },
        })
        .to_string(),
        events_enabled,
    );

    // ---- start ------------------------------------------------------------
    let runner_state = app.state::<Mutex<RunnerHandle>>();
    let coordinator = app.state::<DeviceOperationCoordinator>();
    let debug_sidecar = app.state::<debug::DebugSidecar>();
    if let Err(message) = start_automation_with_config(
        handle,
        config,
        runner_state.inner(),
        coordinator.inner(),
        debug_sidecar.inner(),
    ) {
        let (result, code) = finish(
            RunOutcome::StartupFailed,
            Some(message.clone()),
            Some(project),
            device_used,
            server,
            &overrides,
            None,
            (0, 0, 0),
        );
        write_result_files(&files, &result, args);
        eprintln!("启动失败: {message}");
        emit_stdout(&result_with_type(result, "result"), events_enabled);
        return code;
    }

    // ---- watch ------------------------------------------------------------
    let mut seen_active = false;
    let mut latest_progress: Option<Value> = None;
    let mut counts = (0u64, 0u64, 0u64);
    let watchdog = args
        .value("watchdog")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds));
    let mut watchdog_hard_deadline: Option<std::time::Instant> = None;

    let terminal = loop {
        // Drain events.
        while let Ok(mut event) = event_rx.try_recv() {
            let event_type = event["type"].as_str().unwrap_or("status").to_string();
            if event_type == "progress" {
                latest_progress = Some(event.clone());
            } else {
                match event["level"].as_str().unwrap_or("info") {
                    "warn" => counts.1 += 1,
                    "error" => counts.2 += 1,
                    _ => counts.0 += 1,
                }
            }
            if let Some(object) = event.as_object_mut() {
                object.remove("attack");
                object.remove("action");
            }
            emit_stdout(&event.to_string(), events_enabled);
        }

        // Cancel requests: file first, then Ctrl+C.
        if files.cancel_request().exists() {
            let mode = std::fs::read_to_string(files.cancel_request())
                .ok()
                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
                .and_then(|value| {
                    value["mode"]
                        .as_str()
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "immediate".into());
            let _ = std::fs::remove_file(files.cancel_request());
            let handle_guard = runner_state.lock().unwrap();
            if mode == "after-current" {
                handle_guard.stop_after_current.store(true, Ordering::Relaxed);
            } else {
                handle_guard.cancel.store(true, Ordering::Relaxed);
            }
            eprintln!("[mash-cli] 已请求停止（{mode}）");
        }
        if ctrlc_requested.load(Ordering::Relaxed) {
            ctrlc_requested.store(false, Ordering::Relaxed);
            runner_state
                .lock()
                .unwrap()
                .cancel
                .store(true, Ordering::Relaxed);
            eprintln!("[mash-cli] 收到 Ctrl+C，已请求停止");
        }

        // Watchdog.
        if let (Some(elapsed_limit), None) = (watchdog, watchdog_hard_deadline) {
            if started_at.elapsed().unwrap_or_default() >= elapsed_limit {
                eprintln!("[mash-cli] watchdog 触发，请求停止并进入 120s 宽限");
                runner_state
                    .lock()
                    .unwrap()
                    .cancel
                    .store(true, Ordering::Relaxed);
                watchdog_hard_deadline = Some(std::time::Instant::now() + Duration::from_secs(120));
            }
        }
        if let Some(deadline) = watchdog_hard_deadline {
            if std::time::Instant::now() >= deadline {
                let (result, code) = finish(
                    RunOutcome::Timeout,
                    Some("watchdog 超时强制退出".into()),
                    Some(project),
                    device_used.clone(),
                    server,
                    &overrides,
                    latest_progress.as_ref(),
                    counts,
                );
                write_result_files(&files, &result, args);
                emit_stdout(&result_with_type(result, "result"), events_enabled);
                // process::exit 不运行 Drop：显式清理 run 记录，避免残留
                // 阻塞后续 run（audit P0-1 触发路径 1）。sidecar 由进程退出时
                // 的 kill-on-close Job 兜底回收（audit P1-3）。
                let _ = std::fs::remove_file(files.current());
                let _ = std::fs::remove_file(files.cancel_request());
                let _ = std::io::stdout().flush();
                std::process::exit(code);
            }
        }

        // Terminal state?
        let state = runner_state.lock().unwrap().state.lock().unwrap().clone();
        match state {
            RunnerState::Finished => break None,
            RunnerState::Error { message } => break Some(message),
            RunnerState::Idle if seen_active => break None,
            // Starting/Running (or the brief pre-worker Idle): keep waiting,
            // but remember the run was live so a later Idle means "cancelled".
            RunnerState::Idle | RunnerState::Starting | RunnerState::Running => {
                seen_active = true;
            }
        }

        std::thread::sleep(Duration::from_millis(300));
    };

    // Let the worker flush final events, then drain once more.
    std::thread::sleep(Duration::from_millis(600));
    while let Ok(mut event) = event_rx.try_recv() {
        let event_type = event["type"].as_str().unwrap_or("status").to_string();
        if event_type == "progress" {
            latest_progress = Some(event.clone());
        }
        if let Some(object) = event.as_object_mut() {
            object.remove("attack");
            object.remove("action");
        }
        emit_stdout(&event.to_string(), events_enabled);
    }

    let final_state = runner_state.lock().unwrap().state.lock().unwrap().clone();
    // 审计二轮 P1-1N：watchdog 发起的停止在语义上就是超时——引擎响应取消
    // 优雅收尾（主流形态）时终态是 Idle，必须映射为 Timeout/exit 3，否则
    // 整条编排链会把超时报成 CANCELLED（managed MuMu 清理被连带跳过）。
    // Finished 是唯一例外：取消生效前配额真实跑完，如实上报完成。
    let watchdog_fired = watchdog_hard_deadline.is_some();
    let outcome = final_outcome(&final_state, watchdog_fired);
    let message = terminal.or_else(|| {
        (watchdog_fired && matches!(outcome, RunOutcome::Timeout))
            .then(|| "watchdog 超时：已请求停止，引擎按契约优雅收尾".to_string())
    });
    let (result, code) = finish(
        outcome,
        message,
        Some(project),
        device_used,
        server,
        &overrides,
        latest_progress.as_ref(),
        counts,
    );
    write_result_files(&files, &result, args);
    emit_stdout(&result_with_type(result, "result"), events_enabled);
    code
}

fn result_with_type(result: Value, event_type: &str) -> String {
    let mut value = result;
    if let Some(object) = value.as_object_mut() {
        object.insert("type".into(), json!(event_type));
    }
    value.to_string()
}

fn write_result_files(files: &CliRunFiles, result: &Value, args: &Args) {
    if let Some(path) = args.value("result-file") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(message) = write_json_atomic(&path, result, "run 结果") {
            eprintln!("写入 result-file 失败: {message}");
        }
    }
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let history = files.runs_dir().join(format!("{stamp:013}-{}.json", result["runId"].as_str().unwrap_or("run")));
    let _ = write_json_atomic(&history, result, "run 历史");
    let _ = write_json_atomic(&files.last_run(), result, "last-run 记录");
}

// ---------------------------------------------------------------------------
// cancel / status (file-based, no app bootstrap)
// ---------------------------------------------------------------------------

fn standalone_app_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MASH_APP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // Mirror tauri's app_data_dir without booting the runtime:
    // Windows: %APPDATA%\<identifier>; macOS: ~/Library/Application Support/<identifier>.
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(MASH_IDENTIFIER);
        }
    }
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("XDG_CONFIG_HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join(MASH_IDENTIFIER)
}

fn command_cancel(args: &Args) -> i32 {
    let files = CliRunFiles::new(&standalone_app_data_dir());
    let Some(current) = read_json(&files.current()) else {
        eprintln!("没有正在运行的 mash-cli run");
        return EXIT_ERROR;
    };
    let pid = current["pid"].as_u64().unwrap_or(0) as u32;
    if pid == 0 || !pid_alive(pid) {
        // Audit P0-1: stale record after a hard kill/crash — clean up and
        // fail with a clear message instead of waiting on a dead run.
        clear_stale_run(&files);
        eprintln!("检测到陈旧 run 记录（pid {pid}，进程已退出），已清理；无正在运行的 run");
        return EXIT_ERROR;
    }
    let mode = if args.flag("after-current") {
        "after-current"
    } else {
        "immediate"
    };
    if let Err(message) = std::fs::create_dir_all(&files.root) {
        eprintln!("写入取消请求失败: {message}");
        return EXIT_ERROR;
    }
    if let Err(message) = crate::storage::write_json_atomic(
        &files.cancel_request(),
        &json!({ "mode": mode }),
        "取消请求",
    ) {
        eprintln!("写入取消请求失败: {message}");
        return EXIT_ERROR;
    }
    eprintln!(
        "已发送停止请求（{mode}）给 run {} (pid {})",
        current["runId"].as_str().unwrap_or("?"),
        current["pid"].as_u64().unwrap_or(0)
    );

    let wait_secs: u64 = args
        .value("wait")
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);
    while files.current().exists() {
        if std::time::Instant::now() >= deadline {
            eprintln!("等待超时：run 进程仍在运行（可能正处于不可中断的战斗动画中）");
            return EXIT_ERROR;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    println!("run 已退出");
    EXIT_OK
}

fn command_status(args: &Args) -> i32 {
    let files = CliRunFiles::new(&standalone_app_data_dir());
    let current = read_json(&files.current());
    // Audit P0-1: liveness-check the recorded PID instead of trusting file
    // presence; a stale record is reported as such and cleaned up.
    let mut running = false;
    let mut stale = false;
    if let Some(record) = current.as_ref() {
        let pid = record["pid"].as_u64().unwrap_or(0) as u32;
        if pid != 0 && pid_alive(pid) {
            running = true;
        } else {
            stale = true;
            clear_stale_run(&files);
        }
    }
    let last_run = read_json(&files.last_run());
    let current_for_payload = current.clone().filter(|_| running);
    let payload = json!({
        "running": running,
        "stale": stale,
        "current": current_for_payload,
        "lastRun": last_run,
    });
    if args.flag("json") {
        print_json(&payload, args);
    } else if let Some(current) = current.as_ref().filter(|_| running) {
        println!(
            "running: runId={} project={} pid={} startedAt={}",
            current["runId"].as_str().unwrap_or("?"),
            current["projectId"].as_str().unwrap_or("?"),
            current["pid"].as_u64().unwrap_or(0),
            current["startedAt"].as_str().unwrap_or("?")
        );
    } else {
        if stale {
            println!("idle: 检测并清理了陈旧 run 记录");
        }
        if let Some(last_run) = payload.get("lastRun").filter(|value| !value.is_null()) {
            println!(
                "idle: lastRun outcome={} completedRuns={} endedAt={}",
                last_run["outcome"].as_str().unwrap_or("?"),
                last_run["completedRuns"].as_u64().unwrap_or(0),
                last_run["endedAt"].as_str().unwrap_or("?")
            );
        } else {
            println!("idle: 尚无 run 记录");
        }
    }
    EXIT_OK
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
}

// ---------------------------------------------------------------------------
// Time helpers (no chrono dependency)
// ---------------------------------------------------------------------------

fn iso8601(time: SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem / 60) % 60, rem % 60);
    // days-from-civil inverse (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_timestamps() {
        assert_eq!(
            iso8601(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
        // 2026-10-24 00:00:00 UTC = 1792800000
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_792_800_000);
        assert_eq!(iso8601(time), "2026-10-24T00:00:00Z");
        // Leap-year day: 2024-02-29 12:30:45 UTC = 1709209845
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_709_209_845);
        assert_eq!(iso8601(time), "2024-02-29T12:30:45Z");
    }

    #[test]
    fn args_parse_flags_and_values() {
        let tokens: Vec<String> = [
            "--project", "p1", "--server", "cn", "--json", "--repeat", "count:3",
        ]
        .iter()
        .map(|token| token.to_string())
        .collect();
        let args = Args::parse(&tokens).unwrap();
        assert_eq!(args.value("project"), Some("p1"));
        assert_eq!(args.value("server"), Some("cn"));
        assert_eq!(args.value("repeat"), Some("count:3"));
        assert!(args.flag("json"));
    }

    #[test]
    fn own_pid_is_reported_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn watchdog_fired_idle_maps_timeout_not_cancelled() {
        // 审计二轮 P1-1N：watchdog 发起的停止必须报超时，否则编排链
        // 把主流超时形态误报为取消并跳过 managed MuMu 清理。
        assert_eq!(
            final_outcome(&RunnerState::Idle, true),
            RunOutcome::Timeout
        );
        // 配额在取消生效前真实完成 → 仍是完成。
        assert_eq!(
            final_outcome(&RunnerState::Finished, true),
            RunOutcome::Finished
        );
        // 外部取消（Ctrl+C / mash-cli cancel）与失败不受 watchdog 标志影响。
        assert_eq!(
            final_outcome(&RunnerState::Idle, false),
            RunOutcome::Cancelled
        );
        assert_eq!(final_outcome(&RunnerState::Error { message: String::new() }, false), RunOutcome::Error);
    }

    #[test]
    fn missing_current_run_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        let files = CliRunFiles::new(dir.path());
        assert!(read_current_run(&files).is_none());
    }

    #[test]
    fn stale_current_run_detected_by_dead_pid() {
        let dir = tempfile::tempdir().unwrap();
        let files = CliRunFiles::new(dir.path());
        std::fs::create_dir_all(&files.root).unwrap();
        // PID 4_000_000 极不可能存活（Windows PID 分配范围远小于此），
        // 用于模拟硬杀/崩溃后残留的陈旧记录（audit P0-1）。
        crate::storage::write_json_atomic(
            &files.current(),
            &json!({ "pid": 4_000_000_u64, "runId": "stale" }),
            "测试 run 记录",
        )
        .unwrap();
        let (record, live) = read_current_run(&files).expect("记录应可读");
        assert!(!live, "死 PID 必须判定为陈旧");
        assert_eq!(record["runId"].as_str(), Some("stale"));
        clear_stale_run(&files);
        assert!(!files.current().exists());
        assert!(!files.cancel_request().exists());
    }

    #[test]
    fn args_reject_value_lookalike() {
        let tokens: Vec<String> = ["--project", "--json"]
            .iter()
            .map(|token| token.to_string())
            .collect();
        assert!(Args::parse(&tokens).is_err());
    }

    fn sample_project() -> Project {
        serde_json::from_value(json!({
            "id": "p1",
            "name": "周回",
            "repeatMode": "count",
            "repeatCount": 3,
            "slots": [
                { "id": "s0", "type": "servant", "servantId": 100 },
                { "id": "s1", "type": "servant", "servantId": 200 },
                { "id": "sup", "type": "support", "craftEssenceId": 55 },
            ],
            "apRecoveryItems": ["silver", "gold"],
            "supportServantId": 300,
        }))
        .unwrap()
    }

    #[test]
    fn project_defaults_follow_persisted_repeat_mode() {
        let project = sample_project();
        let overrides = project_default_overrides(&project);
        assert!(!overrides.repeat_mission);
        assert_eq!(overrides.max_mission_runs, Some(3));
        // Canonical order re-applies regardless of persisted order.
        assert_eq!(
            overrides
                .ap_recovery_items
                .iter()
                .map(|item| ap_item_key(*item))
                .collect::<Vec<_>>(),
            vec!["gold", "silver"]
        );
    }

    #[test]
    fn run_config_payload_mirrors_frontend_mapping() {
        let project = sample_project();
        let overrides = project_default_overrides(&project);
        let config = build_project_run_config(&project, &overrides).unwrap();
        assert_eq!(config.project_id, "p1");
        assert_eq!(config.support_servant_id, Some(300));
        assert_eq!(config.support_slot_index, Some(2));
        assert_eq!(
            config.support_craft_essence_ids,
            vec![55],
            "legacy single CE id migrates into the allow-list"
        );
        assert_eq!(config.max_support_scrolls, 3);
        assert_eq!(config.max_mission_runs, Some(3));
        assert_eq!(config.servant_selections.len(), 2);
        assert_eq!(config.servant_selections[0].slot_index, 0);
        assert_eq!(config.servant_selections[1].slot_index, 1);
    }

    #[test]
    fn ap_items_parse_with_limits() {
        let project = sample_project();
        let tokens: Vec<String> = [
            "--project", "p1", "--ap-items", "gold:2,rainbow:*",
        ]
        .iter()
        .map(|token| token.to_string())
        .collect();
        let args = Args::parse(&tokens).unwrap();
        let overrides = parse_run_args(&args, &project).unwrap();
        assert_eq!(
            overrides
                .ap_recovery_items
                .iter()
                .map(|item| ap_item_key(*item))
                .collect::<Vec<_>>(),
            vec!["gold", "rainbow"]
        );
        assert_eq!(overrides.ap_recovery_limits.gold, Some(2));
        assert_eq!(overrides.ap_recovery_limits.rainbow, None);
    }

    #[test]
    fn repeat_arg_overrides_project_default() {
        let project = sample_project();
        let tokens: Vec<String> = ["--project", "p1", "--repeat", "infinite"]
            .iter()
            .map(|token| token.to_string())
            .collect();
        let args = Args::parse(&tokens).unwrap();
        let overrides = parse_run_args(&args, &project).unwrap();
        assert!(overrides.repeat_mission);
        assert_eq!(overrides.max_mission_runs, None);
    }
}
